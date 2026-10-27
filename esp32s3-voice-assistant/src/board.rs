//! 板级配置 —— **换板子只改这个文件**
//!
//! 本文件的引脚已按**卖家提供的 GPIO 表**逐项落定（2026-10-27）。
//! 在此之前的版本是按「这类通用 AI 语音板最常见的接法」猜的，几乎全错，
//! 如果你看到过旧表，请以本文件为准。
//!
//! # 1. 引脚表（数据来源：卖家 GPIO 表）
//!
//! | 功能 | GPIO | 说明 |
//! |------|------|------|
//! | I2S MCLK  | 8  | ES8311 主时钟，必须是 256×Fs = 4.096MHz |
//! | I2S SCLK  | 9  | 位时钟（BCLK） |
//! | I2S LRCK  | 10 | 左右声道选择（WS） |
//! | I2S ASDOUT| 11 | **codec ADC 输出 → ESP 输入**，即麦克风数据（DIN） |
//! | I2S DSDIN | 12 | **ESP 输出 → codec DAC 输入**，即播放数据（DOUT） |
//! | PA CTRL   | 7  | NS4150B 功放使能，高电平开功放 |
//! | CODEC SDA | 42 | ES8311 控制口，和 IMU / 触摸**共用一条 I2C** |
//! | CODEC SCL | 41 | ↑ |
//! | LCD CS    | 21 | |
//! | LCD CLK   | 38 | |
//! | LCD DIN   | 39 | 单向 MOSI，屏没有 MISO |
//! | LCD DC    | 45 | 数据/命令选择（★ strapping 脚） |
//! | LCD RST   | 40 | |
//! | LCD BL    | 46 | 背光，高电平亮（★ strapping 脚） |
//! | TP SDA    | 42 | 触摸，与 codec/IMU 共用 I2C |
//! | TP SCL    | 41 | ↑ |
//! | TP RST    | 47 | |
//! | TP INT    | 48 | |
//! | IMU SDA   | 42 | QMI8658，与 codec/触摸共用 I2C |
//! | IMU SCL   | 41 | ↑ |
//! | IMU INT   | 6  | |
//! | KEY MINUS | 0  | ★ strapping 脚，上电时按住会进下载模式 |
//! | KEY PLUS  | 4  | |
//! | KEY PWR   | 5  | 电源键，通常兼唤醒 |
//! | BAT ADC   | 1  | 电池电压采样（ADC1_CH0） |
//! | BAT EN    | 2  | 电池供电使能 |
//! | CHG STAT  | 3  | 充电状态输入（★ strapping 脚） |
//! | SD D0~D3  | 17 / 18 / 13 / 14 | SDIO 4 位 |
//! | SD CMD    | 15 | |
//! | SD CLK    | 16 | |
//! | USB N/P   | 19 / 20 | 原生 USB |
//! | ESP TXD   | 43 | UART0 日志口 |
//! | ESP RXD   | 44 | |
//!
//! # 2. 和旧版（猜的）相比改了哪些
//!
//! 全改了，一个没留：
//!
//! - I2S 从 12~16 搬到 8~12。旧值 13~16 **正好压在 SD 卡 D0~D3 上**，
//!   真跑起来音频会和 TF 卡抢总线。
//! - I2C 从 4/5 搬到 41/42。旧值 4/5 在新板上是空的。
//! - LCD 六根线全部换位（21/38/39/40/45/46）。
//! - KEY_PLUS 从 0 改成 4；0 其实是 **KEY MINUS**。
//!
//! # 3. 一条 I2C 挂三个设备
//!
//! codec / IMU / 触摸的 SDA、SCL 都是 42/41，物理上就是同一组上拉电阻，
//! 不是画错。总线上预期能扫到：
//!
//! - `0x18` ES8311（固定）
//! - `0x6B` 或 `0x6A` QMI8658（看 SA0 接法）
//! - `0x15` / `0x38` / `0x63` 触摸（CST816 / FT6336 / AXS5106 之一，待实测）
//!
//! 所以扫描结果里有 2~3 个地址是正常的，扫不到 0x18 才说明线接错了。
//!
//! # 4. 不可用的引脚
//!
//! - GPIO 26~32：S3R8 的 OPI PSRAM 占用，碰了直接死机
//! - GPIO 33~37：部分封装给 PSRAM，一并不用
//! - GPIO 19/20：USB D-/D+（做 USB host 时才改作他用）
//! - GPIO 43/44：UART0（日志口）
//!
//! # 5. strapping 脚提醒（不是不能用，是上电瞬间别乱拉）
//!
//! - **IO0**（KEY MINUS）：低电平 = 下载模式。所以上电/复位瞬间别按 MINUS，
//!   否则板子不进固件。这也是绝大多数开发板 BOOT 键的设计，属于正常。
//! - **IO45**（LCD DC）：决定 VDD_SPI 电压（1.8V/3.3V）。做 GPIO 输出没问题，
//!   但**不要在 `init` 之前把它外部拉高**，否则 Flash 供电档位切错会起不来。
//! - **IO46**（LCD BL）：ROM 启动打印使能。外部上拉只影响启动时串口多吐日志，
//!   不影响启动，可忽略。
//! - **IO3**（CHG STAT）：JTAG 信号源选择，做成输入读取没有任何问题。
//!
//! # 6. 到货后怎么核对（5 分钟）
//!
//! 1. 烧 `cargo run --release`，串口看 `[i2c]` 那行，出现 `0x18` 就说明 I2C 对了。
//! 2. 屏幕亮但白屏/花屏 → DC(45)/RST(40) 之一接反，或屏不是 ST7789（见 display.rs）。
//! 3. 喇叭有底噪但没声 → PA CTRL(7) 没拉高；完全没声 → DOUT(12)/BCLK(9) 有问题。
//! 4. 录 1 秒回放波形是平的 → DIN(11) 错，或 codec ADC 没开。

#![allow(dead_code)]

// ---------------- I2S / 音频 ----------------
pub const PIN_I2S_MCLK: u8 = 8;
pub const PIN_I2S_BCLK: u8 = 9; // I2S SCLK
pub const PIN_I2S_WS: u8 = 10; // I2S LRCK
pub const PIN_I2S_DIN: u8 = 11; // I2S ASDOUT：codec ADC -> MCU（麦克风）
pub const PIN_I2S_DOUT: u8 = 12; // I2S DSDIN：MCU -> codec DAC（播放）
pub const PIN_PA_EN: u8 = 7; // PA CTRL，高电平开功放

// ---------------- I2C（codec + IMU + 触摸 共用）----------------
pub const PIN_I2C_SDA: u8 = 42;
pub const PIN_I2C_SCL: u8 = 41;

/// ES8311 的 I2C 地址（7 位），硬件固定
pub const ES8311_ADDR: u8 = 0x18;
/// QMI8658 IMU 的 I2C 地址（7 位）：SA0 接高 0x6B，接低 0x6A
pub const QMI8658_ADDR: u8 = 0x6B;

// ---------------- LCD ----------------
pub const PIN_LCD_SCLK: u8 = 38; // LCD CLK
pub const PIN_LCD_MOSI: u8 = 39; // LCD DIN（单向）
pub const PIN_LCD_CS: u8 = 21; // LCD CS
pub const PIN_LCD_DC: u8 = 45; // LCD DC（strapping）
pub const PIN_LCD_RST: u8 = 40; // LCD RST
pub const PIN_LCD_BL: u8 = 46; // LCD BL（strapping）

// ---------------- 触摸（暂未启用）----------------
pub const PIN_TP_SCL: u8 = 41;
pub const PIN_TP_SDA: u8 = 42;
pub const PIN_TP_RST: u8 = 47;
pub const PIN_TP_INT: u8 = 48;

// ---------------- IMU（暂未启用）----------------
pub const PIN_IMU_SCL: u8 = 41;
pub const PIN_IMU_SDA: u8 = 42;
pub const PIN_IMU_INT: u8 = 6;

// ---------------- 按键 ----------------
pub const PIN_KEY_PLUS: u8 = 4;
pub const PIN_KEY_MINUS: u8 = 0; // strapping：按住上电会进下载模式
pub const PIN_KEY_PWR: u8 = 5;

// ---------------- 电源管理（暂未启用）----------------
pub const PIN_BAT_ADC: u8 = 1; // ADC1_CH0，读电池电压
pub const PIN_BAT_EN: u8 = 2; // 电池供电使能
pub const PIN_CHG_STAT: u8 = 3; // 充电状态输入

// ---------------- TF 卡（暂未启用，SDIO 4 位）----------------
pub const PIN_SD_D0: u8 = 17;
pub const PIN_SD_D1: u8 = 18;
pub const PIN_SD_D2: u8 = 13;
pub const PIN_SD_D3: u8 = 14;
pub const PIN_SD_CMD: u8 = 15;
pub const PIN_SD_CLK: u8 = 16;

// ---------------- 屏参数（★ 卖家表里没给，待确认）----------------
// 资料第二张图是 4 线 SPI + 16bit(RGB565) 经 LUT 映射到 18bit 帧存的时序，
// 只能确认「SPI 接口、65K 色」。驱动 IC 和分辨率需要你确认后改这里。
pub const LCD_WIDTH: u16 = 240;
pub const LCD_HEIGHT: u16 = 240;

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
