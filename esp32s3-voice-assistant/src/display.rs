//! 1.54" ST7789 240x240 状态屏
//!
//! 只做一件事：让用户一眼看出设备现在在干嘛。
//!
//! 四个状态配四张脸：
//! ```text
//!   Idle      ○   ○     睁眼待机
//!   Listening ●   ●     睁大眼 + 微张嘴（"我在听"）
//!   Thinking  ◠   ◠     眼睛上翻 + 点点（"在想"）
//!   Speaking  ●   ●     张嘴 + 声波（"在说"）
//! ```
//!
//! 为什么用表情而不是文字：1.54 寸 240x240 上，6x10 的点阵字一行只能放
//! 40 个英文字符，中文根本不支持（要自己烧字库，几 MB）。表情是矢量画的，
//! 不占 flash，而且远看就懂。

use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{
        Circle, Ellipse, Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, Triangle,
    },
    text::{Baseline, Text},
};
use embassy_futures::select::{select, Either};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::{
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode,
    },
    time::Rate,
};
use mipidsi::{
    models::ST7789,
    options::{ColorInversion, ColorOrder},
    Builder,
};

use crate::audio::{self, State};
use crate::board::{LCD_HEIGHT, LCD_WIDTH};

static SPI_BUF: static_cell::StaticCell<[u8; 1024]> = static_cell::StaticCell::new();

/// Wi-Fi 拿到 IP 后由 `main` 发信号，屏幕据此从开机画面切到状态页。
pub static IP_READY: Signal<CriticalSectionRawMutex, heapless::String<16>> = Signal::new();

// 配色：深底 + 青绿，和大部分 AI 硬件的调性一致
const BG: Rgb565 = Rgb565::new(2, 5, 6);
const FG: Rgb565 = Rgb565::WHITE;
const DIM: Rgb565 = Rgb565::new(9, 20, 16);
const ACCENT: Rgb565 = Rgb565::new(4, 58, 28); // 青绿
const WARN: Rgb565 = Rgb565::new(31, 40, 4); // 琥珀

// ---- 兔子配色 ----
// 注意 Rgb565::new 收的是 5/6/5 位的原始值，不是 0~255。
// 这里的值由 #EDE6D8 / #F0A8BC / #F5BCCB 按位截断换算而来。
/// 兔毛：米白 #EDE6D8
const FUR: Rgb565 = Rgb565::new(29, 57, 27);
/// 内耳 / 鼻子：淡粉 #F0A8BC
const PINK: Rgb565 = Rgb565::new(30, 42, 23);
/// 腮红：更浅的粉 #F5BCCB
const BLUSH: Rgb565 = Rgb565::new(30, 47, 25);
/// 嘴线：灰米 #C4BBA8
const MOUTH: Rgb565 = Rgb565::new(24, 47, 21);


#[embassy_executor::task]
pub async fn display_task(
    spi_peri: esp_hal::peripherals::SPI2<'static>,
    sck: esp_hal::gpio::AnyPin<'static>,
    mosi: esp_hal::gpio::AnyPin<'static>,
    cs: esp_hal::gpio::AnyPin<'static>,
    dc: esp_hal::gpio::AnyPin<'static>,
    rst: esp_hal::gpio::AnyPin<'static>,
    bl: esp_hal::gpio::AnyPin<'static>,
) {
    let mut delay = Delay::new();

    // 背光先开，否则调试时黑屏分不清是"没初始化"还是"没背光"
    let _bl = Output::new(bl, Level::High, OutputConfig::default());

    let spi = match Spi::new(
        spi_peri,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(Mode::_0),
    ) {
        Ok(s) => s.with_sck(sck).with_mosi(mosi),
        Err(_) => {
            log::error!("[lcd] SPI 配置失败");
            return;
        }
    };

    let cs_pin = Output::new(cs, Level::High, OutputConfig::default());
    let dc_pin = Output::new(dc, Level::Low, OutputConfig::default());
    let rst_pin = Output::new(rst, Level::High, OutputConfig::default());

    let spi_dev = match embedded_hal_bus::spi::ExclusiveDevice::new_no_delay(spi, cs_pin) {
        Ok(d) => d,
        Err(_) => return,
    };
    let di = mipidsi::interface::SpiInterface::new(
        spi_dev,
        dc_pin,
        SPI_BUF.uninit().write([0u8; 1024]),
    );

    let mut display = match Builder::new(ST7789, di)
        .display_size(LCD_WIDTH, LCD_HEIGHT)
        // 240x240 的 ST7789 面板内部显存是 240x320，要偏置 80 行
        .display_offset(0, 80)
        .invert_colors(ColorInversion::Inverted)
        .color_order(ColorOrder::Rgb)
        .reset_pin(rst_pin)
        .init(&mut delay)
    {
        Ok(d) => d,
        Err(_) => {
            log::error!("[lcd] 初始化失败（引脚不对或屏幕不是 ST7789）");
            return;
        }
    };
    log::info!("[lcd] 240x240 就绪");

    // ---------------- 开机画面 ----------------
    let boot_at = Instant::now();
    boot_screen(&mut display);
    log::info!("[lcd] 开机画面已显示");

    // 等 Wi-Fi 拿到 IP，最多 12 秒（进度条期间一直挂着兔子脸）
    let ip = match select(IP_READY.wait(), Timer::after(Duration::from_secs(12))).await {
        Either::First(ip) => Some(ip),
        Either::Second(_) => {
            log::warn!("[lcd] 等 IP 超时，先切状态页");
            None
        }
    };

    // 开机画面至少停 1.5 秒，否则网快的时候会一闪而过
    let min_show = Duration::from_millis(1500);
    let shown = boot_at.elapsed();
    if shown < min_show {
        Timer::after(min_show - shown).await;
    }

    // ---------------- 状态页 ----------------
    let title = MonoTextStyle::new(&FONT_6X10, DIM);
    let addr_style = MonoTextStyle::new(&FONT_6X10, FGDIM);

    let _ = display.clear(BG);
    Text::with_baseline("VOICE ASSISTANT", Point::new(8, 6), title, Baseline::Top)
        .draw(&mut display)
        .ok();
    let ip_text = match &ip {
        Some(s) => s.as_str(),
        None => "no wifi",
    };
    Text::with_baseline(ip_text, Point::new(8, 20), addr_style, Baseline::Top)
        .draw(&mut display)
        .ok();

    // 芯片温度（右上角）。sensors_task 每 2 秒刷新一次，这里只负责显示。
    let mut last_temp = crate::soc_temp::last();
    draw_temp(&mut display, last_temp);

    let mut last: Option<State> = None;
    // 说话时的声波动画相位
    let mut phase = 0u8;

    loop {
        // 温度变了单独重绘右上角那一小块，不动整屏（避免闪）
        let t = crate::soc_temp::last();
        if t != last_temp {
            draw_temp(&mut display, t);
            last_temp = t;
        }

        let st = audio::state();
        if last != Some(st) || st == State::Speaking || st == State::Thinking {
            draw_face(&mut display, st, phase);
            last = Some(st);
        }
        if st == State::Speaking || st == State::Thinking {
            phase = phase.wrapping_add(1);
            embassy_time::Timer::after(embassy_time::Duration::from_millis(120)).await;
        } else {
            embassy_time::Timer::after(embassy_time::Duration::from_millis(300)).await;
        }
    }
}

const FGDIM: Rgb565 = Rgb565::new(12, 28, 22);

/// 重绘右上角的芯片温度。
///
/// 6x10 点阵字库只有 ASCII，**没有 ° 符号**，所以写成 `42C` 而不是 `42°C`。
/// 超过 70℃ 变琥珀色 —— S3 长时间满载跑到 70 多是正常的，但值得一眼看见。
fn draw_temp<D>(d: &mut D, t: Option<i16>)
where
    D: DrawTarget<Color = Rgb565>,
{
    use core::fmt::Write as _;

    // 先清掉这块（最宽的情况是 "100C" = 24px）
    Rectangle::new(Point::new(198, 2), Size::new(42, 14))
        .into_styled(PrimitiveStyle::with_fill(BG))
        .draw(d)
        .ok();

    let mut buf = heapless::String::<8>::new();
    let col = match t {
        Some(v) => {
            let _ = write!(buf, "{v}C");
            if v >= 70 {
                WARN
            } else {
                DIM
            }
        }
        None => {
            let _ = buf.push_str("--C");
            DIM
        }
    };

    // 右对齐到 x=236
    let w = buf.len() as i32 * 6;
    Text::with_baseline(
        buf.as_str(),
        Point::new(236 - w, 6),
        MonoTextStyle::new(&FONT_6X10, col),
        Baseline::Top,
    )
    .draw(d)
    .ok();
}

/// 开机画面：一只小兔子。
///
/// 全部用矢量图元画（Circle / Ellipse / Triangle / Line），**不烧位图**：
/// 1.54 寸 240x240 全屏位图要 115KB flash，而这十几个图元编译出来不到 2KB，
/// 改配色只动一个常量、挪位置只动坐标，不用重新转图。
///
/// 坐标是在 240x240 设计稿上定死的，和设计预览一一对应：
///
/// | 部位 | 图元 | 参数 |
/// |------|------|------|
/// | 耳朵 | 椭圆 ×2 | 中心 (95,62)/(145,62)，半径 (15,40) |
/// | 内耳 | 椭圆 ×2 | 同中心，半径 (7,25) |
/// | 头 | 圆 | 中心 (120,148)，r=58 |
/// | 眼睛 | 圆 ×2 | 中心 (100,136)/(140,136)，r=8 |
/// | 鼻子 | 三角 | (120,153)-(113,161)-(127,161) |
///
/// 画序很重要：耳朵先画，脑袋后画，把耳朵根部盖住，看起来才是"长"在头上。
pub fn boot_screen<D>(d: &mut D)
where
    D: DrawTarget<Color = Rgb565>,
{
    d.clear(BG).ok();

    // ---- 耳朵（先画，等下被脑袋压住根部）----
    for cx in [95i32, 145] {
        Ellipse::new(Point::new(cx - 15, 22), Size::new(30, 80))
            .into_styled(PrimitiveStyle::with_fill(FUR))
            .draw(d)
            .ok();
        Ellipse::new(Point::new(cx - 7, 38), Size::new(14, 50))
            .into_styled(PrimitiveStyle::with_fill(PINK))
            .draw(d)
            .ok();
    }

    // ---- 头 ----
    Circle::new(Point::new(62, 90), 116)
        .into_styled(PrimitiveStyle::with_fill(FUR))
        .draw(d)
        .ok();

    // ---- 腮红 ----
    for cx in [82i32, 158] {
        Circle::new(Point::new(cx - 9, 153), 18)
            .into_styled(PrimitiveStyle::with_fill(BLUSH))
            .draw(d)
            .ok();
    }

    // ---- 眼睛：用背景色填充，在米白脸上就形成深色瞳 ----
    for cx in [100i32, 140] {
        Circle::new(Point::new(cx - 8, 128), 16)
            .into_styled(PrimitiveStyle::with_fill(BG))
            .draw(d)
            .ok();
        // 左上角高光，让眼神有精神
        Circle::new(Point::new(cx - 1, 131), 6)
            .into_styled(PrimitiveStyle::with_fill(FG))
            .draw(d)
            .ok();
    }

    // ---- 鼻子 ----
    Triangle::new(
        Point::new(120, 152),
        Point::new(113, 160),
        Point::new(127, 160),
    )
    .into_styled(PrimitiveStyle::with_fill(PINK))
    .draw(d)
    .ok();

    // ---- 嘴：两道折线拼成 w ----
    for (a, b) in [
        ((120, 160), (112, 168)),
        ((112, 168), (106, 164)),
        ((120, 160), (128, 168)),
        ((128, 168), (134, 164)),
    ] {
        Line::new(Point::new(a.0, a.1), Point::new(b.0, b.1))
            .into_styled(PrimitiveStyle::with_stroke(MOUTH, 3))
            .draw(d)
            .ok();
    }

    // ---- 底部一行小字 ----
    let label = "HELLO";
    let tw = label.len() as i32 * 6;
    Text::with_baseline(
        label,
        Point::new(120 - tw / 2, 222),
        MonoTextStyle::new(&FONT_6X10, DIM),
        Baseline::Top,
    )
    .draw(d)
    .ok();
}

fn draw_face<D>(d: &mut D, st: State, phase: u8)
where
    D: DrawTarget<Color = Rgb565>,
{
    // 只清中间那块，避免整屏闪
    let area = Rectangle::new(Point::new(0, 40), Size::new(LCD_WIDTH as u32, 160));
    area.into_styled(PrimitiveStyle::with_fill(BG)).draw(d).ok();

    let cx = LCD_WIDTH as i32 / 2;
    let eye_y = 100;
    let eye_dx = 42;

    let (label, col) = match st {
        State::Idle => ("READY", DIM),
        State::Listening => ("LISTENING", ACCENT),
        State::Thinking => ("THINKING", WARN),
        State::Speaking => ("SPEAKING", FG),
    };

    // ---- 眼睛 ----
    match st {
        State::Idle => {
            // 闭眼：两条短横线
            for dx in [-eye_dx, eye_dx] {
                Line::new(
                    Point::new(cx + dx - 14, eye_y),
                    Point::new(cx + dx + 14, eye_y),
                )
                .into_styled(PrimitiveStyle::with_stroke(FG, 5))
                .draw(d)
                .ok();
            }
        }
        State::Thinking => {
            // 上翻：半圆弧用三角形近似（只用上半个轮廓）
            for dx in [-eye_dx, eye_dx] {
                Triangle::new(
                    Point::new(cx + dx - 14, eye_y + 6),
                    Point::new(cx + dx, eye_y - 10),
                    Point::new(cx + dx + 14, eye_y + 6),
                )
                .into_styled(PrimitiveStyleBuilder::new().stroke_width(4).stroke_color(FG).build())
                .draw(d)
                .ok();
            }
            // 省略号
            for i in 0..3u8 {
                if (phase % 6) as u8 >= i {
                    let x = cx - 20 + i as i32 * 20;
                    Circle::new(Point::new(x, 142), 6)
                        .into_styled(PrimitiveStyle::with_fill(WARN))
                        .draw(d)
                        .ok();
                }
            }
        }
        _ => {
            // 睁眼
            for dx in [-eye_dx, eye_dx] {
                Circle::new(Point::new(cx + dx - 13, eye_y - 13), 26)
                    .into_styled(PrimitiveStyleBuilder::new().stroke_width(4).stroke_color(FG).build())
                    .draw(d)
                    .ok();
                Circle::new(Point::new(cx + dx - 6, eye_y - 6), 12)
                    .into_styled(PrimitiveStyle::with_fill(
                        if st == State::Listening { ACCENT } else { FG },
                    ))
                    .draw(d)
                    .ok();
            }
        }
    }

    // ---- 嘴 ----
    match st {
        State::Idle => {
            Line::new(Point::new(cx - 18, 142), Point::new(cx + 18, 142))
                .into_styled(PrimitiveStyle::with_stroke(DIM, 4))
                .draw(d)
                .ok();
        }
        State::Listening => {
            // 微张的小圆
            Circle::new(Point::new(cx - 10, 132), 20)
                .into_styled(PrimitiveStyle::with_fill(ACCENT))
                .draw(d)
                .ok();
        }
        State::Thinking => {}
        State::Speaking => {
            // 张合的嘴 + 两侧声波
            let h = 10 + (phase % 4) * 5;
            Rectangle::new(Point::new(cx - 20, 140 - h as i32 / 2), Size::new(40, h as u32))
                .into_styled(PrimitiveStyle::with_fill(FG))
                .draw(d)
                .ok();
            for i in 1..=3u8 {
                let w = 3 + (phase + i) % 3;
                let x1 = cx + 44 + i as i32 * 12;
                let x2 = cx - 44 - i as i32 * 12;
                Line::new(Point::new(x1, 140 - w as i32), Point::new(x1, 140 + w as i32))
                    .into_styled(PrimitiveStyle::with_stroke(ACCENT, 3))
                    .draw(d)
                    .ok();
                Line::new(Point::new(x2, 140 - w as i32), Point::new(x2, 140 + w as i32))
                    .into_styled(PrimitiveStyle::with_stroke(ACCENT, 3))
                    .draw(d)
                    .ok();
            }
        }
    }

    // ---- 底部状态文字 ----
    let tw = label.len() as u32 * 6;
    Text::with_baseline(
        label,
        Point::new(cx - tw as i32 / 2, 208),
        MonoTextStyle::new(&FONT_6X10, col),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    // 说话/聆听时底部一条进度条，纯装饰
    if st != State::Idle {
        let w = (LCD_WIDTH as u32).saturating_sub(40);
        let fill = match st {
            State::Listening => w,
            State::Thinking => w / 3,
            _ => w,
        };
        Rectangle::new(Point::new(20, 230), Size::new(fill, 3))
            .into_styled(PrimitiveStyle::with_fill(col))
            .draw(d)
            .ok();
        Rectangle::new(Point::new(20 + fill as i32, 230), Size::new(w - fill, 3))
            .into_styled(PrimitiveStyle::with_fill(DIM))
            .draw(d)
            .ok();
    }
}
