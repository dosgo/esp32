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

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

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
///
/// 【深度 8 → 12，2026-10-28】
///
/// 播放侧有个**必然存在的时钟漂移**：一次 DMA 传输严格是 32ms 的音频，
/// 但 `wait_async()` 返回后还要 copy 2KB + 重新提交描述符，一圈实际要
/// 32ms + 1~2ms —— 也就是设备每秒只消费掉约 0.97 秒的音频。
/// 网关是按 32ms/帧**实时**推的，于是队列每播一句就涨一点：
/// 6 秒的长句能涨 3~6 帧，从 4 帧涨到 8 帧就顶满 → 之后每来一帧都得丢一帧，
/// 丢帧处就是一声「啵」。短句听不出来、长句一直啵，正是用户描述的现象。
///
/// 加深到 24 帧（768ms），配合 main.rs 里的「队列快满就不读 socket」背压，
/// 让 TCP 窗口把网关的发送速率自然压到设备消费速率；每次 `tts start` 会把
/// 队列清空（见 main.rs 的 handle_text），保证跨轮次不会越积越多。
pub static SPK_CH: Channel<CriticalSectionRawMutex, AudioFrame, 24> = Channel::new();

/// 播放统计：给「破音/卡顿」定位用，每轮 TTS 结束由 main.rs 打一行。
///
/// 三件事一眼看出问题在哪：
///   empty  播放时队列空的次数 → 网关推得慢/网络抖，设备只能补静音（咔）
///   drop   队列满丢帧的次数   → 设备消费比网关慢（时钟漂移），丢帧处啵一声
///   used_ms 平均一圈耗时       → 正常应该 32~34ms，明显更大说明 DMA 在等
pub static SPK_PLAYED: AtomicU32 = AtomicU32::new(0);
pub static SPK_EMPTY: AtomicU32 = AtomicU32::new(0);
pub static SPK_DROP: AtomicU32 = AtomicU32::new(0);
pub static SPK_USED_MS: AtomicU32 = AtomicU32::new(0);
/// 本轮播放出现过的最大样本绝对值。
///
/// 用来区分「破音」的两大来源，只看一个数就够：
///   ≈32767 → 数字信号本身已经削顶（网关侧电平给太高 / DAC 音量太大）
///   7000~12000 → 数字侧有 >10dB 余量，破音只可能来自丢帧或模拟侧
pub static SPK_PEAK: AtomicU32 = AtomicU32::new(0);

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

    // ---- 麦克风声道自动校准 ----
    //
    // ES8311 是单声道 ADC，数据只落在 I2S 的一个 slot 上。原来写死取左声道，
    // 如果这块板子实际把 ADC 数据放在右 slot，左 slot 就是浮空 —— 采到的正好是
    // 「±30 底噪 + 帧首假尖峰」，整段音频里没有一个字的语音。
    //
    // 启动后前 3 秒（96 帧 = 3.07s）分别累计左右声道的能量，谁有声音就选谁。
    // 两路都只有底噪时会明确打日志，指向硬件（麦克风没供电 / 引脚接错 / PGA 太小）。
    let mut ch_l: u64 = 0;
    let mut ch_r: u64 = 0;
    let mut ch_n: u32 = 0;
    let mut mic_ch: usize = 0; // 0 = 左，1 = 右
    const CH_CAL_FRAMES: u32 = 96;

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

        // ---- 声道校准（只在启动后的前 3 秒做）----
        //
        // 用平方和比较而不是 RMS 均值，避免开方误差影响判断；两声道样本数相同，
        // 所以直接比总能量即可。
        if ch_n < CH_CAL_FRAMES {
            let l = rms_channel(dma.as_slice(), 0) as u64;
            let r = rms_channel(dma.as_slice(), 1) as u64;
            ch_l += l * l;
            ch_r += r * r;
            ch_n += 1;
            if ch_n == CH_CAL_FRAMES {
                let l_avg = isqrt(ch_l / CH_CAL_FRAMES as u64);
                let r_avg = isqrt(ch_r / CH_CAL_FRAMES as u64);
                mic_ch = if ch_r > ch_l { 1 } else { 0 };
                let (hi, lo) = if l_avg >= r_avg { (l_avg, r_avg) } else { (r_avg, l_avg) };
                if hi < 60 {
                    // 两路都只有底噪 —— 不是声道选错，是麦克风根本没信号
                    log::error!(
                        "[mic] 左右声道都没有信号 L={l_avg} R={r_avg}：查麦克风供电/引脚/PGA 增益"
                    );
                } else if hi > lo * 2 {
                    log::warn!(
                        "[mic] 声道校准 L={l_avg} R={r_avg} → 用{}声道（差异显著，另一路基本是浮空）",
                        if mic_ch == 0 { "左" } else { "右" }
                    );
                } else {
                    log::warn!(
                        "[mic] 声道校准 L={l_avg} R={r_avg} → 用{}声道（两路接近）",
                        if mic_ch == 0 { "左" } else { "右" }
                    );
                }
            }
        }

        // 里面是 interleaved L/R 的 i16，取校准选中的那个声道
        let mono = extract_channel(dma.as_slice(), mic_ch);
        let energy = rms_energy(&mono);
        let st = state();

        // 麦克风电平诊断。
        //
        // 【只打一次，绝不能周期性打】
        // esp-println 走 USB-Serial-JTAG，主机端不读、FIFO 积压时 write 会阻塞，
        // 而 mic_task 一阻塞整条音频链就停摆。实测每 10 秒打一条，几分钟后设备
        // 就 ping 不通了（ping 全超时 = 射频任务还在跑、音频任务被日志堵死）。
        //
        // 所以这里不再周期性输出：需要看电平时，重启一次设备，看启动后约 3 秒
        // 那条「声道校准」日志即可 —— 它会同时给出 L/R 的 RMS 和最终选中的声道。
        // 想看「说话时电平」，直接看网关侧的 ASR 结果和 debug_last.wav 更准。
        mic_tick = mic_tick.wrapping_add(1);
        let _ = mic_tick;

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
                        // 噪声底跟随：向下快、向上慢。
                        //
                        // 原来是 `if energy < noise_floor * 2 { 掺一脚 }`，
                        // 等价于「只要没超阈值就更新底噪」。可是人声里起伏到
                        // 阈值以下的那批帧（实测很多落在 350~600）会一路把底噪
                        // 抬到接近人声水平，阈值跟着涨，设备越用越迟钝 ——
                        // 这也是「一开始能叫醒，用一会儿就不应了」的原因之一。
                        //
                        // 现在：明显比底噪安静时一帧拉掉 1/4（快速收敛到真实底噪），
                        // 略高于底噪时才慢跟，人声那种远离底噪的帧完全不参与。
                        if energy * 4 < noise_floor * 3 {
                            noise_floor = (noise_floor * 3 + energy) / 4;
                        } else if energy * 2 < noise_floor * 3 {
                            noise_floor = (noise_floor * 31 + energy) / 32;
                        }
                        // 【阈值下限取 80】
                        //
                        // 实测这台机器的麦克风电平：安静环境 rms 在 100 以下，
                        // 正常说话几百到上千。原来的 `.max(400)` 会把大量人声帧
                        // 判成静音，连续 2 帧的门槛永远凑不齐 —— 表现就是
                        // 「我明明说话了，它一动不动」。
                        //
                        // 80 位于「安静上限」和「说话下限」之间，留了余量，
                        // 也不会被环境底噪误触发。
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
                        // 触发来源改用 debug 级别：这是**每次唤醒都会打**的，
                        // 环境偏吵时设备会反复进出 Listening，info 级别会刷屏
                        // 并把 USB-JTAG 堵死（整机 ping 不通）。需要看时把
                        // ESP_LOG 调成 debug 即可。
                        log::debug!(
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
        let _ = frame.is_none() && {
            // 正在播放（Speaking）却取不到帧 = 队列被抽干，这一帧只能补静音。
            // 每一次补静音都是一个「有声→静音→有声」的硬跳变，听起来就是咔。
            if crate::audio::state() == State::Speaking {
                SPK_EMPTY.fetch_add(1, Ordering::Relaxed);
            }
            true
        };

        if let Some(f) = &frame {
            // 峰值统计（每帧扫一遍，512 个样本，开销可忽略）
            let mut pk = 0u32;
            let n = f.len() / 2;
            for i in 0..n {
                let v = i16::from_le_bytes([f[i * 2], f[i * 2 + 1]]);
                let a = (v as i32).unsigned_abs();
                if a > pk {
                    pk = a;
                }
            }
            if pk > SPK_PEAK.load(Ordering::Relaxed) {
                SPK_PEAK.store(pk, Ordering::Relaxed);
            }
        }

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
        // 累计耗时，用来算「设备消费音频的实际速度」（时钟漂移的直接证据）
        SPK_USED_MS.fetch_add(used as u32, Ordering::Relaxed);
        SPK_PLAYED.fetch_add(1, Ordering::Relaxed);
        // 【只在「明显异常地快」时才补睡】
        //
        // 原来写的是 `if used < frame_ms { sleep(frame_ms - used) }`。
        // 问题是 `used` 是 as_millis() 截断的：实际 31.9ms 会算成 31，
        // 于是每次都去睡 1ms；而在 esp-rtos 的 tick 粒度下，1ms 的 Timer
        // 实际会睡 5~6ms —— 一圈变成 38ms，比 32ms 慢 19%。
        //
        // 这个「慢」的后果是连锁的：设备消费音频比网关推送慢 → 队列被抽干
        // → 一半的帧只能补静音 → 每个接缝一声「啵」。
        // 实测 [spk] 平均周期一直是 38ms，改成只在 used < 半帧时才补睡之后
        // 回到 32~33ms。
        //
        // 保留这个兜底是为了防「wait_async 立刻返回」造成的忙循环（曾经把设备
        // 干到彻底静默），只要求它在异常路径上生效即可。
        if used < frame_ms / 2 {
            Timer::after(Duration::from_millis(frame_ms - used)).await;
        }
    }
}

/// 从 interleaved L/R 的 i16 字节流里取指定声道（0 = 左，1 = 右）
///
/// 【为什么不再写死取左声道】
/// ES8311 是**单声道 ADC**，数据只落在 I2S 的某一个 slot 上。之前这里写死
/// 取左声道，如果这块板子实际把 ADC 数据放在右 slot，左 slot 就是浮空的 ——
/// 采到的恰好是「±30 底噪 + 帧首假尖峰」，也就是 `server/debug_last.wav` 里
/// 看到的那副样子。音频里没有语音，ASR 自然 100% 空结果。
fn extract_channel(interleaved: &[u8], ch: usize) -> heapless::Vec<u8, FRAME_BYTES> {
    let mut out = heapless::Vec::new();
    let samples = interleaved.len() / 4;
    let off = ch * 2;
    for i in 0..samples {
        let _ = out.push(interleaved[i * 4 + off]);
        let _ = out.push(interleaved[i * 4 + off + 1]);
    }
    // 【必须抹掉本帧第 1 个样本 —— 它是硬件的假数据，不是声音】
    //
    // 实测（把喇叭静音、只录麦克风）：每一次 I2S DMA 传输的第 1 个样本都是一个
    // 随机满幅值（8193 / 16383 / 28672 / 32767…），而后 511 个样本正常。
    // 证据：静音段的 512 样本块里，只有一个样本是 ±32768、其余全部 <40，
    // 块 RMS 恰好 1448 = sqrt(32768²/512)，也就是纯粹由这一个假样本贡献的。
    //
    // 危害极大，因为它会被当成「声音」：
    //   · 块 RMS 从 25 被抬到 1448（正常说话才 350~1450）→ 网关的 VAD
    //     在**静音时也判成有人说话** → 静音计时永远清零 → 断句永远等不到，
    //     只能每 6 秒强制切一次 → 用户感觉「说完半天不理我」
    //   · 它同时混进 ASR 的输入，每 32ms 一个满幅脉冲，识别率必然掉
    //
    // 用「复制下一个样本」而不是置 0：置 0 等于人为造一个阶跃，同样是一次
    // 咔哒声；复制一个相邻样本在 16kHz 下误差 < 1 个采样周期，听不出来。
    if out.len() >= 4 {
        out[0] = out[2];
        out[1] = out[3];
    }
    out
}

/// 直接对 interleaved 数据算某个声道的 RMS。
///
/// 特意不复制缓冲：`mic_task` 的栈很紧（曾经 2KB 数组就把它压爆过），
/// 而声道校准要同时看 L/R 两个数，用这个函数是零额外分配的。
fn rms_channel(interleaved: &[u8], ch: usize) -> u32 {
    let samples = interleaved.len() / 4;
    if samples == 0 {
        return 0;
    }
    let off = ch * 2;
    let mut sum: u64 = 0;
    for i in 0..samples {
        let s = i16::from_le_bytes([interleaved[i * 4 + off], interleaved[i * 4 + off + 1]]) as i64;
        sum += (s * s) as u64;
    }
    isqrt(sum / samples as u64)
}

/// 整数开方（牛顿法），避免引 libm
fn isqrt(mean: u64) -> u32 {
    let mut x = mean.max(1);
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + mean / x) / 2;
    }
    x as u32
}

/// 16bit 有符号采样的 RMS
///
/// 【跳过第 1 个样本 —— 这是「设备一直不理人」的直接元凶之一】
///
/// 2026-10-28 复盘真实上行录音（`server/debug_last.wav`，由网关保存）：
/// - 每 512 个样本（= 每帧 = 每个 WS 消息 = 每次 DMA 传输）的**第 1 个样本**
///   都是假尖峰，取值全是 2 的幂次附近：-32767 / 28672 / 16383 / 8192 / -8193 …
///   98 个大尖峰的位置 **mod 512 全部等于 0**，一个例外都没有；
/// - 把这一帧的首样本剔掉后，剩下 511 个样本的 RMS 只有 **17.8**、峰值 **116**。
///
/// 一个 -32767 就足以把整帧 RMS 从 17 抬到 1448，于是能量 VAD 被它骗死：
/// 设备永远退不出 Listening（静音计时被尖峰反复清零）→ 一直上传，
/// 网关只能每 6 秒兜底切分一次（日志里 1693 次）→ ASR 拿到全是碎片 → 空结果。
///
/// 丢掉一个样本对语音毫无损失（32ms 里砍掉 62.5µs）。
fn rms_energy(pcm: &[u8]) -> u32 {
    let n = pcm.len() / 2;
    if n < 3 {
        return 0;
    }
    let mut sum: u64 = 0;
    for i in 1..n {
        let s = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as i64;
        sum += (s * s) as u64;
    }
    isqrt(sum / (n as u64 - 1))
}
