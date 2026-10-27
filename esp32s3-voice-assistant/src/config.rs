//! 编译期常量 —— 只有设备名还留在这里
//!
//! **WiFi 账号密码、网关地址、AI/语音的 key 已经全部移到 flash 里了**
//! （见 `store.rs`），通过网页配置，不用再改代码重烧。
//!
//! 配网步骤：
//!   1. 手机连热点 `ESP32-VOICE`（密码 `voice1234`）
//!   2. 浏览器打开 http://192.168.4.1
//!   3. 填 WiFi、网关 IP、API key，保存
//!
//! 想知道网关 IP：在跑 `gateway.py` 的电脑上 `ipconfig` 看 IPv4 地址。

/// 设备名，会在网关日志和 WebSocket 握手里出现
pub const DEVICE_NAME: &str = "esp32s3-voice";
