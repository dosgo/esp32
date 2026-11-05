//! 1.54" ST7789 240x240 状态屏
//!
//! 这块屏是板载的（和语音助手项目同一块板），引脚：
//! SCLK=38 / MOSI=39 / CS=21 / DC=45 / RST=40 / BL=46。
//!
//! 屏幕只干一件事：把「连哪个热点、密码多少、隧道通没通」直接怼在脸上。
//! 之前这个固件完全没有屏幕代码，上电后屏是黑的（或残留上一版固件的画面），
//! 用户只能靠串口或网页猜现在的状态。
//!
//! 240x240 的内置点阵字库只有 ASCII，所以界面全英文；中文要自己烧字库，
//! 几 MB 的 flash 为一块状态屏不值当。

use embassy_time::{Duration, Timer};
use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_6X10},
        MonoTextStyle,
    },
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
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

use crate::state;

/// mipidsi 每次写屏都要一块缓冲，放静态区省栈
static SPI_BUF: static_cell::StaticCell<[u8; 1024]> = static_cell::StaticCell::new();

const LCD_W: i32 = 240;
const LCD_H: i32 = 240;

// 深底 + 青绿，和网页/安卓端一套配色
const BG: Rgb565 = Rgb565::new(2, 5, 6);
const FG: Rgb565 = Rgb565::new(30, 62, 30);
const DIM: Rgb565 = Rgb565::new(11, 24, 20);
const ACCENT: Rgb565 = Rgb565::new(4, 58, 28);
const WARN: Rgb565 = Rgb565::new(31, 40, 4);

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

    // 背光先开：否则初始化失败时黑屏，分不清是「没点亮」还是「没驱动」
    let _bl = Output::new(bl, Level::High, OutputConfig::default());

    let spi = match Spi::new(
        spi_peri,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(Mode::_0),
    ) {
        Ok(s) => s.with_sck(sck).with_mosi(mosi),
        Err(_) => {
            log::error!("[lcd] SPI config failed");
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
        .display_size(240, 240)
        // offset 必须是 (0,0)：ST7789 显存是 240x320，这块面板显示的是 GRAM 的
        // 0~239 行。写 (0,80) 会让顶部 80 行永远没被写过，上电就是一条雪花噪点。
        .display_offset(0, 0)
        // 这块 IPS 面板需要全局反色，关掉会变成黑白
        .invert_colors(ColorInversion::Inverted)
        .color_order(ColorOrder::Rgb)
        .reset_pin(rst_pin)
        .init(&mut delay)
    {
        Ok(d) => d,
        Err(_) => {
            log::error!("[lcd] init failed (wrong pins or not ST7789)");
            return;
        }
    };
    log::info!("[lcd] 240x240 ready");

    // 开机画面，至少停 800ms，避免网快时一闪而过
    boot_screen(&mut display);
    Timer::after(Duration::from_millis(800)).await;

    let mut last_sig = StatusSig::default();

    loop {
        let sig = StatusSig::now();
        if sig != last_sig {
            draw_status(&mut display, &sig);
            last_sig = sig;
        }
        // 300ms 采样一次，够快看出连没连上，又不会一直刷 SPI
        Timer::after(Duration::from_millis(300)).await;
    }
}

/// 只在内容变化时重绘，减少 SPI 占用（全屏 115KB，40MHz 约 25ms）
#[derive(PartialEq, Default, Clone, Copy)]
struct StatusSig {
    /// FOU 隧道开关（网页上那个）
    enabled: bool,
    /// 手机是否真的连上 AP 并拿到 IP：靠 ARP 表里有没有它判断，
    /// 比看配置里的目标地址靠谱（那个地址有默认值，永远非空）
    linked: bool,
    tx: u32,
    rx: u32,
    client: u32,
    fou_ip: u32,
    fou_port: u16,
    ssid_len: u8,
    pass_len: u8,
    /// 上行 Wi-Fi：开着没有、连上没有
    up_on: bool,
    up_ip: u32,
    up_state: u8,
    /// 当前出口：0 无 / 1 本热点里的手机 / 2 上行 Wi-Fi
    path: u8,
    up_target: u32,
}

impl StatusSig {
    fn now() -> Self {
        let s = state::snapshot();
        let (ssid, pass) = state::net_copy();
        Self {
            enabled: s.up,
            // 两条路任意一条通就算连上（AP 侧看 ARP，上行侧看有没有拿到地址）
            linked: s.path != 0,
            tx: s.fwd_out,
            rx: s.fwd_in,
            client: u32::from(s.client_ip),
            fou_ip: state::fou_ip_raw(),
            fou_port: state::fou_port(),
            ssid_len: ssid.len() as u8,
            pass_len: pass.len() as u8,
            up_on: s.up_on,
            up_ip: u32::from(s.up_ip),
            up_state: s.up_state,
            path: s.path,
            up_target: u32::from(s.up_target),
        }
    }
}

fn boot_screen<D>(d: &mut D)
where
    D: DrawTarget<Color = Rgb565>,
{
    d.clear(BG).ok();

    let title = "POCKET GATEWAY";
    let w = title.len() as i32 * 10;
    Text::with_baseline(
        title,
        Point::new((LCD_W - w) / 2, 78),
        MonoTextStyle::new(&FONT_10X20, ACCENT),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    let sub = "FOU tunnel gateway";
    let w = sub.len() as i32 * 6;
    Text::with_baseline(
        sub,
        Point::new((LCD_W - w) / 2, 108),
        MonoTextStyle::new(&FONT_6X10, DIM),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    // 中间一条呼吸进度条，纯装饰，表示在初始化
    Rectangle::new(Point::new(40, 140), Size::new(160, 3))
        .into_styled(PrimitiveStyle::with_fill(DIM))
        .draw(d)
        .ok();
    Rectangle::new(Point::new(40, 140), Size::new(70, 3))
        .into_styled(PrimitiveStyle::with_fill(ACCENT))
        .draw(d)
        .ok();
}

/// 状态页。布局固定，每块区域先清再用，避免旧内容叠字。
fn draw_status<D>(d: &mut D, sig: &StatusSig)
where
    D: DrawTarget<Color = Rgb565>,
{
    let (ssid, pass) = state::net_copy();

    d.clear(BG).ok();

    // ---------------- 标题 ----------------
    Text::with_baseline(
        "POCKET GATEWAY",
        Point::new(8, 6),
        MonoTextStyle::new(&FONT_6X10, DIM),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    // 右上角：手机连没连上。这是整块屏最该一眼看到的信息
    let (state_txt, state_col) = if sig.linked {
        ("LINKED", ACCENT)
    } else {
        ("WAITING", WARN)
    };
    let w = state_txt.len() as i32 * 6;
    Text::with_baseline(
        state_txt,
        Point::new(LCD_W - 8 - w, 6),
        MonoTextStyle::new(&FONT_6X10, state_col),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    Line::new(Point::new(8, 22), Point::new(LCD_W - 8, 22))
        .into_styled(PrimitiveStyle::with_stroke(DIM, 1))
        .draw(d)
        .ok();

    // ---------------- Wi-Fi 账号密码 ----------------
    // 用户要的就是这两行：新设备连上来，不用翻文档
    field(d, "WIFI NAME", ssid.as_str(), 30, FG);
    field(d, "PASSWORD", pass.as_str(), 74, ACCENT);

    Line::new(Point::new(8, 118), Point::new(LCD_W - 8, 118))
        .into_styled(PrimitiveStyle::with_stroke(DIM, 1))
        .draw(d)
        .ok();

    // ---------------- 设置页 / FOU 地址 ----------------
    kv(
        d,
        "SETUP",
        "http://192.168.4.1",
        124,
        MonoTextStyle::new(&FONT_6X10, FG),
    );

    let mut fou = heapless::String::<32>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut fou,
        format_args!("{}:{}", state::fou_ip(), sig.fou_port),
    );
    let fou_col = if sig.enabled { ACCENT } else { DIM };
    kv(d, "FOU", fou.as_str(), 140, MonoTextStyle::new(&FONT_6X10, fou_col));

    Line::new(Point::new(8, 158), Point::new(LCD_W - 8, 158))
        .into_styled(PrimitiveStyle::with_stroke(DIM, 1))
        .draw(d)
        .ok();

    // ---------------- 流量 ----------------
    let mut line = heapless::String::<48>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut line,
        format_args!("tx {}   rx {}", sig.tx, sig.rx),
    );
    Text::with_baseline(
        line.as_str(),
        Point::new(8, 168),
        MonoTextStyle::new(&FONT_6X10, FG),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    // 客户端信息：连着哪台手机
    let mut cli = heapless::String::<48>::new();
    match sig.path {
        // 手机就在本热点里
        1 => {
            let o = sig.client.to_be_bytes();
            let _ = core::fmt::Write::write_fmt(
                &mut cli,
                format_args!("ap: {}.{}.{}.{}", o[0], o[1], o[2], o[3]),
            );
        }
        // 手机在上游 Wi-Fi 里
        2 => {
            let o = sig.up_target.to_be_bytes();
            if sig.up_target == 0 {
                let _ = cli.push_str("up: (no target)");
            } else {
                let _ = core::fmt::Write::write_fmt(
                    &mut cli,
                    format_args!("up: {}.{}.{}.{}", o[0], o[1], o[2], o[3]),
                );
            }
        }
        _ => {
            if sig.up_on {
                let _ = cli.push_str(match sig.up_state {
                    3 => "up: connect failed",
                    _ => "up: connecting...",
                });
            } else {
                let _ = cli.push_str("peer: none");
            }
        }
    }
    Text::with_baseline(
        cli.as_str(),
        Point::new(8, 184),
        MonoTextStyle::new(&FONT_6X10, DIM),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    // ---------------- 底部提示 ----------------
    let hint = if !sig.enabled {
        "FOU tunnel is OFF"
    } else if sig.up_on && sig.up_ip == 0 && sig.path == 0 {
        "Uplink Wi-Fi not connected"
    } else if sig.path == 0 {
        "Waiting for phone (AP or uplink)"
    } else if sig.tx > 0 && sig.rx == 0 {
        "No reply from phone - check tunnel"
    } else if sig.linked {
        "Traffic tunnelled via phone"
    } else {
        "Waiting for phone to join"
    };
    Text::with_baseline(
        hint,
        Point::new(8, 210),
        MonoTextStyle::new(&FONT_6X10, DIM),
        Baseline::Top,
    )
    .draw(d)
    .ok();
}

/// 「小标签 + 大号值」，值太长时自动降到 6x10，保证不出界
fn field<D>(d: &mut D, label: &str, value: &str, y: i32, col: Rgb565)
where
    D: DrawTarget<Color = Rgb565>,
{
    Text::with_baseline(
        label,
        Point::new(10, y),
        MonoTextStyle::new(&FONT_6X10, DIM),
        Baseline::Top,
    )
    .draw(d)
    .ok();

    let y2 = y + 14;
    let max = LCD_W - 20;
    if value.len() as i32 * 10 <= max {
        Text::with_baseline(
            value,
            Point::new(10, y2),
            MonoTextStyle::new(&FONT_10X20, col),
            Baseline::Top,
        )
        .draw(d)
        .ok();
    } else {
        // 32 字符的 SSID 用 6x10 也才 192px，一定放得下
        Text::with_baseline(
            value,
            Point::new(10, y2 + 5),
            MonoTextStyle::new(&FONT_6X10, col),
            Baseline::Top,
        )
        .draw(d)
        .ok();
    }
}

/// 左标签 + 右值的单行
fn kv<D>(d: &mut D, label: &str, value: &str, y: i32, style: MonoTextStyle<'_, Rgb565>)
where
    D: DrawTarget<Color = Rgb565>,
{
    Text::with_baseline(
        label,
        Point::new(10, y),
        MonoTextStyle::new(&FONT_6X10, DIM),
        Baseline::Top,
    )
    .draw(d)
    .ok();
    Text::with_baseline(
        value,
        Point::new(56, y),
        style,
        Baseline::Top,
    )
    .draw(d)
    .ok();
}
