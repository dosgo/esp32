//! esp-sr 模型注册表的共享入口
//!
//! # 为什么需要这一层
//!
//! `ModelList` 是**进程级单例**：`esp_sr_sys` 内部有个全局标志，第二次调用
//! `ModelList::load()` 会直接返回 `Error::ModelListAlreadyLoaded`。
//!
//! VADNet 和 WakeNet 都要借用同一个 `ModelList`（它们的生命周期参数
//! `&'models ModelList` 就是这个意思），所以必须由一个人加载、其他人共享。
//!
//! # 模型包
//!
//! `assets/srmodels.bin` 是 `assets/pack_models.py` 生成的，只含两个模型：
//!
//! | 模型 | 用途 | 大小 |
//! |---|---|---|
//! | `vadnet1_medium` | 神经网络 VAD | 281 KB |
//! | `wn9_xiaotexiaote_tts2` | 唤醒词「小特小特」 | 284 KB |
//!
//! 不用 `ModelList::load_default()` 是因为它会把 `vendor/models` 下**全部**模型
//! 打进去（含 2.2MB 的中文命令词 `mn5q8_cn`），共 3.4MB。
//!
//! 模型在 flash 里通过 XIP 直接读，不占 RAM。

use core::cell::UnsafeCell;

use ember_esp_sr::ModelList;

/// 只含 VADNet + WakeNet 的模型包（565KB），由 `assets/pack_models.py` 生成。
static MODEL_BLOB: &[u8] = include_bytes!("../assets/srmodels.bin");

/// `ModelList` 是 `!Send`，不能放进 async task。放 static + `unsafe impl Sync`，
/// 安全性由「只有 `mic_task` 会调用本模块」这个约定保证。
struct Global<T>(UnsafeCell<Option<T>>);
unsafe impl<T> Sync for Global<T> {}

static MODELS: Global<ModelList> = Global(UnsafeCell::new(None));

/// 取模型注册表，第一次调用时加载。
///
/// 返回 `None` 表示加载失败（模型包损坏），调用方应该回落到不依赖模型的路径。
pub fn models() -> Option<&'static ModelList> {
    let slot: &'static mut Option<ModelList> = unsafe { &mut *MODELS.0.get() };
    if slot.is_none() {
        match ModelList::load(MODEL_BLOB) {
            Ok(m) => {
                log::info!("[srmodel] 模型包已加载 {} KB", MODEL_BLOB.len() / 1024);
                let _ = slot.insert(m);
            }
            Err(e) => {
                log::error!("[srmodel] 模型包加载失败: {e:?}（检查 assets/srmodels.bin）");
                return None;
            }
        }
    }
    slot.as_ref()
}
