//! 0.96" ST7735S IPS 80x160 屏 —— CSI 项目的「仪表盘」
//!
//! 没有串口、手机又不在同一个网里的时候，这块屏是唯一能直接看到状态的地方：
//! 热点账号密码、STA 拿到的 IP、芯片温度、CSI 帧率。
//!
//! 接线（这块板**不是** LilyGO T-Dongle-S3，接线完全不同）：
//!   来源：github.com/beastboost/jcorp-nomad/blob/main/docs/PocketDongleS3.md
//!   SCL / SCLK = GPIO10
//!   SDA / MOSI = GPIO11
//!   CS         = GPIO12
//!   DC / RS    = GPIO13
//!   RST / RES  = GPIO14
//!   背光：没有背光脚，硬连常亮（上电就亮，不归软件管）
//!
//! 其它已知接线：BOOT 键 = GPIO0；TF 卡独占 SPI（17/18/16/47）；
//! GPIO33~37 被八线 PSRAM 永久占用，任何设计都不能用。

use core::fmt::Write as _;

use embassy_time::{Duration, Timer};
use embedded_graphics::{
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_5X8, FONT_6X10, FONT_10X20},
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
/// ST7735 显存是 132x162，80x160 的面板要偏置，否则画面错位
const OFF_X: u16 = 26;
const OFF_Y: u16 = 1;

/// mipidsi 的 SPI 接口要一块缓冲区，必须 static（Display 活到任务结束）
static SPI_BUF: static_cell::StaticCell<[u8; 512]> = static_cell::StaticCell::new();

const BG: Rgb565 = Rgb565::new(3, 6, 4); // 深蓝灰
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
            .with_frequency(Rate::from_mhz(26))
            .with_mode(Mode::_0),
    ) {
        Ok(s) => s.with_sck(sck).with_mosi(mosi),
        Err(_) => {
            log::warn!("[lcd] SPI 配置失败");
            return;
        }
    };

    let cs_pin = Output::new(cs, Level::High, OutputConfig::default());
    let dc_pin = Output::new(dc, Level::Low, OutputConfig::default());
    let rst_pin = Output::new(rst, Level::High, OutputConfig::default());

    let spi_dev = match embedded_hal_bus::spi::ExclusiveDevice::new_no_delay(spi, cs_pin) {
        Ok(d) => d,
        Err(_) => {
            log::warn!("[lcd] CS 设备创建失败");
            return;
        }
    };
    let di = mipidsi::interface::SpiInterface::new(
        spi_dev,
        dc_pin,
        SPI_BUF.uninit().write([0u8; 512]),
    );

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
            log::warn!("[lcd] 屏幕初始化失败（引脚或驱动不对）");
            return;
        }
    };
    log::info!("[lcd] 屏幕就绪 80x160");

    // ===== 静态部分：只画一次 =====
    display.clear(BG).ok();

    draw_str(&mut display, 2, 1, "CSI SENSOR", MonoTextStyle::new(&FONT_5X8, DIM));

    draw_str(&mut display, 2, 12, "AP", MonoTextStyle::new(&FONT_5X8, DIM));
    draw_str(
        &mut display,
        2,
        21,
        crate::config::AP_SSID,
        MonoTextStyle::new(&FONT_6X10, FG),
    );
    let mut line: String<24> = String::new();
    let _ = write!(line, "pw {}", crate::config::AP_PASS);
    draw_str(&mut display, 2, 33, line.as_str(), MonoTextStyle::new(&FONT_6X10, FG));
    draw_hline(&mut display, 47);

    draw_str(&mut display, 2, 50, "STA", MonoTextStyle::new(&FONT_5X8, DIM));
    draw_hline(&mut display, 71);

    draw_str(&mut display, 2, 74, "CSI", MonoTextStyle::new(&FONT_5X8, DIM));
    draw_hline(&mut display, 95);

    draw_str(&mut display, 2, 98, "SoC", MonoTextStyle::new(&FONT_5X8, DIM));

    // ===== 动态部分：值变了才重画（整屏重画会有 1Hz 闪烁）=====
    let mut last_ip: Option<u32> = None;
    let mut last_fps: Option<u32> = None;
    let mut last_temp: Option<i32> = None;
    let mut last_frames = csi_frames();
    let mut tick: u32 = 0;

    loop {
        // ---- STA IP ----
        let ip = crate::STA_IP.load(core::sync::atomic::Ordering::Relaxed);
        if last_ip != Some(ip) {
            line.clear();
            if ip == 0 {
                let _ = write!(line, "no link");
            } else {
                let o = ip.to_be_bytes();
                let _ = write!(line, "{}.{}.{}.{}", o[0], o[1], o[2], o[3]);
            }
            // IP 用 5x8 窄字体：80px 宽用 6x10 只放得下 13 个字符，
            // "192.168.88.214" 是 14 个，会被切掉最后一位 ——
            // 用户照着打字就会打到一个不存在的地址。
            clear_box(&mut display, 2, 59, 78, 10);
            draw_str(
                &mut display,
                2,
                60,
                line.as_str(),
                MonoTextStyle::new(&FONT_5X8, if ip == 0 { DIM } else { FG }),
            );
            last_ip = Some(ip);
        }

        // ---- CSI 帧率 + 子载波数 ----
        let frames = csi_frames();
        let fps = frames.wrapping_sub(last_frames) / 2;
        last_frames = frames;
        if last_fps != Some(fps) {
            line.clear();
            let n = crate::csi::LAST_N.load(core::sync::atomic::Ordering::Relaxed);
            let _ = write!(line, "{}fps {}ch", fps, n);
            clear_box(&mut display, 2, 82, 78, 10);
            // 0 帧说明没有下行流量：AP 模式没开页面时是常态，用暗色区分
            let col = if fps == 0 { DIM } else { OK };
            draw_str(
                &mut display,
                2,
                83,
                line.as_str(),
                MonoTextStyle::new(&FONT_5X8, col),
            );
            last_fps = Some(fps);
        }

        // ---- 芯片温度（0.1℃ 变化才重画，避免数字一直抖）----
        let t = crate::TEMP_X10.load(core::sync::atomic::Ordering::Relaxed);
        if t != 0 && last_temp != Some(t) {
            line.clear();
            let _ = write!(line, "{}.{}C", t / 10, t % 10);
            // 62℃ 以下绿，62~75 白，75 以上红（S3 结温 85℃ 开始降频）
            let col = if t < 620 {
                OK
            } else if t < 750 {
                FG
            } else {
                BAD
            };
            clear_box(&mut display, 2, 107, 76, 22);
            draw_str(
                &mut display,
                2,
                108,
                line.as_str(),
                MonoTextStyle::new(&FONT_10X20, col),
            );
            last_temp = Some(t);
        }

        tick = tick.wrapping_add(1);
        if tick % 10 == 0 {
            // 偶尔来一行日志，方便确认屏还活着
            log::debug!("[lcd] 刷新中");
        }

        Timer::after(Duration::from_secs(2)).await;
    }
}

fn csi_frames() -> u32 {
    crate::csi::FRAMES.load(core::sync::atomic::Ordering::Relaxed)
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

/// 局部重绘前先把这块擦回背景色，避免新旧文字叠在一起
fn clear_box<D>(display: &mut D, x: i32, y: i32, w: u32, h: u32)
where
    D: DrawTarget<Color = Rgb565>,
{
    embedded_graphics::primitives::Rectangle::new(Point::new(x, y), Size::new(w, h))
        .into_styled(embedded_graphics::primitives::PrimitiveStyle::with_fill(BG))
        .draw(display)
        .ok();
}
