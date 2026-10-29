//! AEC 回声消除（esp-sr 官方算法，`ember_esp_sr` 裸机移植版）
//!
//! # 它解决什么问题
//!
//! 喇叭在响的时候，麦克风收到的全是喇叭的声音。不消除这层回声，就没法区分
//! 「用户在说话」和「我自己在响」，于是只能采用最笨的办法：播放时干脆不开麦克风。
//!
//! 本模块把「即将送去播放的音频」当作参考信号，从麦克风信号里把它减掉。
//! 减干净之后，播放期间也能继续收音，才有可能做**语音打断**。
//!
//! # 数据流
//!
//! ```text
//! speaker_task ──push_reference()──> 参考环形缓冲 ──┐
//!                                                    ├─> process() ─> 干净信号
//! mic_task ──────────────────────────────────────────┘
//! ```
//!
//! # 参考信号为什么必须延迟
//!
//! `push_reference()` 调用的时刻是「样本写进 I2S DMA」的瞬间，但声音要到
//! 一帧之后才真正从喇叭出来，再经空气传到麦克风。所以麦克风此刻听到的，
//! 是一段时间**之前**写入的参考。
//!
//! 这个时间差就是 [`AEC_REF_DELAY`]，对不准的话 AEC 基本无效（ERLE 停在 0~5dB）。
//! 串口会周期性打印 ERLE，调它的方法见 `board.rs` 里那个常量的注释。
//!
//! # 为什么工作区放在 static
//!
//! 三个 `AlignedFrame` 各 1KB，加上参考缓冲 16KB。放进 task 局部变量会把
//! embassy 的栈撑爆（这个项目已经因为同样原因踩过一次链接错误）。

use core::cell::{RefCell, UnsafeCell};
use core::sync::atomic::{AtomicU32, Ordering};

use critical_section::Mutex;
use ember_esp_sr::{Aec, AlignedFrame};

use crate::board::{AEC_REF_DELAY, FRAME_SAMPLES};

/// AEC 官方固定的块长（32ms @ 16kHz）。`board::FRAME_SAMPLES` 必须等于它，
/// 否则每帧会多出零头，需要在外面做累积重分块。
const AEC_BLOCK: usize = 512;

const _: () = assert!(
    FRAME_SAMPLES == AEC_BLOCK,
    "FRAME_SAMPLES 必须等于 AEC 的 512，否则 mic_task 要做重分块"
);

/// 参考环形缓冲容量（样本）。16kHz 下 8192 = 512ms，
/// 足够覆盖 DMA 缓冲 + 硬件 FIFO + 声学传播的总延迟。
const REF_CAP: usize = 8192;

/// `Aec` 是 `!Send`（内部 `PhantomData<*mut ()>`），不能放进 async task。
/// 用 UnsafeCell 放 static，安全性由「只有 mic_task 碰它」这个约定保证。
struct Global<T>(UnsafeCell<Option<T>>);
unsafe impl<T> Sync for Global<T> {}

static AEC: Global<Aec> = Global(UnsafeCell::new(None));

struct RefRing {
    buf: &'static mut [i16],
    /// 下一个写入位置
    head: usize,
}

static REF: Mutex<RefCell<Option<RefRing>>> = Mutex::new(RefCell::new(None));

/// 三帧工作区 + ERLE 累计。放 static 不占 task 栈。
struct Work {
    mic: AlignedFrame,
    rf: AlignedFrame,
    out: AlignedFrame,
}

static WORK: Global<Work> = Global(UnsafeCell::new(None));

// ERLE 统计：存的是「每样本平均能量 ÷ 1024」，用 u32 累加若干帧也不会溢出。
static RAW_E: AtomicU32 = AtomicU32::new(0);
static CLEAN_E: AtomicU32 = AtomicU32::new(0);
static STAT_FRAMES: AtomicU32 = AtomicU32::new(0);

// ============================================================================
// 初始化
// ============================================================================

/// 创建 AEC 并准备参考缓冲。失败返回 false，调用方改成直通（功能不受影响，
/// 只是播放期间没法收音）。
pub fn init() -> bool {
    // ---- 参考缓冲：16KB，放 PSRAM，别进 BSS ----
    let raw = crate::alloc_psram_buf(REF_CAP * 2);
    if raw.as_ptr() as usize % 2 != 0 {
        log::error!("[aec] 参考缓冲不是 2 字节对齐，放弃");
        return false;
    }
    // SAFETY: `raw` 是 REF_CAP*2 字节、至少 2 字节对齐的可写内存，
    // 转成 REF_CAP 个 i16 后长度严格不超出。
    let samples: &'static mut [i16] =
        unsafe { core::slice::from_raw_parts_mut(raw.as_mut_ptr() as *mut i16, REF_CAP) };
    critical_section::with(|cs| {
        REF.borrow_ref_mut(cs).replace(RefRing {
            buf: samples,
            head: 0,
        });
    });

    // ---- 工作区 ----
    let slot: &'static mut Option<Work> = unsafe { &mut *WORK.0.get() };
    let _ = slot.insert(Work {
        mic: AlignedFrame::silence(),
        rf: AlignedFrame::silence(),
        out: AlignedFrame::silence(),
    });

    // ---- AEC 本体 ----
    match Aec::new() {
        Ok(a) => {
            let slot: &'static mut Option<Aec> = unsafe { &mut *AEC.0.get() };
            let _ = slot.insert(a);
            log::info!(
                "[aec] 就绪，{} 样本/块，参考延迟 {} 样本（{}ms）",
                AEC_BLOCK,
                AEC_REF_DELAY,
                AEC_REF_DELAY * 1000 / 16_000
            );
            true
        }
        Err(e) => {
            // 最常见原因：内部堆不够（官方算法要 ~122KiB）
            log::warn!("[aec] 创建失败: {e:?}（内存不足？回落直通）");
            false
        }
    }
}

// ============================================================================
// 参考信号
// ============================================================================

/// AEC 是否可用。`speaker_task` 靠它决定要不要每帧推参考。
pub fn ready() -> bool {
    unsafe { (*AEC.0.get()).is_some() }
}

/// 把「即将送去 I2S TX 的样本」写进参考缓冲。
///
/// **必须在写 I2S 之前调用**，而且没音频时要推等长静音 —— 否则缓冲的
/// 写指针会停住，和麦克风的时间轴错位，延迟就再也对不上了。
pub fn push_reference(mono: &[i16]) {
    critical_section::with(|cs| {
        let mut g = REF.borrow_ref_mut(cs);
        let Some(r) = g.as_mut() else { return };
        for &s in mono {
            // 先取出 head：`r.buf[r.head] = s` 会同时可变+不可变借用 r
            let h = r.head;
            r.buf[h] = s;
            r.head = (h + 1) % REF_CAP;
        }
    });
}

/// 取「延迟 `delay` 个样本之前」的一段参考，写进 `out`。
fn fill_reference(out: &mut [i16], delay: usize) {
    critical_section::with(|cs| {
        let g = REF.borrow_ref(cs);
        let Some(r) = g.as_ref() else { return };
        let n = out.len().min(REF_CAP);
        // 目标是「head 往前退 delay+n 个样本」处，再往后读 n 个
        let back = (delay + n) % REF_CAP;
        let start = (r.head + REF_CAP - back) % REF_CAP;
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = r.buf[(start + i) % REF_CAP];
        }
    });
}

// ============================================================================
// 处理
// ============================================================================

/// 对一帧麦克风数据跑 AEC。
///
/// - `mic_pcm`：u8 小端单声道 PCM，长度至少 `FRAME_SAMPLES * 2`
/// - `out`：写出 `FRAME_SAMPLES` 个 i16 样本
///
/// 返回 true 表示真的跑了 AEC（false 时 `out` 里是原样拷贝）。
pub fn process(mic_pcm: &[u8], out: &mut [i16]) -> bool {
    let ok = process_inner(mic_pcm, out);
    if !ok {
        // 直通：AEC 不可用时保持音频链路完整
        let n = (mic_pcm.len() / 2).min(out.len());
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = i16::from_le_bytes([mic_pcm[2 * i], mic_pcm[2 * i + 1]]);
        }
    }
    ok
}

fn process_inner(mic_pcm: &[u8], out: &mut [i16]) -> bool {
    let Some(w) = (unsafe { (&mut *WORK.0.get()).as_mut() }) else {
        return false;
    };

    // u8 PCM → i16
    let n = (mic_pcm.len() / 2).min(AEC_BLOCK);
    for i in 0..n {
        w.mic.0[i] = i16::from_le_bytes([mic_pcm[2 * i], mic_pcm[2 * i + 1]]);
    }
    for slot in w.mic.0.iter_mut().skip(n) {
        *slot = 0;
    }

    // 取对应的参考
    fill_reference(&mut w.rf.0, AEC_REF_DELAY);

    // 统计原始能量（在 AEC 里，mic 会被原地改写，所以要提前算）
    let mut raw_sum: u64 = 0;
    for &s in w.mic.0.iter() {
        raw_sum += (s as i64 * s as i64) as u64;
    }

    let slot: &'static mut Option<Aec> = unsafe { &mut *AEC.0.get() };
    let Some(aec) = slot.as_mut() else {
        return false;
    };
    aec.process(&mut w.mic, &mut w.rf, &mut w.out);

    let mut clean_sum: u64 = 0;
    for (i, dst) in out.iter_mut().take(AEC_BLOCK).enumerate() {
        let s = w.out.0[i];
        *dst = s;
        clean_sum += (s as i64 * s as i64) as u64;
    }

    // 累加 ERLE 统计（÷1024 后进 u32，累积几十帧不溢出）
    RAW_E.fetch_add((raw_sum / AEC_BLOCK as u64 / 1024) as u32, Ordering::Relaxed);
    CLEAN_E.fetch_add((clean_sum / AEC_BLOCK as u64 / 1024) as u32, Ordering::Relaxed);
    STAT_FRAMES.fetch_add(1, Ordering::Relaxed);
    true
}

/// 取一次累计的 ERLE（dB）并清零。攒够 128 帧（约 4 秒）才返回，否则 None。
///
/// ERLE = 10·log10(原始能量 / 残余能量)。正常值 15~25dB；
/// 一直 0~5dB 说明 `AEC_REF_DELAY` 没对准，按 `board.rs` 里注释调。
pub fn take_erle() -> Option<f32> {
    if STAT_FRAMES.load(Ordering::Relaxed) < 128 {
        return None;
    }
    let raw = RAW_E.swap(0, Ordering::Relaxed);
    let clean = CLEAN_E.swap(0, Ordering::Relaxed);
    STAT_FRAMES.store(0, Ordering::Relaxed);
    if raw == 0 || clean == 0 {
        return None;
    }
    Some(10.0 * libm::log10f(raw as f32 / clean as f32))
}
