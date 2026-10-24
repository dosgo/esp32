//! USB HID 报告描述符。
//!
//! 一共四个 interface，各自独立（不带 Report ID）：
//!   1. 键盘 NKRO（bitmap，任意多键同按）
//!   2. 键盘 Boot 6KRO（BIOS 可用）
//!   3. 鼠标 Boot（相对位移）
//!   4. Consumer Control（音量 / 播放暂停等媒体键）
//!
//! 为什么分成多个 interface 而不是一个复合设备带 Report ID：
//! boot protocol（BIOS / 无驱动环境）只认「单报告、无 Report ID」的设备。
//! 分成独立 interface 后，BIOS 里键盘和鼠标都能直接工作，这是 KVM 场景的硬需求。

/// NKRO 键盘报告长度：1 字节修饰键 + 19 字节 bitmap = 20 字节
pub const NKRO_REPORT_LEN: usize = 20;
/// Boot 键盘报告长度（标准 8 字节）
pub const BOOT_REPORT_LEN: usize = 8;
/// 鼠标报告长度：按键 + X + Y + 滚轮
#[allow(dead_code)]
pub const MOUSE_REPORT_LEN: usize = 4;
/// Consumer Control 报告长度：16 位 usage
#[allow(dead_code)]
pub const CONSUMER_REPORT_LEN: usize = 2;

// ---------------------------------------------------------------------------
// NKRO 键盘（bitmap，全键无冲）
// ---------------------------------------------------------------------------

/// NKRO 键盘：20 字节
///   byte0      = 修饰键位图（bit0 LCtrl … bit7 RGUI）
///   byte1..19  = 152 位 bitmap，bit n 对应 usage n（覆盖 usage 0x00–0x97）
///
/// bitmap 方案下「同时按多少个键」没有上限，6KRO 的「第 7 个键丢失」问题彻底消失。
/// 代价：boot protocol 下主机只认 8 字节报告，所以 NKRO 描述符在 BIOS 里不可用
/// —— 这就是为什么下面还保留了一个独立的 boot 6KRO 接口。
pub const NKRO_DESC: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0xE0, //   Usage Min (224) LeftControl
    0x29, 0xE7, //   Usage Max (231) RightGUI
    0x15, 0x00, //   Logical Min 0
    0x25, 0x01, //   Logical Max 1
    0x75, 0x01, //   Report Size 1
    0x95, 0x08, //   Report Count 8
    0x81, 0x02, //   Input (Data,Var,Abs)  -> 修饰键 1 字节
    0x19, 0x00, //   Usage Min (0)
    0x29, 0x97, //   Usage Max (151)
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x01, //   Report Size 1
    0x95, 0x98, //   Report Count 152
    0x81, 0x02, //   Input (Data,Var,Abs)  -> bitmap 19 字节
    0x05, 0x08, //   Usage Page (LEDs)
    0x19, 0x01, //   Usage Min 1
    0x29, 0x05, //   Usage Max 5
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x01,
    0x95, 0x05,
    0x91, 0x02, //   Output (Data,Var,Abs) -> LED 5 位
    0x75, 0x03,
    0x95, 0x01,
    0x91, 0x03, //   Output (Cnst)         -> 补齐 3 位
    0xC0, // End Collection
];

// ---------------------------------------------------------------------------
// Boot 键盘（6KRO，BIOS 用）
// ---------------------------------------------------------------------------

/// 键盘（boot protocol）：8 字节
///   byte0 = 修饰键位图（bit0 LCtrl … bit7 RGUI）
///   byte1 = 保留
///   byte2..7 = 最多 6 个按键 usage（6KRO）
///
/// 只在「BIOS 模式」下使用。日常走 NKRO 接口。
pub const KBD_DESC: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0xE0, //   Usage Min (224) LeftControl
    0x29, 0xE7, //   Usage Max (231) RightGUI
    0x15, 0x00, //   Logical Min 0
    0x25, 0x01, //   Logical Max 1
    0x75, 0x01, //   Report Size 1
    0x95, 0x08, //   Report Count 8
    0x81, 0x02, //   Input (Data,Var,Abs)  -> 修饰键
    0x75, 0x08, //   Report Size 8
    0x95, 0x01, //   Report Count 1
    0x81, 0x03, //   Input (Cnst,Var,Abs)  -> 保留字节
    0x19, 0x00, //   Usage Min 0
    0x29, 0x65, //   Usage Max 101
    0x15, 0x00, //   Logical Min 0
    0x25, 0x65, //   Logical Max 101
    0x75, 0x08, //   Report Size 8
    0x95, 0x06, //   Report Count 6
    0x81, 0x00, //   Input (Data,Ary,Abs)  -> 6 键
    0x05, 0x08, //   Usage Page (LEDs)
    0x19, 0x01, //   Usage Min 1
    0x29, 0x03, //   Usage Max 3
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x01,
    0x95, 0x03,
    0x91, 0x02, //   Output (Data,Var,Abs) -> LED 回传
    0x75, 0x05,
    0x95, 0x01,
    0x91, 0x03, //   Output (Cnst)         -> 补齐 1 字节
    0xC0, // End Collection
];

/// 鼠标（boot protocol）：4 字节
///   byte0 = 按键位图（bit0 左键 / bit1 右键 / bit2 中键 / bit3、4 侧键）
///   byte1 = X 相对位移（有符号）
///   byte2 = Y 相对位移（有符号）
///   byte3 = 滚轮（有符号）
///
/// 标准 boot mouse 只有前 3 字节；第 4 字节的滚轮在 boot protocol 下会被 BIOS 忽略，
/// 但进系统后（走报告描述符模式）正常工作，鼠标移动不受影响。
pub const MOUSE_DESC: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Buttons)
    0x19, 0x01, //     Usage Min 1
    0x29, 0x05, //     Usage Max 5
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x01,
    0x95, 0x05, //     Report Count 5
    0x81, 0x02, //     Input (Data,Var,Abs) -> 5 个按键位
    0x75, 0x03,
    0x95, 0x01,
    0x81, 0x03, //     Input (Cnst)         -> 补齐 3 位
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Min -127
    0x25, 0x7F, //     Logical Max 127
    0x75, 0x08,
    0x95, 0x03,
    0x81, 0x06, //     Input (Data,Var,Rel) -> X/Y/Wheel
    0xC0, //   End Collection
    0xC0, // End Collection
];

// ---------------------------------------------------------------------------
// Consumer Control（媒体键）
// ---------------------------------------------------------------------------

/// Consumer Control：2 字节，16 位 usage（Usage Page 0x0C）
///
/// 常见的几个：0xE2 静音 / 0xE9 音量+ / 0xEA 音量− / 0xCD 播放暂停 /
/// 0xB5 下一曲 / 0xB6 上一曲 / 0xB7 停止 / 0x006F 亮度+ / 0x0070 亮度−
///
/// 这些键**不在**标准键盘 usage page（0x07）里，必须单独开一个 interface，
/// 否则音量键之类的完全发不出去 —— 这是「全键盘」最容易漏掉的一块。
pub const CONSUMER_DESC: &[u8] = &[
    0x05, 0x0C, // Usage Page (Consumer)
    0x09, 0x01, // Usage (Consumer Control)
    0xA1, 0x01, // Collection (Application)
    0x75, 0x10, //   Report Size 16
    0x95, 0x01, //   Report Count 1
    0x15, 0x00, //   Logical Min 0
    0x26, 0xFF, 0x03, //   Logical Max 1023
    0x19, 0x00, //   Usage Min 0
    0x2A, 0xFF, 0x03, //   Usage Max 1023
    0x81, 0x00, //   Input (Data,Ary,Abs)
    0xC0, // End Collection
];
