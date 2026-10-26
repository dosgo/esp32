//! 运行配置 —— 烧固件前先改这里

/// 家里的 Wi-Fi 名称（2.4GHz，ESP32-S3 不支持 5GHz）
pub const WIFI_SSID: &str = "YOUR_WIFI_SSID";
/// Wi-Fi 密码
pub const WIFI_PASSWORD: &str = "YOUR_WIFI_PASSWORD";

/// 语音网关的地址。
///
/// 网关跑在你的电脑上（`server/gateway.py`），所以要填电脑的局域网 IP。
/// 电脑上 `ipconfig` 看 IPv4 地址，比如 192.168.88.160。
/// ESP32 和电脑必须在同一个网段，否则连不上。
pub const GATEWAY_HOST: &str = "192.168.88.160";
pub const GATEWAY_PORT: u16 = 8765;

/// 设备名，会在网关日志里显示
pub const DEVICE_NAME: &str = "esp32s3-voice";
