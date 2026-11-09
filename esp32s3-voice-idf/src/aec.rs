//! 官方 esp-sr AEC C API（`esp_aec.h`）的安全封装。
//!
//! 这里全部是 Rust：句柄和对齐缓冲通过 `heap_caps_aligned_alloc()` 分配，
//! 因为 `aec_process()` 要求 16 字节对齐的 16 位缓冲。

use core::ffi::c_void;

use esp_idf_sys as sys;

pub struct Aec {
    handle: *mut sys::aec_handle_t,
    frame: usize,
    input: *mut i16,
    reference: *mut i16,
    output: *mut i16,
}

impl Aec {
    /// 创建帧长与 I2S 块一致的 AEC 实例。
    /// 依次尝试与旧 C 垫片相同的（filter_length, mode）组合。
    pub fn new(frame_size: usize) -> Option<Self> {
        unsafe {
            // 2.6.x：优先用基于 config 的构造器；旧版 aec_create() 路径
            // 在这版固件上会让 esp_aec3_hps16fft_process() 死循环。
            let mut config = sys::aec_config_t {
                mic_num: 1,
                ref_num: 1,
                out_num: 1,
                filter_length: 4,
                sample_rate: 16000,
                caps: sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT,
                mode: sys::aec_mode_t_AEC_MODE_SR_LOW_COST,
                nlp_level: sys::aec_nlp_level_t_AEC_NLP_LEVEL_AGGR,
            };
            let handle = sys::aec_create_from_config(&mut config);
            if handle.is_null() {
                return None;
            }
            let n = sys::aec_get_chunksize(handle) as usize;
            if n != frame_size {
                sys::aec_destroy(handle);
                return None;
            }

            let bytes = frame_size * 2;
            let input = sys::heap_caps_aligned_alloc(16, bytes, sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT) as *mut i16;
            let reference = sys::heap_caps_aligned_alloc(16, bytes, sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT) as *mut i16;
            let output = sys::heap_caps_aligned_alloc(16, bytes, sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT) as *mut i16;

            if input.is_null() || reference.is_null() || output.is_null() {
                sys::heap_caps_free(input as *mut c_void);
                sys::heap_caps_free(reference as *mut c_void);
                sys::heap_caps_free(output as *mut c_void);
                sys::aec_destroy(handle);
                return None;
            }

            Some(Self { handle, frame: frame_size, input, reference, output })
        }
    }
    /// 在 `mic` 中消除 `reference`（喇叭正在播放的内容）的回声，
    /// 把近端信号写入 `out`。
    pub fn process(&mut self, mic: &[i16], reference: &[i16], out: &mut [i16]) {
        if mic.len() < self.frame || reference.len() < self.frame || out.len() < self.frame {
            return;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(mic.as_ptr(), self.input, self.frame);
            core::ptr::copy_nonoverlapping(reference.as_ptr(), self.reference, self.frame);
            sys::aec_process(self.handle, self.input, self.reference, self.output);
            core::ptr::copy_nonoverlapping(self.output, out.as_mut_ptr(), self.frame);
        }
    }

    pub fn frame_size(&self) -> usize {
        self.frame
    }
}

impl Drop for Aec {
    fn drop(&mut self) {
        unsafe {
            sys::aec_destroy(self.handle);
            sys::heap_caps_free(self.input as *mut c_void);
            sys::heap_caps_free(self.reference as *mut c_void);
            sys::heap_caps_free(self.output as *mut c_void);
        }
    }
}
