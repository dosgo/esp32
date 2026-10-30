//! Minimal ST7789 status screen for the ESP-IDF port.

use std::sync::mpsc;
use std::thread;

use embedded_graphics::mono_font::{ascii::FONT_10X20, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::{OutputPin, PinDriver};
use esp_idf_hal::spi::config::{Config, DriverConfig};
use esp_idf_hal::spi::{SpiBusDriver, SpiDriver, SPI2};
use esp_idf_hal::units::FromValueType;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, ColorOrder};
use mipidsi::Builder;

const BG: Rgb565 = Rgb565::new(2, 5, 6);
const FG: Rgb565 = Rgb565::BLUE;
const ACCENT: Rgb565 = Rgb565::new(4, 58, 28);

pub fn start(
    spi: SPI2<'static>,
    sclk: impl OutputPin + Send + 'static,
    mosi: impl OutputPin + Send + 'static,
    cs: impl OutputPin + Send + 'static,
    dc: impl OutputPin + Send + 'static,
    rst: impl OutputPin + Send + 'static,
    bl: impl OutputPin + Send + 'static,
) -> mpsc::Sender<&'static str> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _bl = PinDriver::output(bl).unwrap();
        let _bl = {
            let mut p = _bl;
            p.set_high().ok();
            p
        };

        let drv_cfg = DriverConfig::new();
        let dev_cfg = Config::new().baudrate(40.MHz().into());
        let spi = SpiDriver::new(spi, sclk, mosi, None::<esp_idf_hal::gpio::AnyInputPin>, &drv_cfg)
            .expect("LCD SPI");
        let cs = PinDriver::output(cs).expect("LCD CS");
        let mut dc = PinDriver::output(dc).expect("LCD DC");
        dc.set_low().ok();
        let mut rst = PinDriver::output(rst).expect("LCD RST");
        rst.set_high().ok();

        let bus = SpiBusDriver::new(spi, &dev_cfg).expect("LCD SPI bus");
        let spi_dev = ExclusiveDevice::new_no_delay(bus, cs).expect("LCD SPI device");
        let buf: &'static mut [u8] = Box::leak(vec![0u8; 1024].into_boxed_slice());
        let di = mipidsi::interface::SpiInterface::new(spi_dev, dc, buf);

        let mut delay = FreeRtos;
        let mut display = Builder::new(ST7789, di)
            .display_size(240, 240)
            .display_offset(0, 0)
            .invert_colors(ColorInversion::Inverted)
            .color_order(ColorOrder::Rgb)
            .reset_pin(rst)
            .init(&mut delay)
            .expect("LCD init");

        draw(&mut display, "BOOT");
        while let Ok(status) = rx.recv() {
            draw(&mut display, status);
        }
    });
    tx
}

fn draw<D>(display: &mut D, status: &str)
where
    D: DrawTarget<Color = Rgb565>,
{
    let bg = Rectangle::new(Point::zero(), Size::new(240, 240))
        .into_styled(PrimitiveStyle::with_fill(BG));
    let _ = bg.draw(display);

    let title = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let _ = Text::new("ESP32-S3", Point::new(20, 50), title).draw(display);
    let _ = Text::new("VOICE", Point::new(20, 78), title).draw(display);

    let accent = Circle::new(Point::new(20, 120), 20)
        .into_styled(PrimitiveStyle::with_fill(ACCENT));
    let _ = accent.draw(display);

    let _ = Text::new(status, Point::new(55, 137), title).draw(display);
}
