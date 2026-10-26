//! 板级配置 —— **换板子只改这个文件**
//!
//! 这块板子不是乐鑫官方板，卖家没有给原理图时只能靠实测。
//! 下面这套默认值是按「1.54 寸屏 + ES8311 + NS4150B + QMI8658」这一类通用
//! AI 语音板最常见的接法填的，**到货后请务必按下面第 3 节的方法逐项核对**。
//!
//! # 1. 引脚表
//!
//! | 功能 | GPIO | 说明 |
//! |------|------|------|
//! | I2S MCLK  | 16 | ES8311 主时钟，必须是 256×Fs = 4.096MHz |
//! | I2S BCLK  | 15 | 位时钟 |
//! | I2S WS    | 14 | 左右声道选择（LRCK） |
//! | I2S DIN   | 13 | **codec → ESP**，即麦克风数据 |
//! | I2S DOUT  | 12 | **ESP → codec**，即播放数据 |
//! | PA_EN     | 7  | NS4150B 使能，高电平开功放 |
//! | I2C SDA   | 4  | ES8311 / QMI8658 共用 |
//! | I2C SCL   | 5  | |
//! | LCD SCLK  | 21 | ST7789 SPI 时钟 |
//! | LCD MOSI  | 47 | |
//! | LCD CS    | 41 | |
//! | LCD DC    | 40 | 数据/命令选择 |
//! | LCD RST   | 39 | |
//! | LCD BL    | 42 | 背光，高电平亮 |
//! | KEY_PLUS  | 0  | 板载 BOOT 键（低电平按下） |
//!
//! # 2. 不可用的引脚
//!
//! - GPIO 26~32：S3R8 的 OPI PSRAM 占用，碰了直接死机
//! - GPIO 19/20：USB D-/D+
//! - GPIO 43/44：UART0（日志口）
//! - GPIO 33~37：部分封装里给 PSRAM，一并不用

#![allow(dead_code)]
//!
//! # 3. 怎么核对（到货后照做，5 分钟）
//!
//! 1. 烧 `cargo run --release`，串口看 `[i2c] 扫描` 那行。
//!    能扫到 `0x18` 说明 SDA/SCL 对了（ES8311 的固定地址）。
//! 2. 扫不到就改 `PIN_I2C_SDA` / `PIN_I2C_SCL` 重烧，直到扫到 0x18 和 0x6B(QMI8658)。
//! 3. I2S 四根线：先只接 BCLK/WS/MCLK 和 DOUT，播一段测试音，
//!    有声音说明 DOUT 对；没声音就把 DOUT 换成 MCU 侧其它候选脚再试。
//! 4. 麦克风：录 1 秒回放，波形是平的说明 DIN 接错或 ES8311 的 ADC 没开。

// PIN_KEY_PLUS / QMI8658_ADDR 是留给后续扩展的（按键唤醒、姿态检测），先允许未使用
#![allow(dead_code)]

// ---------------- I2S / 音频 ----------------
pub const PIN_I2S_MCLK: u8 = 16;
pub const PIN_I2S_BCLK: u8 = 15;
pub const PIN_I2S_WS: u8 = 14;
pub const PIN_I2S_DIN: u8 = 13; // codec -> MCU（麦克风）
pub const PIN_I2S_DOUT: u8 = 12; // MCU -> codec（播放）
pub const PIN_PA_EN: u8 = 7;

// ---------------- I2C ----------------
pub const PIN_I2C_SDA: u8 = 4;
pub const PIN_I2C_SCL: u8 = 5;

/// ES8311 的 I2C 地址（7 位），硬件固定
pub const ES8311_ADDR: u8 = 0x18;
/// QMI8658 IMU 的 I2C 地址（7 位），SA0 接高时是 0x6B
pub const QMI8658_ADDR: u8 = 0x6B;

// ---------------- LCD ----------------
pub const PIN_LCD_SCLK: u8 = 21;
pub const PIN_LCD_MOSI: u8 = 47;
pub const PIN_LCD_CS: u8 = 41;
pub const PIN_LCD_DC: u8 = 40;
pub const PIN_LCD_RST: u8 = 39;
pub const PIN_LCD_BL: u8 = 42;

pub const LCD_WIDTH: u16 = 240;
pub const LCD_HEIGHT: u16 = 240;

// ---------------- 按键 ----------------
pub const PIN_KEY_PLUS: u8 = 0;

// ---------------- 音频参数 ----------------
/// 采样率。16000 是 ASR 的通用输入，别乱改
pub const SAMPLE_RATE: u32 = 16_000;
/// 一帧的时长（毫秒）。60ms = 960 个采样点 = 1920 字节
pub const FRAME_MS: u32 = 60;
/// 一帧的采样点数（单声道）
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;
/// 一帧的字节数（i16 单声道）—— 这是上传/下行的 PCM 长度
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
/// 一帧在 I2S 总线上的字节数（立体声，左右各一份）
pub const I2S_FRAME_BYTES: usize = FRAME_SAMPLES * 4;
