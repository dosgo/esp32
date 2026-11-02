//! Safe wrapper over the official esp-sr VAD (WebRTC) C API.
//!
//! The bindings come from `src/esp_sr_bindings.h` -> `esp_vad.h`, so no C code
//! of the component is modified.

use esp_idf_sys as sys;

pub struct Vad {
    handle: sys::vad_handle_t,
}

impl Vad {
    /// `VAD_MODE_2` = very aggressive; 16 kHz; 30 ms frames (the VAD only
    /// accepts 10/20/30 ms); 96 ms of speech before reporting speech and
    /// 450 ms of noise before reporting silence back.
    pub fn new() -> Result<Self, String> {
        let handle = unsafe {
            sys::vad_create_with_param(sys::vad_mode_t_VAD_MODE_2, 16000, 30, 96, 450)
        };
        if handle.is_null() {
            Err("vad_create_with_param() returned NULL".into())
        } else {
            Ok(Self { handle })
        }
    }

    /// Feed one I2S chunk (512 samples = 32 ms). The VAD consumes the first
    /// 480 samples (30 ms) of the buffer.
    pub fn speech(&mut self, pcm: &[i16]) -> bool {
        debug_assert!(pcm.len() >= 480);
        unsafe {
            sys::vad_process_with_trigger(self.handle, pcm.as_ptr() as *mut i16)
                == sys::vad_state_t_VAD_SPEECH
        }
    }

    /// Reset the internal trigger so the next utterance starts from silence.
    pub fn reset(&mut self) {
        unsafe { sys::vad_reset_trigger(self.handle) };
    }
}

impl Drop for Vad {
    fn drop(&mut self) {
        unsafe { sys::vad_destroy(self.handle) };
    }
}
