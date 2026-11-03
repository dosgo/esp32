//! Pre-recorded local prompts (piper, 16 kHz mono s16le) compiled into the
//! firmware so the device can still talk when the network is down.

pub const HERE: &[u8] = include_bytes!("../assets/p_here.raw");
pub const AGAIN: &[u8] = include_bytes!("../assets/p_again.raw");
pub const THINKING: &[u8] = include_bytes!("../assets/p_thinking.raw");
pub const OK: &[u8] = include_bytes!("../assets/p_ok.raw");
pub const NET: &[u8] = include_bytes!("../assets/p_net.raw");
