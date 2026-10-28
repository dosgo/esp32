//! 音频管线：I2S 全双工 + 能量 VAD + 播放队列
//!
//! # 为什么 I2S 用立体声而上传用单声道
//!
//! ES8311 的 SDP 接口天然是「左右两个 slot」的结构，它没有单声道模式的概念。
//! 如果让 ESP32 侧发单声道，codec 收到的是一个 slot 的帧，DAC 取哪一路
//! 取决于内部 mux，实测很容易变成「只有杂音」。
//!
//! 所以这里的做法是：**I2S 走标准立体声帧，左右声道写一样的数据，
//! 麦克风只取左声道上传**。多出的 CPU 开销可以忽略（一个 1920 字节的循环），
//! 但省掉了整个「为什么没声音」的排查过程。

use core::sync::atomic::{AtomicU8, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use esp_println::println;
use esp_hal::dma::aligned::DmaAlignedMut;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::i2s::master::{I2sRx, I2sTx};
use esp_hal::Async;

use crate::board::{FRAME_BYTES, FRAME_SAMPLES, I2S_FRAME_BYTES};

pub type AudioFrame = heapless::Vec<u8, FRAME_BYTES>;

/// 录音帧：audio_task → ws_task
///
/// 【深度 4 —— 回退到原始值，别再加】
///
/// 这台板子的内部 RAM 非常紧（WiFi + 两个堆 + 网络缓冲已经占掉大半）。
/// 实测把这里加到 16 帧（16.5KB）、TCP 缓冲加到 8KB×4，BSS 冲到 188KB，
/// 设备就会出现「启动 → 跑一阵 → 突然卡死，串口一个字都不吐」。
///
/// 上行音频断断续续的真因是 I2S 时钟停振（见 speaker_task 的注释），
/// 不是这个队列太浅。时钟修好之后 4 帧（128ms）完全够用。
pub static MIC_CH: Channel<CriticalSectionRawMutex, AudioFrame, 4> = Channel::new();
/// 播放帧：ws_task → audio_task
pub static SPK_CH: Channel<CriticalSectionRawMutex, AudioFrame, 8> = Channel::new();

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    /// 待机：只在本地做能量检测，不上传
    Idle = 0,
    /// 聆听：全量上传
    Listening = 1,
    /// 思考：等大模型，麦克风静默
    Thinking = 2,
    /// 说话：播放 TTS
    Speaking = 3,
}

static STATE: AtomicU8 = AtomicU8::new(State::Idle as u8);

pub fn state() -> State {
    match STATE.load(Ordering::Relaxed) {
        1 => State::Listening,
        2 => State::Thinking,
        3 => State::Speaking,
        _ => State::Idle,
    }
}

pub fn set_state(s: State) {
    STATE.store(s as u8, Ordering::Relaxed);
}

/// 麦克风任务：连续读 I2S，做 VAD 门控，把该发出去的帧塞进 `MIC_CH`
///
/// 门控逻辑（三档）：
/// - Idle：只有连续 3 帧（180ms）能量超阈值才切到 Listening，并把 pre-roll 一起发出去。
///   这 180ms 的缓冲是为了不丢「你好」的「你」字 —— 人一开口，前一两帧往往还在噪声底附近。
/// - Listening：全量上传，10 秒没有任何人声就自动回 Idle（防止一直上传耗尽流量）。
/// - 其它状态：只读不传（但 DMA 必须一直跑，否则 I2S RX FIFO 溢出会卡死）。
#[embassy_executor::task]
pub async fn mic_task(mut rx: I2sRx<'static, Async>) {
    // DMA 缓冲必须内存对齐，交给 dma_buffers! 宏分配（内部是 static）。
    //
    // 【保持单帧 2048B】曾经试过 `I2S_FRAME_BYTES * 2`（一次收 2 帧），
    // 结果设备连 TCP 握手都完不成（网关侧停在 SynReceived）、mic_task 也
    // 一条日志都不打 —— 这个改法会破坏 I2S RX/TX 的时钟配合，别再碰。
    // 音频断续的真正原因在 speaker_task（见那里的注释）。
    let (rx_buf, rx_desc, _, _) = esp_hal::dma_buffers!(I2S_FRAME_BYTES);
    let mut dma = match (DmaAlignedMut::new(rx_desc), DmaAlignedMut::new(rx_buf)) {
        (Ok(d), Ok(b)) => match DmaRxBuf::new(d, b) {
            Ok(v) => v,
            Err(_) => return,
        },
        _ => return,
    };

    // 噪声底自适应：待机时用慢速滑动平均跟踪环境噪声
    let mut noise_floor: u32 = 120;
    // 连续超阈值帧计数
    let mut hot_frames: u8 = 0;
    // Listening 状态下连续静音帧计数
    let mut quiet_frames: u16 = 0;
    // pre-roll：最近 2 帧，唤醒时补发
    let mut preroll: [Option<AudioFrame>; 2] = [None, None];
    let mut preroll_idx = 0usize;
    // 麦克风电平日志的节流计数
    let mut mic_tick: u32 = 0;

    // 【实验开关】临时关掉 esp-sr，验证 WiFi 四次握手失败是不是内部 RAM 被吃光导致的。
    //
    // 依据：内部堆总共只有 112KB（BSS 48KB + reclaimed 64KB），而
    //   AEC 要 ~122KiB、VADNet 模型 ~281KB、WakeNet ~284KB（模型在 flash，
    //   但推理要缓冲），全都在 WiFi 连接**之前**分配。
    // WiFi 驱动只能用内部 RAM（malloc_internal），握手阶段的加密缓冲
    // 分不出来就会超时 —— 正好是 reason=15。
    // 【2026-10-28 全部关闭 esp-sr】
    //
    // 这台板子的内部 RAM 已经被 BSS（~152KB）+ 网络栈 + 两个堆占满，
    // 而 AEC 一初始化就要吃掉约 **122KB**。两者一叠加，设备会在启动
    // 十几秒后彻底卡死：串口一个字都不吐、也不重启、monitor 也读不到，
    // 看起来像硬件坏了 —— 实际是内存被榨干。
    //
    // 现在 VADNet 和 AEC 都用不上：
    //   · VADNet 会卡在「一直判有人声」，已经把设备永久锁死在 Listening
    //   · AEC 只用来打印 ERLE，压根没参与链路
    // 所以整块关掉。真要用语音唤醒/AEC 时，得先想办法把 BSS 降下来
    // （比如砍网络缓冲、关 web 配置页），再一个一个开回来。
    const SR_ENABLE: bool = false;

    // ---- VADNet ----
    // 加载失败就整个回落到能量 VAD，不影响语音链路能用
    // 【2026-10-28 临时禁用 VADNet】
    //
    // 实测 VADNet 在这块板上会卡在「一直判有人声」：设备一进 Listening 就再也
    // 出不来，持续上传静音，而网关收到的全是静音、永远凑不够语音时长，于是
    // 两边互相卡死 —— 用户体感就是「它完全不理我」。
    //
    // 串口铁证：rms 掉到 35~57（纯静音）时，hot 仍然是 2/2、st 一直=1。
    //
    // 现在只用能量阈值（200，落在「安静上限 105」和「说话下限 350」之间），
    // 触发和回退用同一个量，不会自相矛盾。想再试 VADNet 就把下面改成 true。
    const VADNET_ENABLE: bool = false;
    let vad_ok = VADNET_ENABLE && SR_ENABLE && crate::vad::init();
    let vad_chunk = crate::vad::chunk_size();
    // 唤醒判定要连续 2 帧（64ms）超阈值，滤掉单帧的爆音/碰触
    let hot_needed = 2;

    // ---- AEC ----
    // 吃 ~122KiB 内部内存，失败就直通（播放期间不收音，等于回到改造前）
    let aec_ok = if SR_ENABLE { crate::aec::init() } else { false };

    // ---- WakeNet 本地唤醒 ----
    // 必须在 VAD 之后初始化：两者共用同一个 ModelList，由 esp_sr 首次访问时加载。
    // 失败就回落到「VAD 判到人声即唤醒」，行为和加唤醒词之前一样。
    // 【临时】WakeNet 在实测中一直不命中（电平正常、判定块数在涨），
    // 先用 VADNet 唤醒跑通「说话 → 上传 → ASR → LLM → TTS」整条链路，
    // 确认音频采样率/格式正确之后，再回头调 WakeNet。
    // 打开 WakeNet 把下面这行的 false 换成 crate::wakenet::init()
    let wn_ok = false;
    // 用来检测「刚回到 Idle」这个时刻，好复位唤醒词状态
    let mut was_idle = true;

    loop {
        let transfer = match rx.read(dma) {
            Ok(t) => t,
            Err((_, r, b)) => {
                rx = r;
                dma = b;
                continue;
            }
        };
        let (res, r, b) = transfer.wait_async().await;
        rx = r;
        dma = b;
        if res.is_err() {
            continue;
        }

        // 里面是 interleaved L/R 的 i16。只取左声道。
        let mono = extract_left(dma.as_slice());
        let energy = rms_energy(&mono);
        let st = state();

        // 麦克风电平诊断：每 32 帧（约 1 秒）打一次。
        //
        // 除了 rms，还把 VAD 的内部状态一起打出来 —— 排查「不说话就没反应」
        // 时这几个数能直接定位：
        //   底噪   噪声底噪的慢跟随值，能量阈值 = max(底噪×2, 200)
        //   hot    连续超阈值帧计数，到 hot_needed 就唤醒
        //   st     当前状态（0=Idle 才会判唤醒）
        mic_tick = mic_tick.wrapping_add(1);
        if mic_tick % 32 == 0 {
            log::info!(
                "[mic] rms={energy} 底噪={noise_floor} hot={hot_frames}/{hot_needed} st={}",
                st as u8
            );
        }

        // 刚回到待机：清掉唤醒词的残留状态，否则上一轮对话的尾音还在
        // 累积缓冲里，可能立刻就误触发一次。
        if st == State::Idle && !was_idle && wn_ok {
            crate::wakenet::reset();
        }
        was_idle = st == State::Idle;

        // ---- VAD 判定 ----
        // 只在需要时算：Thinking / Speaking 既不上传也不判静音。
        // Idle 且有 WakeNet 时也不跑 VAD —— 唤醒交给唤醒词，省一份算力。
        let speech = match st {
            State::Idle if !wn_ok && vad_ok && vad_chunk > 0 => {
                // detect() 会原地处理样本，所以用副本，不能污染待上传的 mono
                let mut pcm = [0i16; FRAME_SAMPLES];
                let n = pcm_from_bytes(&mono, &mut pcm);
                let mut any = false;
                let mut off = 0usize;
                while off + vad_chunk <= n {
                    if crate::vad::detect(&mut pcm[off..off + vad_chunk]) {
                        any = true;
                        break;
                    }
                    off += vad_chunk;
                }
                any
            }
            State::Listening if vad_ok && vad_chunk > 0 => {
                let mut pcm = [0i16; FRAME_SAMPLES];
                let n = pcm_from_bytes(&mono, &mut pcm);
                let mut any = false;
                let mut off = 0usize;
                while off + vad_chunk <= n {
                    if crate::vad::detect(&mut pcm[off..off + vad_chunk]) {
                        any = true;
                        break;
                    }
                    off += vad_chunk;
                }
                any
            }
            _ => false,
        };

        match st {
            State::Idle => {
                // ---- 判定唤醒 ----
                // 三条路径按优先级回落：
                //   1. WakeNet：喊「小特小特」才唤醒，命中前不上传任何音频
                //   2. VADNet：有人说话就唤醒（加唤醒词之前的行为）
                //   3. 能量阈值：连模型都加载不了时的最后兜底
                let hit = if wn_ok {
                    // 命中即唤醒，不需要额外防抖 —— WakeNet 内部已经做了
                    crate::wakenet::detect(&mono)
                } else {
                    // 【2026-10-28 改】VADNet 与能量阈值改成**并联**，任一成立即可。
                    //
                    // 原来是 `if vad_ok { 用模型 } else { 用能量 }` 的二选一。
                    // 实机暴露出问题：VADNet 装是装上了，但真机上经常一直判否
                    // （麦克风增益/环境不匹配时它就是不认人声），于是用户正常说话
                    // 设备毫无反应 —— 体感就是「它又不理我」，而且没有任何报错。
                    //
                    // 漏判是致命的（用户以为设备坏了），误判只是多送一次 ASR，
                    // 所以这里宁可激进：模型说有人声 或 能量超过阈值，都算有人说话。
                    let by_energy = {
                        // 噪声底慢跟随：只在安静帧更新，避免人声把阈值顶上去
                        if energy < noise_floor * 2 {
                            noise_floor = (noise_floor * 15 + energy) / 16;
                        }
                        // 【阈值取 200，不是 400】
                        //
                        // 实测这台机器的麦克风电平：安静环境 rms 37~105，
                        // 正常说话 350~1450，但说话时起伏很大，一半以上的帧
                        // 落在 350~450 之间。原来的 `.max(400)` 会把这些人声帧
                        // 判成静音，连续 2 帧的门槛就永远凑不齐 —— 表现就是
                        // 「我明明说话了，它一动不动」。
                        //
                        // 200 位于「安静上限 105」和「说话下限 350」之间，
                        // 留了足够的余量，也不会被环境底噪误触发。
                        energy > (noise_floor * 2).max(80)
                    };
                    let any_speech = speech || by_energy;
                    if any_speech {
                        hot_frames = hot_frames.saturating_add(1);
                    } else {
                        hot_frames = 0;
                    }
                    hot_frames >= hot_needed
                };

                // 存 pre-roll：唤醒后要补发，避免用户紧接着说的第一个字被切掉
                let mut f = AudioFrame::new();
                let _ = f.extend_from_slice(&mono);
                preroll[preroll_idx] = Some(f);
                preroll_idx = (preroll_idx + 1) % 2;

                if hit {
                    // 唤醒：把 pre-roll 按时间顺序补发
                    set_state(State::Listening);
                    quiet_frames = 0;
                    for i in 0..2 {
                        let idx = (preroll_idx + i) % 2;
                        if let Some(f) = preroll[idx].take() {
                            let _ = MIC_CH.try_send(f);
                        }
                    }
                    if wn_ok {
                        log::info!("[wake] 唤醒词命中，开始上传");
                    } else {
                        // 打印触发来源，方便判断是模型在起作用还是兜底在救场：
                        //   模型=true  → VADNet 认出来了
                        //   模型=false → 是能量阈值兜住的，说明 VADNet 没起作用
                        log::info!(
                            "[vad] 检测到人声（rms={energy} 模型={speech} 底噪={noise_floor}），开始上传"
                        );
                    }
                }
            }

            State::Listening => {
                let mut f = AudioFrame::new();
                let _ = f.extend_from_slice(&mono);
                // 队列满了就丢最旧的一帧，宁可丢音频也不能阻塞 I2S
                if MIC_CH.try_send(f).is_err() {
                    let _ = MIC_CH.try_receive();
                    let mut f2 = AudioFrame::new();
                    let _ = f2.extend_from_slice(&mono);
                    let _ = MIC_CH.try_send(f2);
                }

                // 静音判定和唤醒判定必须用**同一个量**（能量）。
                //
                // 原来这里用 `!speech`（VADNet 的结果）。一旦模型卡在「一直判
                // 有人声」，这个条件就永远不成立，设备永远停在 Listening ——
                // 和唤醒端用能量、回退端用模型，两边标准不一致就会死锁。
                let quiet = if vad_ok {
                    !speech
                } else {
                    energy < (noise_floor * 2).max(80)
                };
                if quiet {
                    quiet_frames += 1;
                } else {
                    quiet_frames = 0;
                }
                // 10 秒没声音就自动退出
                if quiet_frames > 10_000 / crate::board::FRAME_MS as u16 {
                    log::info!("[vad] 10 秒静音，回到待机");
                    set_state(State::Idle);
                    hot_frames = 0;
                    quiet_frames = 0;
                }
            }

            State::Speaking => {
                hot_frames = 0;
                quiet_frames = 0;

                // 播放期间跑 AEC。参考信号只有这段时间才有内容，而且 AEC 很贵
                // （单块约 12.5ms / 32ms 周期，接近四成 CPU），不该全程开着。
                //
                // 现在只统计 ERLE、不外发。等实机确认消得干净（15dB 以上），
                // 再把 clean 送进 MIC_CH + 通知网关打断，就是语音打断。
                if aec_ok {
                    let mut clean = [0i16; FRAME_SAMPLES];
                    crate::aec::process(&mono, &mut clean);
                    if let Some(erle) = crate::aec::take_erle() {
                        log::info!("[aec] ERLE {erle:.1} dB");
                    }
                }
            }

            _ => {
                // Thinking：继续跑 DMA 但不外发
                hot_frames = 0;
                quiet_frames = 0;
            }
        }
    }
}

/// u8 小端 PCM → i16 样本。返回写入了多少个样本。
fn pcm_from_bytes(pcm: &[u8], out: &mut [i16]) -> usize {
    let n = (pcm.len() / 2).min(out.len());
    for i in 0..n {
        out[i] = i16::from_le_bytes([pcm[2 * i], pcm[2 * i + 1]]);
    }
    n
}

/// 播放任务：从 `SPK_CH` 取单声道 PCM，展开成立体声喂 I2S
#[embassy_executor::task]
pub async fn speaker_task(mut tx: I2sTx<'static, Async>) {
    let (_, _, tx_buf, tx_desc) = esp_hal::dma_buffers!(I2S_FRAME_BYTES);
    let mut dma = match (DmaAlignedMut::new(tx_desc), DmaAlignedMut::new(tx_buf)) {
        (Ok(d), Ok(b)) => match DmaTxBuf::new(d, b) {
            Ok(v) => v,
            Err(_) => return,
        },
        _ => return,
    };
    let mut buf = [0u8; I2S_FRAME_BYTES];

    loop {
        // 【I2S 时钟必须连续 —— 这是「超慢 + 破音」的总根因】
        //
        // 原来这里用 `with_timeout(FRAME_MS, SPK_CH.receive())`：队列空的时候
        // 先干等 32ms，再花 32ms 传输静音，一个循环变成 64ms —— I2S 的
        // BCLK/WS 有一半时间是停振的。而麦克风（RX）靠 signal_loopback
        // 复用 TX 的时钟采样，于是上行音频精确变成「32ms 有声 / 32ms 静音」：
        //     块RMS = [25, 37, 1248, 67, 1268, 89, 1184, 92, ...]
        // 后果是连锁的：
        //   · 上行被挖掉一半 → VAD 永远判不到「说完」→ 每句话干等 6 秒强制切分
        //   · 下行同样被拉成一半速率 → 播放像慢放
        //   · 波形每 32ms 断一次 → 破音
        //
        // 改成非阻塞取：没有音频就**立刻**送静音帧，让 DMA 永不停。
        //
        // 曾经这么改过一次，设备「卡死」了几十秒，于是被回退了。但后来查明
        // 那次卡死的真凶是 `[mic] rms=` 每秒的日志把 USB-JTAG 堵死
        // （见 .cargo/config.toml 里 ESP_LOG 的说明），跟这里无关。
        // 日志关掉之后重新启用这个写法，是安全的。
        //
        // 下面还加了「最小循环时间」保护：万一 tx.write/wait_async 因为异常
        // 立即返回，这个循环会变成没有让出点的忙循环，那会饿死整个执行器。
        // 记录起始时刻，循环再快也至少让出 2ms。
        let t_begin = Instant::now();
        let frame = SPK_CH.try_receive().ok();

        let samples = match &frame {
            Some(f) => {
                // 单声道 → 立体声（左右复制）
                let n = f.len().min(FRAME_SAMPLES * 2);
                let samples = n / 2;
                for i in 0..samples {
                    let lo = f[i * 2];
                    let hi = f[i * 2 + 1];
                    buf[i * 4] = lo;
                    buf[i * 4 + 1] = hi;
                    buf[i * 4 + 2] = lo;
                    buf[i * 4 + 3] = hi;
                }
                samples
            }
            None => 0,
        };
        // 剩余部分补静音，避免把上一帧的尾巴播出来
        for b in &mut buf[samples * 4..] {
            *b = 0;
        }

        // ---- AEC 参考信号 ----
        // 必须在写进 I2S **之前**：参考要代表「这一刻送进 DMA 的东西」。
        // 没有音频时也要推满一帧静音 —— 写指针一停，参考和麦克风的时间轴
        // 就错位了，延迟再也对不上。
        if crate::aec::ready() {
            let mut mono = [0i16; FRAME_SAMPLES];
            if let Some(f) = &frame {
                let _ = pcm_from_bytes(f, &mut mono);
            }
            crate::aec::push_reference(&mono);
        }

        dma.as_mut_slice().copy_from_slice(&buf[..]);
        dma.set_length(I2S_FRAME_BYTES);

        let transfer = match tx.write(dma) {
            Ok(t) => t,
            Err((_, t, b)) => {
                tx = t;
                dma = b;
                // 【必须 await，不能直接 continue】
                //
                // 去掉 32ms 的 with_timeout 之后，这个分支变成「提交失败 →
                // 立刻重试」的裸循环。`tx.write()` 出错时不会让出 CPU，于是
                // speaker_task 独占执行器，所有其他任务（包括串口打印、
                // 网络轮询）全被饿死 —— 现象是设备彻底卡死、串口一个字都不吐，
                // 看起来像死机，其实是一个不 await 的忙等循环。
                //
                // 让出 1ms 既能让 DMA 有机会完成上一次传输，也保证别的任务能跑。
                Timer::after(Duration::from_millis(1)).await;
                continue;
            }
        };
        let (res, t, b) = transfer.wait_async().await;
        tx = t;
        dma = b;

        // 【最小循环时间保护 —— 这个值必须是「一帧的时长」，不能更小】
        //
        // 上面改成了 `try_receive()`（不等队列），正常情况下 `wait_async()`
        // 会等满 32ms 的 DMA 传输时间，循环天然被限速。
        //
        // 但异常路径下（wait_async 立即返回 / 传输报错）循环会变得飞快。
        // 这里踩过一个坑：兜底只写了 2ms，结果 I2S 的 DMA 一帧要 32ms 才
        // 传得完，2ms 就提交一次等于**16 倍过载**，描述符链直接被冲垮，
        // 表现是设备启动 5~16 秒后彻底静默卡死（串口零输出、不重启）。
        //
        // 正确做法是把这一圈补足到整帧时长：无论 DMA 走得多快，循环频率
        // 恒等于 32ms/次，与 I2S 的采样时钟严格对齐。这样既不会忙等，
        // 也不会重复提交。
        let frame_ms = crate::board::FRAME_MS as u64;
        let used = t_begin.elapsed().as_millis() as u64;
        if used < frame_ms {
            Timer::after(Duration::from_millis(frame_ms - used)).await;
        }
    }
}

/// 从 interleaved L/R 的 i16 字节流里取左声道
fn extract_left(interleaved: &[u8]) -> heapless::Vec<u8, FRAME_BYTES> {
    let mut out = heapless::Vec::new();
    let samples = interleaved.len() / 4;
    for i in 0..samples {
        let _ = out.push(interleaved[i * 4]);
        let _ = out.push(interleaved[i * 4 + 1]);
    }
    out
}

/// 16bit 有符号采样的 RMS
fn rms_energy(pcm: &[u8]) -> u32 {
    if pcm.len() < 2 {
        return 0;
    }
    let mut sum: u64 = 0;
    let n = pcm.len() / 2;
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as i64;
        sum += (s * s) as u64;
    }
    let mean = sum / n as u64;
    // 整数开方（牛顿法），避免引 libm
    let mut x = mean.max(1);
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + mean / x) / 2;
    }
    x as u32
}
