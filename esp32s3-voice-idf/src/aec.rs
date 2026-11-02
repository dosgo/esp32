//! Safe wrapper over the official esp-sr AEC C API (`esp_aec.h`).
//!
//! Everything here is Rust: the handle and the aligned buffers are allocated
//! through `heap_caps_aligned_alloc()` because `aec_process()` requires 16-byte
//! aligned 16-bit buffers.

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
    /// Create an AEC instance whose frame size matches the I2S chunk.
    /// Tries the same (filter_length, mode) candidates as the old C shim.
    pub fn new(frame_size: usize) -> Option<Self> {
        unsafe {
            // 2.6.x: prefer the config-based constructor; the legacy
            // aec_create() path made esp_aec3_hps16fft_process() spin forever
            // on this build.
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
    /// Cancel the echo of `reference` (what the speaker is playing) in `mic`
    /// and write the near-end signal to `out`.
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
