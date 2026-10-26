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
    primitives::{Circle, Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, Triangle},
    text::{Baseline, Text},
};
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

// 配色：深底 + 青绿，和大部分 AI 硬件的调性一致
const BG: Rgb565 = Rgb565::new(2, 5, 6);
const FG: Rgb565 = Rgb565::WHITE;
const DIM: Rgb565 = Rgb565::new(9, 20, 16);
const ACCENT: Rgb565 = Rgb565::new(4, 58, 28); // 青绿
const WARN: Rgb565 = Rgb565::new(31, 40, 4); // 琥珀


#[embassy_executor::task]
pub async fn display_task(
    spi_peri: esp_hal::peripherals::SPI2<'static>,
    sck: esp_hal::gpio::AnyPin<'static>,
    mosi: esp_hal::gpio::AnyPin<'static>,
    cs: esp_hal::gpio::AnyPin<'static>,
    dc: esp_hal::gpio::AnyPin<'static>,
    rst: esp_hal::gpio::AnyPin<'static>,
    bl: esp_hal::gpio::AnyPin<'static>,
    ip: heapless::String<16>,
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

    let title = MonoTextStyle::new(&FONT_6X10, DIM);
    let addr_style = MonoTextStyle::new(&FONT_6X10, FGDIM);

    let _ = display.clear(BG);
    Text::with_baseline("VOICE ASSISTANT", Point::new(8, 6), title, Baseline::Top)
        .draw(&mut display)
        .ok();
    Text::with_baseline(ip.as_str(), Point::new(8, 20), addr_style, Baseline::Top)
        .draw(&mut display)
        .ok();

    let mut last: Option<State> = None;
    // 说话时的声波动画相位
    let mut phase = 0u8;

    loop {
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
