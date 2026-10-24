//! SPI TFT 屏幕：显示 WiFi 名、IP、连接状态。
//!
//! 用 `mipidsi`（通用 MIPI DBI 驱动）+ `embedded-graphics` 画图。
//! 换屏幕型号只需改 `LCD_MODEL` 那个数字（0/1/2/3）。

use core::fmt::Write as _;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_8X13_BOLD},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use heapless::String;
use mipidsi::{
    Builder,
    interface::SpiInterface,
    models::{ILI9341Rgb565, ST7735s, ST7789},
};

// ---------------- 屏幕要显示的状态（全局共享） ----------------

pub const SSID_MAX: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum WifiState {
    #[default]
    Idle,
    Connecting,
    Connected,
    Failed,
}

impl WifiState {
    pub fn label(self) -> &'static str {
        match self {
            WifiState::Idle => "-",
            WifiState::Connecting => "connecting",
            WifiState::Connected => "connected",
            WifiState::Failed => "FAILED",
        }
    }
    fn color(self) -> Rgb565 {
        match self {
            WifiState::Connected => GREEN,
            WifiState::Failed => RED,
            _ => GREY,
        }
    }
}

#[derive(Clone, Default)]
pub struct Status {
    /// 热点地址（永远有：192.168.4.1）
    pub ap_ip: Option<[u8; 4]>,
    /// STA 连的那个 WiFi 的名字（没配过就是空）
    pub sta_ssid: String<SSID_MAX>,
    /// STA 在局域网里拿到的 IP，None = 没连上
    pub sta_ip: Option<[u8; 4]>,
    /// STA 的连接状态
    pub sta_state: WifiState,
    pub usb_ready: bool,
    pub ws_clients: u8,
    /// 连进热点的设备数（靠 DNS 探测计数）
    pub stations: u8,
    /// 临时提示（长按 BOOT 时显示"还要按多久"），空 = 不显示
    pub hint: String<28>,
}

pub static STATUS: Mutex<CriticalSectionRawMutex, Status> = Mutex::new(Status {
    ap_ip: None,
    sta_ssid: String::new(),
    sta_ip: None,
    sta_state: WifiState::Idle,
    usb_ready: false,
    ws_clients: 0,
    stations: 0,
    hint: String::new(),
});

// ---------------- 配色 ----------------

const BG: Rgb565 = Rgb565::new(0x0C >> 3, 0x10 >> 2, 0x16 >> 3);
const BAR: Rgb565 = Rgb565::new(0x10 >> 3, 0x60 >> 2, 0xA0 >> 3);
const WHITE: Rgb565 = Rgb565::new(0xF5 >> 3, 0xF5 >> 2, 0xF5 >> 3);
const GREY: Rgb565 = Rgb565::new(0x80 >> 3, 0x88 >> 2, 0x90 >> 3);
const GREEN: Rgb565 = Rgb565::new(0x30 >> 3, 0xD0 >> 2, 0x60 >> 3);
const RED: Rgb565 = Rgb565::new(0xE0 >> 3, 0x40 >> 2, 0x40 >> 3);
const CYAN: Rgb565 = Rgb565::new(0x40 >> 3, 0xC8 >> 2, 0xE0 >> 3);
const AMBER: Rgb565 = Rgb565::new(0xF0 >> 3, 0xB0 >> 2, 0x40 >> 3);

// ---------------- 主任务 ----------------

/// 屏幕型号：不知道自己屏是哪块就把这个数字挨个试一遍（0 → 1 → 2 → 3），
/// 哪个能正常显示就用哪个，不用改别的代码。
///
///   0 = ILI9341   240x320   2.4 / 2.8 / 3.2 寸（最常见，默认）
///   1 = ST7789    240x240   1.14 / 1.3 / 1.54 寸方屏
///   2 = ST7789    240x320   2.0 / 2.4 寸无 CS 的长条屏（常见于 ESP32 套件）
///   3 = ST7735s   128x160   1.8 寸
///
/// 颜色不对（红蓝反了）在下面 `Builder` 那行加 `.color_order(ColorOrder::Bgr)`；
/// 整体反色加 `.invert_colors(ColorInversion::Inverted)`。
const LCD_MODEL: usize = 0;

#[embassy_executor::task]
pub async fn lcd_task(
    spi: esp_hal::peripherals::SPI2<'static>,
    sck: esp_hal::peripherals::GPIO12<'static>,
    mosi: esp_hal::peripherals::GPIO11<'static>,
    cs: esp_hal::peripherals::GPIO10<'static>,
    dc: esp_hal::peripherals::GPIO9<'static>,
    rst: esp_hal::peripherals::GPIO8<'static>,
) {
    // SPI：初始化时序别跑太快，20MHz 稳。屏幕不亮就先降到 1MHz 试。
    let spi = Spi::new(spi, SpiConfig::default().with_frequency(Rate::from_mhz(20)))
        .unwrap()
        .with_sck(sck)
        .with_mosi(mosi);

    let cs = Output::new(cs, Level::High, OutputConfig::default());
    let dc = Output::new(dc, Level::Low, OutputConfig::default());
    let rst = Output::new(rst, Level::High, OutputConfig::default());

    // mipidsi 要的是 SpiDevice（自带 CS 管理），
    // 而 esp-hal 的 Spi 只实现 SpiBus —— 用 ExclusiveDevice 包一层补上 CS。
    let spi_dev = match ExclusiveDevice::new(spi, cs, Delay::new()) {
        Ok(d) => d,
        Err(_) => {
            log::error!("[lcd] SPI CS 初始化失败");
            return;
        }
    };

    // 像素批量发送缓冲，越大越快（4KB 足够，再大收益递减）
    let buffer = {
        static BUF: static_cell::StaticCell<[u8; 4096]> = static_cell::StaticCell::new();
        BUF.uninit().write([0u8; 4096])
    };
    let di = SpiInterface::new(spi_dev, dc, buffer);
    let mut delay = Delay::new();

    // 各型号返回的 Display 类型不同，没法放进同一个变量，
    // 所以每个分支各自初始化完直接进 `run()`（run 是泛型的，吃任何 DrawTarget）。
    match LCD_MODEL {
        1 => {
            let mut d = Builder::new(ST7789, di)
                .display_size(240, 240)
                .reset_pin(rst)
                .init(&mut delay);
            start(&mut d, 240, 240).await;
        }
        2 => {
            let mut d = Builder::new(ST7789, di)
                .display_size(240, 320)
                .reset_pin(rst)
                .init(&mut delay);
            start(&mut d, 240, 320).await;
        }
        3 => {
            let mut d = Builder::new(ST7735s, di)
                .display_size(128, 160)
                .reset_pin(rst)
                .init(&mut delay);
            start(&mut d, 128, 160).await;
        }
        _ => {
            let mut d = Builder::new(ILI9341Rgb565, di)
                .reset_pin(rst)
                .init(&mut delay);
            start(&mut d, 240, 320).await;
        }
    }
}

/// 初始化结果处理 + 进入刷新循环
async fn start<D, E>(res: &mut Result<D, E>, w: i32, h: i32)
where
    D: DrawTarget<Color = Rgb565>,
{
    let display = match res {
        Ok(d) => d,
        Err(_) => {
            log::error!("[lcd] 初始化失败（检查接线 / LCD_MODEL 型号）");
            return;
        }
    };
    display.clear(BG).ok();
    log::info!("[lcd] 屏幕已初始化 {w}x{h} (model {LCD_MODEL})");

    let mut last = Status::default();
    let mut force = true;

    loop {
        // 状态没变就不重绘（SPI 刷屏不便宜）
        let cur = STATUS.lock().await.clone();
        if force || !same(&cur, &last) {
            draw(display, &cur, w, h);
            last = cur;
            force = false;
        }
        // 定期强制刷一次，防止偶发花屏
        embassy_time::Timer::after(embassy_time::Duration::from_millis(300)).await;
    }
}

fn same(a: &Status, b: &Status) -> bool {
    a.ap_ip == b.ap_ip
        && a.sta_ssid == b.sta_ssid
        && a.sta_ip == b.sta_ip
        && a.sta_state == b.sta_state
        && a.usb_ready == b.usb_ready
        && a.ws_clients == b.ws_clients
        && a.stations == b.stations
        && a.hint == b.hint
}

const BAR_H: i32 = 26;

/// 一行：左边灰色标签，右边彩色值
fn row<D>(d: &mut D, label: &str, text: &str, col: Rgb565, y: i32, val_x: i32)
where
    D: DrawTarget<Color = Rgb565>,
{
    let lb = MonoTextStyle::new(&FONT_8X13_BOLD, GREY);
    let vb = MonoTextStyle::new(&FONT_8X13_BOLD, col);
    let _ = Text::with_baseline(label, Point::new(8, y), lb, Baseline::Top).draw(d);
    let _ = Text::with_baseline(text, Point::new(val_x, y), vb, Baseline::Top).draw(d);
}

/// 字库只有 ASCII；非 ASCII（中文 SSID）退化成 '?'
fn push_ascii<const N: usize>(dst: &mut String<N>, src: &str) {
    for c in src.chars() {
        if dst.len() + 1 >= N {
            break;
        }
        let ch = if c.is_ascii() { c } else { '?' };
        dst.push(ch).ok();
    }
    if dst.is_empty() {
        dst.push('-').ok();
    }
}

fn draw<D>(display: &mut D, s: &Status, w: i32, h: i32)
where
    D: DrawTarget<Color = Rgb565>,
{
    // 小屏（128x160）行距收紧、标签缩短、不画底部地址条
    let compact = h < 200;
    let step = if compact { 21 } else { 26 };
    let val_x = if compact { 40 } else { 90 };

    let _ = display.clear(BG);

    // ---- 顶部标题条 + 模式徽章 ----
    let _ = Rectangle::new(Point::new(0, 0), Size::new(w as u32, BAR_H as u32))
        .into_styled(PrimitiveStyle::with_fill(BAR))
        .draw(display);
    let title_txt = if compact { "KVM" } else { "ESP32-S3 KVM" };
    let _ = Text::with_baseline(
        title_txt,
        Point::new(8, 18),
        MonoTextStyle::new(&FONT_8X13_BOLD, WHITE),
        Baseline::Top,
    )
    .draw(display);

    // 右上角：连进热点的设备数
    let mut dev: String<8> = String::new();
    let _ = write!(dev, "{}dev", s.stations);
    let bx = w - 8 - dev.len() as i32 * 8;
    let _ = Text::with_baseline(
        &dev,
        Point::new(if bx < 56 { 56 } else { bx }, 18),
        MonoTextStyle::new(
            &FONT_8X13_BOLD,
            if s.stations > 0 { GREEN } else { GREY },
        ),
        Baseline::Top,
    )
    .draw(display);

    let mut y = BAR_H + 12;

    // ---- MODE：AP 常开 + STA 可选 ----
    row(display, "MODE", "AP+STA", WHITE, y, val_x);
    y += step;

    // ---- AP：热点地址（永远可用）----
    let mut ap: String<16> = String::new();
    let ap_col = match s.ap_ip {
        Some(ip) => {
            let _ = write!(ap, "{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
            AMBER
        }
        None => {
            ap.push_str("off").ok();
            RED
        }
    };
    row(display, "AP", &ap, ap_col, y, val_x);
    y += step;

    // ---- STA：连的哪个 WiFi（小屏放不下就省略这行）----
    if !compact {
        let mut ssid: String<SSID_MAX> = String::new();
        if s.sta_ssid.is_empty() {
            ssid.push_str("(not set)").ok();
            row(display, "STA", &ssid, GREY, y, val_x);
        } else {
            push_ascii(&mut ssid, &s.sta_ssid);
            row(display, "STA", &ssid, WHITE, y, val_x);
        }
        y += step;
    }

    // ---- LINK：STA 的连接状态 ----
    row(display, "LINK", s.sta_state.label(), s.sta_state.color(), y, val_x);
    y += step;

    // ---- IP：STA 在局域网里的地址 ----
    let mut ip_txt: String<16> = String::new();
    let ip_col = match s.sta_ip {
        Some(ip) => {
            let _ = write!(ip_txt, "{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
            CYAN
        }
        None => {
            ip_txt.push_str("-").ok();
            GREY
        }
    };
    row(display, "IP", &ip_txt, ip_col, y, val_x);
    y += step;

    // ---- USB ----
    let (u_txt, u_col) = if s.usb_ready {
        ("ready", GREEN)
    } else {
        ("not ready", GREY)
    };
    row(display, "USB", u_txt, u_col, y, val_x);
    y += step;

    // ---- CTRL：有几个控制端连着（小屏省略）----
    if !compact {
        let mut c: String<16> = String::new();
        let _ = write!(c, "{} client(s)", s.ws_clients);
        row(
            display,
            "CTRL",
            &c,
            if s.ws_clients > 0 { GREEN } else { GREY },
            y,
            val_x,
        );
    }

    // ---- 底部：默认显示访问地址；长按 BOOT 时改显示"还要按多久" ----
    if !compact {
        let mut bottom: String<32> = String::new();
        let mut col = WHITE;
        if !s.hint.is_empty() {
            push_ascii(&mut bottom, &s.hint);
            col = AMBER;
        } else if let Some(ip) = s.ap_ip {
            let _ = write!(bottom, "http://{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        }
        if !bottom.is_empty() {
            let _ = Rectangle::new(Point::new(0, h - 34), Size::new(w as u32, 34))
                .into_styled(PrimitiveStyle::with_fill(BAR))
                .draw(display);
            let _ = Text::with_baseline(
                &bottom,
                Point::new(8, h - 24),
                MonoTextStyle::new(&FONT_8X13_BOLD, col),
                Baseline::Top,
            )
            .draw(display);
        }
    } else if !s.hint.is_empty() {
        // 小屏没底部条，提示直接接在最后一行下面
        let mut hnt: String<28> = String::new();
        push_ascii(&mut hnt, &s.hint);
        let _ = Text::with_baseline(
            &hnt,
            Point::new(8, y + step),
            MonoTextStyle::new(&FONT_8X13_BOLD, AMBER),
            Baseline::Top,
        )
        .draw(display);
    }
}
