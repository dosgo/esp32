//! 预录的本地提示音（piper，16 kHz 单声道 s16le），编译进固件，
//! 断网时设备也能开口说话。

pub const HERE: &[u8] = include_bytes!("../assets/p_here.raw");
pub const AGAIN: &[u8] = include_bytes!("../assets/p_again.raw");
pub const THINKING: &[u8] = include_bytes!("../assets/p_thinking.raw");
pub const OK: &[u8] = include_bytes!("../assets/p_ok.raw");
pub const NET: &[u8] = include_bytes!("../assets/p_net.raw");
pub const NO_WIFI: &[u8] = include_bytes!("../assets/no_wifi.raw");
pub const NO_GATEWAY: &[u8] = include_bytes!("../assets/no_gateway.raw");
