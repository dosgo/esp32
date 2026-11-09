//! 官方 esp-sr VAD（WebRTC）C API 的安全封装。
//!
//! VAD 只接受 10/20/30 毫秒的帧，而 I2S 块是 32 毫秒（512 样本）。
//! 每块只取前 480 个样本会丢掉 6% 的信号并让检测器漂移，
//! 所以在这里把样本攒起来，按连续的 30 毫秒流喂给 VAD。

use esp_idf_sys as sys;

const FRAME: usize = 480; // 16 kHz 下 30 毫秒

pub struct Vad {
    handle: sys::vad_handle_t,
    pending: Vec<i16>,
}

impl Vad {
    /// `VAD_MODE_2`，96 毫秒语音起始确认、450 毫秒静音拖尾。
    pub fn new() -> Result<Self, String> {
        let handle =
            unsafe { sys::vad_create_with_param(sys::vad_mode_t_VAD_MODE_2, 16000, 30, 96, 450) };
        if handle.is_null() {
            Err("vad_create_with_param() returned NULL".into())
        } else {
            Ok(Self {
                handle,
                pending: Vec::with_capacity(FRAME * 2),
            })
        }
    }

    /// 送入一个 I2S 块（512 样本）。其中任何 30 毫秒子帧被判为
    /// 语音时返回 true。
    pub fn speech(&mut self, pcm: &[i16]) -> bool {
        self.pending.extend_from_slice(pcm);
        let mut voiced = false;
        while self.pending.len() >= FRAME {
            let mut frame = [0i16; FRAME];
            frame.copy_from_slice(&self.pending[..FRAME]);
            self.pending.drain(..FRAME);
            let state = unsafe { sys::vad_process_with_trigger(self.handle, frame.as_mut_ptr()) };
            if state == sys::vad_state_t_VAD_SPEECH {
                voiced = true;
            }
        }
        voiced
    }

    /// 复位内部触发器，让下一句话从静音状态开始判。
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
