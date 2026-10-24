//! 0.96" ST7735S IPS 80x160 屏（Pocket Dongle S3 / LilyGO T-Dongle-S3 同款接线）
//!
//! 只做一件事：把热点账号、联网状态、Blinker 状态显示出来。
//! 没有串口的时候，这块屏就是最直观的"仪表盘"。
//!
//! 接线（**不是** LilyGO T-Dongle-S3，这块板接线完全不同）：
//!   来源：github.com/beastboost/jcorp-nomad/blob/main/docs/PocketDongleS3.md
//!         （引脚表来自 GNPE 提供的原理图）
//!   SCL / SCLK = GPIO10
//!   SDA / MOSI = GPIO11
//!   CS         = GPIO12
//!   DC / RS    = GPIO13
//!   RST / RES  = GPIO14
//!   背光：原理图里没有背光脚，硬连常亮（所以上电就是亮的，不归软件管）
//!
//! 顺便记一下这块板的其它已知接线（都在这份原理图里）：
//!   BOOT 键 = GPIO0
//!   TF 卡   = 独立 SPI：SCLK 17 / MOSI 18 / MISO 16 / CS 47
//!   没有 RGB LED
//!   GPIO33~37 被八线 PSRAM 永久占用，任何设计都不能用

use core::fmt::Write as _;

use embassy_time::{Duration, Timer};
use embedded_graphics::{
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_5X8, FONT_6X10},
    },
    pixelcolor::Rgb565,
    prelude::*,
    text::{Baseline, Text},
};
use esp_hal::{
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
};
use heapless::String;
use mipidsi::{
    Builder,
    models::ST7735s,
    options::{ColorInversion, ColorOrder},
};

/// 屏幕物理尺寸（竖屏）
const W: u16 = 80;
const H: u16 = 160;
/// ST7735 的显存是 132x162，80x160 的面板要偏置一下，否则画面会错位
const OFF_X: u16 = 26;
const OFF_Y: u16 = 1;

/// mipidsi 的 SPI 接口要一块缓冲区（DC 切换、批量写像素时用）。
/// 必须 static：Display 的生命周期会一直活到任务结束。
static SPI_BUF: static_cell::StaticCell<[u8; 512]> = static_cell::StaticCell::new();

const BG: Rgb565 = Rgb565::new(3, 6, 4); // 深蓝灰，比纯黑好看一点
const FG: Rgb565 = Rgb565::WHITE;
const DIM: Rgb565 = Rgb565::new(14, 32, 20);
const OK: Rgb565 = Rgb565::new(10, 50, 12);
const BAD: Rgb565 = Rgb565::new(28, 8, 8);

#[embassy_executor::task]
pub async fn lcd_task(
    spi_peri: esp_hal::peripherals::SPI2<'static>,
    sck: esp_hal::peripherals::GPIO10<'static>,
    mosi: esp_hal::peripherals::GPIO11<'static>,
    cs: esp_hal::peripherals::GPIO12<'static>,
    dc: esp_hal::peripherals::GPIO13<'static>,
    rst: esp_hal::peripherals::GPIO14<'static>,
) {
    let mut delay = Delay::new();

    let spi = match Spi::new(
        spi_peri,
        SpiConfig::default()
            // 官方在这块板上用 27MHz 实测正常；这里取 26MHz（同一档），
            // 1 秒刷新一次的界面完全够用。
            .with_frequency(Rate::from_mhz(26))
            .with_mode(Mode::_0),
    ) {
        Ok(s) => s.with_sck(sck).with_mosi(mosi),
        Err(_) => {
            crate::push_log(format_args!("[lcd] SPI 配置失败"));
            return;
        }
    };

    let cs_pin = Output::new(cs, Level::High, OutputConfig::default());
    let dc_pin = Output::new(dc, Level::Low, OutputConfig::default());
    let rst_pin = Output::new(rst, Level::High, OutputConfig::default());

    let spi_dev = match embedded_hal_bus::spi::ExclusiveDevice::new_no_delay(spi, cs_pin) {
        Ok(d) => d,
        Err(_) => {
            crate::push_log(format_args!("[lcd] CS 设备创建失败"));
            return;
        }
    };
    let di = mipidsi::interface::SpiInterface::new(spi_dev, dc_pin, SPI_BUF.uninit().write([0u8; 512]));

    let mut display = match Builder::new(ST7735s, di)
        .display_size(W, H)
        .display_offset(OFF_X, OFF_Y)
        .invert_colors(ColorInversion::Inverted) // IPS 面板基本都要反色
        .color_order(ColorOrder::Bgr)
        .reset_pin(rst_pin)
        .init(&mut delay)
    {
        Ok(d) => d,
        Err(_) => {
            crate::push_log(format_args!("[lcd] 屏幕初始化失败（引脚或驱动不对）"));
            return;
        }
    };
    crate::push_log(format_args!("[lcd] 屏幕就绪 80x160"));

    let style = MonoTextStyle::new(&FONT_6X10, FG);
    let dim_style = MonoTextStyle::new(&FONT_6X10, DIM);
    let mut line: String<24> = String::new();

    // ===== 静态部分：只画一次 =====
    //
    // 之前每秒 `clear(BG)` + 全屏重画，肉眼能看到 1Hz 的整屏闪烁。
    // 现在静态内容画一次就不动，动态内容（STA/IP/Blinker）只在变化时
    // 擦掉自己那一小块再重画，屏幕基本是稳定的。
    display.clear(BG).ok();

    draw_str(&mut display, 2, 2, "WiFi AP", dim_style);
    draw_str(&mut display, 2, 14, crate::store::AP_SSID, style);
    line.clear();
    let _ = write!(line, "pw {}", crate::store::AP_PASSWORD);
    draw_str(&mut display, 2, 26, line.as_str(), style);
    draw_hline(&mut display, 40);

    draw_str(&mut display, 2, 44, "STA", dim_style);
    draw_hline(&mut display, 70);

    draw_str(&mut display, 2, 74, "Blinker", dim_style);
    draw_hline(&mut display, 100);

    draw_str(&mut display, 2, 104, "config", dim_style);
    draw_str(&mut display, 2, 116, "192.168.4.1", style);
    // 最后一行原来是 "BOOT = wake"，改成芯片温度（BOOT 的用法文档里写着，屏幕上没必要占位）
    draw_str(&mut display, 2, 132, "SoC", dim_style);

    // ===== 动态部分：值变了才重画 =====
    // None 表示"还没画过"，第一轮一定会画
    let mut last_sta: Option<u8> = None;
    let mut last_ip: Option<Option<[u8; 4]>> = None;
    let mut last_bl: Option<bool> = None;
    let mut last_temp: Option<i32> = None;
    // 前 5 次读到的温度打到 /log（含 raw 值），之后就不再刷屏
    let mut temp_log_left = 5u8;

    loop {
        let (state, ip, on) = {
            let n = crate::NET.lock().await;
            (
                n.state,
                n.ip,
                crate::BLINKER_ONLINE.load(core::sync::atomic::Ordering::Relaxed),
            )
        };

        // ---- 联网状态：OK / ... / FAIL / - ----
        if last_sta != Some(state) {
            let (txt, col) = match state {
                2 => ("OK", OK),
                1 => ("...", FG),
                3 => ("FAIL", BAD),
                _ => ("-", DIM),
            };
            clear_box(&mut display, 26, 44, 54, 10);
            draw_str(&mut display, 26, 44, txt, MonoTextStyle::new(&FONT_6X10, col));
            last_sta = Some(state);
        }

        // ---- 局域网 IP ----
        if last_ip != Some(ip) {
            line.clear();
            match ip {
                Some(o) => {
                    let _ = write!(line, "{}.{}.{}.{}", o[0], o[1], o[2], o[3]);
                }
                None => {
                    let _ = write!(line, "--");
                }
            }
            // IP 用 5x8 窄字体：一块屏只有 80px 宽，6x10 一行只放得下 13 个字符，
            // 而 "192.168.88.214" 是 15 个字符，会被切掉最后一位 —— 用户照着打字
            // 就会打到一个不存在的地址。5x8 每行能放 16 个，刚好够。
            clear_box(&mut display, 2, 55, 78, 9);
            draw_str(
                &mut display,
                2,
                56,
                line.as_str(),
                MonoTextStyle::new(&FONT_5X8, FG),
            );
            last_ip = Some(ip);
        }

        // ---- Blinker 在线状态 ----
        if last_bl != Some(on) {
            clear_box(&mut display, 2, 86, 78, 10);
            draw_str(
                &mut display,
                2,
                86,
                if on { "on-line" } else { "off-line" },
                MonoTextStyle::new(&FONT_6X10, if on { OK } else { BAD }),
            );
            last_bl = Some(on);
        }

        // ---- 芯片温度（整数度变化才重画，避免数字一直抖）----
        if let Some(raw) = crate::soc_temp::read_raw() {
            let celsius = crate::soc_temp::celsius_idf(raw);
            // 前几次把 raw 和两种换算都打出来，方便对照实物判断公式对不对
            if temp_log_left > 0 {
                temp_log_left -= 1;
                crate::push_log(format_args!(
                    "[temp] raw={} idf={:.1}C alt={:.1}C",
                    raw,
                    celsius,
                    crate::soc_temp::celsius_alt(raw)
                ));
            }
            // 存一份给 /status（单位 0.1℃，整数运算避免浮点格式化）
            crate::TEMP_DECI.store((celsius * 10.0) as i16, core::sync::atomic::Ordering::Relaxed);
            let whole = (celsius + if celsius >= 0.0 { 0.5 } else { -0.5 }) as i32;
            if last_temp != Some(whole) {
                line.clear();
                let _ = write!(line, "{}C", whole);
                // 温度配色：60℃ 以下绿，60~75 白，75 以上红
                let col = if whole < 60 {
                    OK
                } else if whole < 75 {
                    FG
                } else {
                    BAD
                };
                clear_box(&mut display, 26, 132, 52, 10);
                draw_str(&mut display, 26, 132, line.as_str(), MonoTextStyle::new(&FONT_6X10, col));
                last_temp = Some(whole);
            }
        }

        Timer::after(Duration::from_secs(1)).await;
    }
}

fn draw_str<D>(display: &mut D, x: i32, y: i32, s: &str, style: MonoTextStyle<'_, Rgb565>)
where
    D: DrawTarget<Color = Rgb565>,
{
    Text::with_baseline(s, Point::new(x, y), style, Baseline::Top)
        .draw(display)
        .ok();
}

fn draw_hline<D>(display: &mut D, y: i32)
where
    D: DrawTarget<Color = Rgb565>,
{
    embedded_graphics::primitives::Line::new(Point::new(2, y), Point::new(77, y))
        .into_styled(embedded_graphics::primitives::PrimitiveStyle::with_stroke(DIM, 1))
        .draw(display)
        .ok();
}

/// 把一块矩形区域涂回背景色（局部重绘前先擦干净，避免新旧文字叠在一起）
fn clear_box<D>(display: &mut D, x: i32, y: i32, w: u32, h: u32)
where
    D: DrawTarget<Color = Rgb565>,
{
    embedded_graphics::primitives::Rectangle::new(Point::new(x, y), Size::new(w, h))
        .into_styled(
            embedded_graphics::primitives::PrimitiveStyle::with_fill(BG),
        )
        .draw(display)
        .ok();
}
