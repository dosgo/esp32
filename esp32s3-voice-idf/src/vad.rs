//! Safe wrapper over the official esp-sr VAD (WebRTC) C API.
//!
//! The VAD only accepts 10/20/30 ms frames, while the I2S block is 32 ms
//! (512 samples). Feeding it the first 480 samples of every block drops 6% of
//! the signal and makes the detector drift, so the samples are buffered here
//! and consumed as a continuous 30 ms stream.

use esp_idf_sys as sys;

const FRAME: usize = 480; // 30 ms at 16 kHz

pub struct Vad {
    handle: sys::vad_handle_t,
    pending: Vec<i16>,
}

impl Vad {
    /// `VAD_MODE_1` (balanced) - mode 2 was too eager to call speech "noise" in
    /// a noisy room. 96 ms of speech before reporting speech, 450 ms of noise
    /// before reporting silence again.
    pub fn new() -> Result<Self, String> {
        let handle = unsafe {
            sys::vad_create_with_param(sys::vad_mode_t_VAD_MODE_2, 16000, 30, 96, 450)
        };
        if handle.is_null() {
            Err("vad_create_with_param() returned NULL".into())
        } else {
            Ok(Self {
                handle,
                pending: Vec::with_capacity(FRAME * 2),
            })
        }
    }

    /// Feed one I2S chunk (512 samples). Returns true if any 30 ms sub-frame in
    /// it was classified as speech.
    pub fn speech(&mut self, pcm: &[i16]) -> bool {
        self.pending.extend_from_slice(pcm);
        let mut voiced = false;
        while self.pending.len() >= FRAME {
            let mut frame = [0i16; FRAME];
            frame.copy_from_slice(&self.pending[..FRAME]);
            self.pending.drain(..FRAME);
            let state = unsafe {
                sys::vad_process_with_trigger(self.handle, frame.as_mut_ptr())
            };
            if state == sys::vad_state_t_VAD_SPEECH {
                voiced = true;
            }
        }
        voiced
    }

    /// Reset the internal trigger so the next utterance starts from silence.
    pub fn reset(&mut self) {
        self.pending.clear();
        unsafe { sys::vad_reset_trigger(self.handle) };
    }
}

impl Drop for Vad {
    fn drop(&mut self) {
        unsafe { sys::vad_destroy(self.handle) };
    }
}
