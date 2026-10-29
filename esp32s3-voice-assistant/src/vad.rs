//! 用 esp-sr 的 **VADNet** 替代原来的「能量阈值 VAD」
//!
//! # 为什么要换
//!
//! 原来的判据是每帧的 RMS 能量：超过自适应噪声底就认为"有人在说话"。
//! 问题是它分不清人声和噪音 —— 关门、咳嗽、敲桌子、电视、风扇，只要够响
//! 都算语音，等于白白上传 + 白烧一次 ASR 调用。
//!
//! VADNet 是乐鑫训练的小神经网络（模型 `vadnet1_medium`，281KB），
//! 对频谱特征做二分类，能把这些非人声排除掉。
//!
//! # 为什么模型要自己打包
//!
//! `esp_sr_sys` 的 build.rs 会把 `vendor/models` 下**所有**模型打成 3.4MB
//! 的 blob 塞进固件（其中中文命令词 `mn5q8_cn` 独占 2.2MB）。我们只用 VADNet，
//! 没必要为它撑爆 4MB 的 app 分区。`assets/pack_models.py` 复现了官方打包格式，
//! 只打 `vadnet1_medium`，281KB。
//!
//! # 为什么放在 static 里而不是 task 里
//!
//! `VadNet` 和 `ModelList` 都被标记成 `!Send`（内部 `PhantomData<*mut ()>`），
//! 而 embassy task 的 future 必须 `Send`。所以不能把它们放进 async fn 跨 await 持有。
//! 这里用 `UnsafeCell + unsafe impl Sync` 放到 static，安全性靠一个约定保证：
//! **只有 `mic_task` 一个任务会碰它**，且 `detect()` 和 `init()` 不会并发。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use ember_esp_sr::{VadMode, VadNet, VadState};

use crate::esp_sr;

/// 见文件头「为什么放在 static 里」
struct Global<T>(UnsafeCell<Option<T>>);
unsafe impl<T> Sync for Global<T> {}

static VAD: Global<VadNet<'static>> = Global(UnsafeCell::new(None));
/// 模型要求的每帧样本数（由模型决定，运行时才知道）
static CHUNK: AtomicUsize = AtomicUsize::new(0);

/// 加载模型并创建 VADNet。失败返回 false，调用方回落到能量 VAD。
pub fn init() -> bool {
    // 模型注册表由 `esp_sr` 统一加载（进程级单例，VADNet / WakeNet 共用）
    let Some(models) = esp_sr::models() else {
        return false;
    };

    let name = match models.find(c"vadnet", None) {
        Some(n) => n,
        None => {
            log::error!("[vad] 模型包里没有 vadnet，检查打包脚本");
            return false;
        }
    };

    // 模式取 Normal（最严格）。数字越大越容易把噪音判成语音，
    // 想减少漏检再往上调，想减少误触发就往下调。
    // 后两个参数是防抖：连续 200ms 才算语音、连续 400ms 才算安静。
    let vad = match VadNet::new(models, name, VadMode::Normal, 200, 400) {
        Ok(v) => v,
        Err(e) => {
            log::error!("[vad] VADNet 创建失败: {e:?}");
            return false;
        }
    };

    let chunk = vad.chunk_size();
    CHUNK.store(chunk, Ordering::Relaxed);
    log::info!(
        "[vad] VADNet 就绪，每帧 {} 样本（{}ms）",
        chunk,
        chunk * 1000 / 16_000
    );

    let slot: &'static mut Option<VadNet<'static>> = unsafe { &mut *VAD.0.get() };
    let _ = slot.insert(vad);
    true
}

/// 模型要求的帧长（样本数）。0 表示没初始化成功。
pub fn chunk_size() -> usize {
    CHUNK.load(Ordering::Relaxed)
}

/// 判断这一段是不是人声。`samples` 的长度必须正好等于 `chunk_size()`。
pub fn detect(samples: &mut [i16]) -> bool {
    let slot: &'static mut Option<VadNet<'static>> = unsafe { &mut *VAD.0.get() };
    match slot.as_mut() {
        Some(v) => matches!(v.detect(samples), Ok(VadState::Speech)),
        None => false,
    }
}
