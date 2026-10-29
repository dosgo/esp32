//! WakeNet 本地唤醒词
//!
//! # 它改变了什么
//!
//! 加这个之前，唤醒靠「VAD 判到有人在说话 → 把音频传给网关 → 服务端 ASR 结果里
//! 匹配关键词」。两个问题：
//!
//! 1. 每次环境噪音都要传一轮音频、烧一次 ASR 调用
//! 2. 要等 ASR 出结果才知道是不是在叫它（多几百毫秒）
//!
//! WakeNet 把这一步搬到本地：模型常驻 flash（284KB），每 30ms 判一次，
//! 命中之前**一个字节都不上传**。
//!
//! # 唤醒词
//!
//! 当前模型 `wn9_xiaotexiaote_tts2`，唤醒词是 **「小特小特」**。
//! 想换词得换模型文件：把新模型目录放进 `assets/models/`，跑一次
//! `assets/pack_models.py`，再改这里的 `find(c"wakenet", ...)` 前缀。
//!
//! # 为什么要有累积缓冲
//!
//! WakeNet 的块长由模型决定（通常是 480 = 30ms），而 I2S 帧长是 512（32ms，
//! 为了对齐 AEC）。512 除不尽 480，直接丢弃余数会每帧丢 6% 的音频，
//! 唤醒率会掉。所以这里跨帧累积，凑够一块就判一块。
//!
//! # 状态
//!
//! `WakeNet` 是 `!Send`，和其它 esp-sr 对象一样只能放 static，靠
//! 「只有 `mic_task` 会调用」这个约定保证安全。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use ember_esp_sr::{WakeNet, WakeNetMode, WakeNetState};

use crate::esp_sr;

/// 累积缓冲容量。要容得下「余数（< 块长）+ 一帧（512）」，1024 有余。
const ACC_CAP: usize = 1024;

struct Global<T>(UnsafeCell<Option<T>>);
unsafe impl<T> Sync for Global<T> {}

static WN: Global<WakeNet<'static>> = Global(UnsafeCell::new(None));

struct Acc {
    buf: [i16; ACC_CAP],
    len: usize,
}

static ACC: Global<Acc> = Global(UnsafeCell::new(None));

/// 模型要求的块长（样本数）。0 表示没初始化成功。
static CHUNK: AtomicUsize = AtomicUsize::new(0);

/// 累计判定过多少块，用来确认 detect 真的在被调用（每块 32ms）
static CHUNKS: AtomicUsize = AtomicUsize::new(0);

/// 供 audio.rs 打日志用
pub fn chunks_total() -> usize {
    CHUNKS.load(Ordering::Relaxed)
}

/// 加载模型并创建 WakeNet。失败返回 false，调用方回落到 VAD 唤醒。
pub fn init() -> bool {
    let Some(models) = esp_sr::models() else {
        return false;
    };

    // 前缀必须用 ESP_WN_PREFIX，也就是 "wn" —— 不是 "wakenet"。
    // 对照：VADNet 用 "vadnet"、MultiNet 用 "mn"，都在 esp-sr 的头文件里定义。
    let name = match models.find(c"wn", None) {
        Some(n) => n,
        None => {
            log::error!("[wake] 模型包里没有 WakeNet 模型（前缀 wn），检查 assets/pack_models.py");
            return false;
        }
    };

    // 实测 Normal 下喊「小特小特」一直不命中（麦克风电平正常，rms 能到 1000），
    // 改成 Aggressive 提高召回率。误触发变多再调回来。
    let wn = match WakeNet::new(models, name, WakeNetMode::Aggressive) {
        Ok(w) => w,
        Err(e) => {
            log::error!("[wake] 创建失败: {e:?}");
            return false;
        }
    };

    let chunk = wn.chunk_size();
    if chunk == 0 || chunk > ACC_CAP {
        log::error!("[wake] 块长异常: {chunk}");
        return false;
    }
    CHUNK.store(chunk, Ordering::Relaxed);

    // 唤醒词是哪个，直接从模型里读出来，省得用户猜
    let words = wn.word_count();
    if words > 0 {
        if let Some(w) = wn.word_name(1) {
            if let Ok(text) = w.to_str() {
                log::info!("[wake] 唤醒词 = {text}");
            }
        }
    }
    log::info!(
        "[wake] 就绪，每块 {} 样本（{}ms）",
        chunk,
        chunk * 1000 / 16_000
    );

    let slot: &'static mut Option<WakeNet<'static>> = unsafe { &mut *WN.0.get() };
    let _ = slot.insert(wn);

    let slot: &'static mut Option<Acc> = unsafe { &mut *ACC.0.get() };
    let _ = slot.insert(Acc {
        buf: [0; ACC_CAP],
        len: 0,
    });

    true
}

/// 喂一帧麦克风 PCM（u8 小端 i16），返回是否命中唤醒词。
pub fn detect(pcm: &[u8]) -> bool {
    let chunk = CHUNK.load(Ordering::Relaxed);
    if chunk == 0 {
        return false;
    }

    let Some(acc) = (unsafe { (&mut *ACC.0.get()).as_mut() }) else {
        return false;
    };
    let slot: &'static mut Option<WakeNet<'static>> = unsafe { &mut *WN.0.get() };
    let Some(wn) = slot.as_mut() else {
        return false;
    };

    // ---- 把这一帧追加进累积缓冲 ----
    let n = (pcm.len() / 2).min(ACC_CAP);
    if acc.len + n > ACC_CAP {
        // 正常不会发生（每帧只会剩 < chunk 的尾巴）。真溢出就丢掉最旧的，
        // 宁可漏一小段也不能越界。
        let drop = (acc.len + n) - ACC_CAP;
        acc.buf.copy_within(drop..acc.len, 0);
        acc.len -= drop;
    }
    for i in 0..n {
        acc.buf[acc.len + i] = i16::from_le_bytes([pcm[2 * i], pcm[2 * i + 1]]);
    }
    acc.len += n;

    // ---- 凑够一块就判一块 ----
    let mut hit = false;
    let mut len = acc.len;
    while len >= chunk {
        CHUNKS.fetch_add(1, Ordering::Relaxed);
        if matches!(wn.detect(&mut acc.buf[..chunk]), Ok(WakeNetState::Detected)) {
            hit = true;
        }
        acc.buf.copy_within(chunk..len, 0);
        len -= chunk;
    }
    acc.len = len;

    if hit {
        // 命中后复位，避免同一次「小特小特」连报好几次
        wn.reset();
        acc.len = 0;
    }
    hit
}

/// 清空内部状态。离开 Idle 时调用 —— 否则回到 Idle 时缓冲里还留着
/// 上次对话的残音，可能立刻误触发。
pub fn reset() {
    let slot: &'static mut Option<WakeNet<'static>> = unsafe { &mut *WN.0.get() };
    if let Some(wn) = slot.as_mut() {
        wn.reset();
    }
    let acc = unsafe { (&mut *ACC.0.get()).as_mut() };
    if let Some(a) = acc {
        a.len = 0;
    }
}
