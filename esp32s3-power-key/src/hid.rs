//! 键鼠用的两个 HID 报告描述符（从 esp32s3-hid-kvm 移植）。
//!
//! 电源键原来只有一个 interface（键盘 + 系统控制，带 Report ID 1/2），
//! 这里只补 **键盘之外** 的两个独立 interface：
//!
//!   ② 鼠标 boot（4 字节，相对位移 + 滚轮）
//!   ③ Consumer Control（2 字节，音量 / 播放暂停等媒体键）
//!
//! 键盘不新建 —— 直接复用原来那个 boot 6KRO 接口（Report ID 1），
//! 这样 Windows 里不会多出一堆"键盘"设备，电源唤醒相关的设置也不用重配。
//!
//! 所以整个设备一共 3 个 interface（原 1 + 鼠标 + 媒体键），
//! 仍在 embassy-usb 的 `max-interface-count-4` 之内，无需改 feature。

/// 鼠标报告长度：按键 + X + Y + 滚轮
pub const MOUSE_REPORT_LEN: usize = 4;
/// Consumer Control 报告长度：16 位 usage
pub const CONSUMER_REPORT_LEN: usize = 2;

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
    0x15, 0x00, //     Logical Min 0
    0x25, 0x01, //     Logical Max 1
    0x75, 0x01, //     Report Size 1
    0x95, 0x05, //     Report Count 5
    0x81, 0x02, //     Input (Data,Var,Abs) -> 5 个按键位
    0x75, 0x03, //     Report Size 3
    0x95, 0x01, //     Report Count 1
    0x81, 0x03, //     Input (Cnst)         -> 补齐 3 位
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Min -127
    0x25, 0x7F, //     Logical Max 127
    0x75, 0x08, //     Report Size 8
    0x95, 0x03, //     Report Count 3
    0x81, 0x06, //     Input (Data,Var,Rel) -> X/Y/Wheel
    0xC0, //   End Collection
    0xC0, // End Collection
];

/// Consumer Control：2 字节，16 位 usage（Usage Page 0x0C）
///
/// 常见的几个：0xE2 静音 / 0xE9 音量+ / 0xEA 音量− / 0xCD 播放暂停 /
/// 0xB5 下一曲 / 0xB6 上一曲 / 0xB7 停止 / 0x006F 亮度+ / 0x0070 亮度−
///
/// 这些键**不在**标准键盘 usage page（0x07）里，混在键盘描述符里发不出去，
/// 必须单独开一个 interface。
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
