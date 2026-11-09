//! 官方 esp-sr WakeNet C API 的安全封装。
//!
//! 绑定由 esp-idf-sys 从 `src/esp_sr_bindings.h` 生成——它只是包含官方
//! 头文件，本项目已经没有任何自定义 C 垫片。

use core::ffi::{c_char, c_void};

use esp_idf_sys as sys;

pub struct WakeNet {
    models: *mut sys::srmodel_list_t,
    iface: *const sys::esp_wn_iface_t,
    /// `esp_srmodel_filter` 返回的名字——指向 `models` 内部，`create()`
    /// 必须用到；保留它让 `reset()` 能重建检测器而不再碰模型列表。
    name: *const c_char,
    model: *mut sys::model_iface_data_t,
    chunk: usize,
}

impl WakeNet {
    /// 从 `model` 分区初始化 WakeNet，取名字包含 `keyword` 的第一个模型
    ///（例如 `"xiaotexiaote"`）。
    pub fn new(keyword: &str) -> Result<Self, String> {
        let mut keyword_z = Vec::with_capacity(keyword.len() + 1);
        keyword_z.extend_from_slice(keyword.as_bytes());
        keyword_z.push(0);

        unsafe {
            let models = sys::esp_srmodel_init(c"model".as_ptr());
            if models.is_null() {
                return Err(
                    "esp_srmodel_init(\"model\") returned NULL (model partition missing?)".into(),
                );
            }

            let name = sys::esp_srmodel_filter(
                models,
                c"wn".as_ptr(),
                keyword_z.as_ptr() as *const c_char,
            );
            if name.is_null() {
                sys::esp_srmodel_deinit(models);
                return Err(format!(
                    "no WakeNet model containing \"{keyword}\" in model partition"
                ));
            }

            let iface = sys::esp_wn_handle_from_name(name);
            if iface.is_null() {
                sys::esp_srmodel_deinit(models);
                return Err(format!("esp_wn_handle_from_name({}) failed", c_str(name)));
            }

            let Some(create) = (*iface).create else {
                sys::esp_srmodel_deinit(models);
                return Err("esp_wn_iface_t::create is NULL".into());
            };
            let model = create(name as *const c_void, sys::det_mode_t_DET_MODE_95);
            if model.is_null() {
                sys::esp_srmodel_deinit(models);
                return Err(format!("wakenet create({}) failed", c_str(name)));
            }

            let chunk = (*iface).get_samp_chunksize.map(|f| f(model)).unwrap_or(0);
            if chunk <= 0 {
                if let Some(destroy) = (*iface).destroy {
                    destroy(model);
                }
                sys::esp_srmodel_deinit(models);
                return Err("get_samp_chunksize returned 0".into());
            }

            println!(
                "[idf-kws] model={} chunk={} rate={}",
                c_str(name),
                chunk,
                (*iface).get_samp_rate.map(|f| f(model)).unwrap_or(0)
            );

            Ok(Self {
                models,
                iface,
                name,
                model,
                chunk: chunk as usize,
            })
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk
    }

    /// 送入一块 16 kHz 单声道样本。是唤醒词时返回 true。
    pub fn detect(&mut self, pcm: &[i16]) -> bool {
        if self.model.is_null() {
            return false;
        }
        debug_assert_eq!(pcm.len(), self.chunk);
        unsafe {
            let Some(detect) = (*self.iface).detect else {
                return false;
            };
            detect(self.model, pcm.as_ptr() as *mut i16) == sys::wakenet_state_t_WAKENET_DETECTED
        }
    }

    // 注意：刻意不用 `esp_wn_iface_t::clean` 做 reset。
    // esp-sr 2.6.1 的 `clean`（model_clean -> dl_convq_queue_bzero）在
    // 我们这版固件上对量化 wn9 模型会解引用 NULL 队列，
    // 直接 LoadProhibited 崩溃。

    /// 通过在*现有*接口上销毁并重建模型实例来清空检测器状态。
    /// 绝不重新执行 `esp_srmodel_init`/`deinit`：它们共享 esp-sr 内部的
    /// 全局状态，二次 init 之后再 deinit 会 munmap 失效指针，
    /// 实测直接 LoadProhibited 崩溃。
    pub fn reset(&mut self) -> bool {
        unsafe {
            if let Some(destroy) = (*self.iface).destroy {
                destroy(self.model);
            }
            let Some(create) = (*self.iface).create else {
                self.model = core::ptr::null_mut();
                return false;
            };
            let model = create(self.name as *const c_void, sys::det_mode_t_DET_MODE_95);
            if model.is_null() {
                self.model = core::ptr::null_mut();
                return false;
            }
            self.model = model;
            self.chunk = (*self.iface)
                .get_samp_chunksize
                .map(|f| f(model))
                .unwrap_or(0) as usize;
            true
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.model.is_null()
    }
}

impl Drop for WakeNet {
    fn drop(&mut self) {
        unsafe {
            if !self.model.is_null() {
                if let Some(destroy) = (*self.iface).destroy {
                    destroy(self.model);
                }
            }
            // 刻意不调用 esp_srmodel_deinit：srmodel 列表及其 flash mmap
            // 在 esp-sr 内部是全局/共享的；二次 init 之后 deinit 会崩。
            // 映射保留到断电为止，model 分区本来就是这么用的。
        }
    }
}

unsafe fn c_str(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return "<null>".into();
    }
    core::ffi::CStr::from_ptr(ptr)
        .to_string_lossy()
        .into_owned()
}
