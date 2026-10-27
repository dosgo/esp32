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
use embassy_time::{with_timeout, Duration};
use esp_hal::dma::aligned::DmaAlignedMut;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::i2s::master::{I2sRx, I2sTx};
use esp_hal::Async;

use crate::board::{FRAME_BYTES, FRAME_SAMPLES, I2S_FRAME_BYTES};

pub type AudioFrame = heapless::Vec<u8, FRAME_BYTES>;

/// 录音帧：audio_task → ws_task
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
    // DMA 缓冲必须内存对齐，交给 dma_buffers! 宏分配（内部是 static）
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
    const SR_ENABLE: bool = true;

    // ---- VADNet ----
    // 加载失败就整个回落到能量 VAD，不影响语音链路能用
    let vad_ok = if SR_ENABLE { crate::vad::init() } else { false };
    let vad_chunk = crate::vad::chunk_size();
    // 唤醒判定：VADNet 内部已有 200ms 防抖，外面再要 2 帧（64ms）
    let hot_needed = if vad_ok { 2 } else { 3 };

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

        // 麦克风电平诊断：每 32 帧（约 1 秒）打一次。
        // 静音底噪通常是个位数，正常说话能到几百甚至上千。
        // 如果一直贴着 0~2，说明 ES7210 没出数据或 I2S RX 接错。
        mic_tick = mic_tick.wrapping_add(1);
        if mic_tick % 32 == 0 {
            log::info!(
                "[mic] rms={energy}  已判 {} 块唤醒",
                crate::wakenet::chunks_total()
            );
        }

        let st = state();

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
                } else if vad_ok {
                    if speech {
                        hot_frames = hot_frames.saturating_add(1);
                    } else {
                        hot_frames = 0;
                    }
                    hot_frames >= hot_needed
                } else {
                    // 噪声底慢跟随：只在安静帧更新，避免人声把阈值顶上去
                    if energy < noise_floor * 2 {
                        noise_floor = (noise_floor * 15 + energy) / 16;
                    }
                    let threshold = (noise_floor * 3).max(400);
                    if energy > threshold {
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
                        log::info!("[vad] 检测到人声，开始上传");
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

                let quiet = if vad_ok {
                    !speech
                } else {
                    let threshold = (noise_floor * 3).max(400);
                    energy < threshold
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
        // 【关键】没有音频时必须补静音帧，不能让 TX 停下来。
        //
        // ESP32-S3 的 I2S 是「有 DMA 传输才输出 BCLK/WS」的。一旦 TX 空闲，
        // BCLK 停振，而 RX 靠 signal_loopback 复用 TX 的时钟 —— 于是麦克风
        // 也就静默了，现象是"不说话的时候按唤醒没反应"。
        // 这里用 60ms 超时兜底：有音频就播，没有就送一帧静音维持时钟。
        let frame = with_timeout(Duration::from_millis(crate::board::FRAME_MS as u64), SPK_CH.receive())
            .await
            .ok();

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
                continue;
            }
        };
        let (res, t, b) = transfer.wait_async().await;
        tx = t;
        dma = b;
        if res.is_err() {
            continue;
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
