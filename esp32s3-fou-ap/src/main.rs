//! ESP32-S3 FOU 网关
//!
//! 起一个 AP，把 AP 下客户端的 IPv4 包整条塞进 UDP（FOU），发给同一 AP 下的
//! 安卓手机（跑 PocketGateway），由手机上的 gVisor netstack 出站上网。
//!
//! 手机端的 5555/udp FOU 监听一行都不用改 —— 它本来就是收 IP-in-UDP。

#![no_std]
#![no_main]

extern crate alloc;

mod apdrv;
mod dhcp;
mod display;
mod http;
mod state;
mod uplink;

use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_bootloader_esp_idf::esp_app_desc;
use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{AuthenticationMethod, Config as WifiConfig, ControllerConfig};
use esp_radio::wifi::ap::AccessPointConfig;
use log::{info, warn};

esp_app_desc!();

/// 网关自己在 AP 网段里的地址
pub const GW_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
/// 网段掩码位数
pub const GW_PREFIX: u8 = 24;
/// DHCP 地址池
pub const POOL_START: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 2);
pub const POOL_END: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 200);
/// AP 接口 MTU。FOU 外层要塞 28 字节 UDP+IP 头，内层限到 1400 就不会二次分片。
pub const INNER_MTU: u16 = 1400;
/// 下发 / 展示用的公网 DNS。它们不在 AP 网段，查询会被隧道带到手机侧解析。
pub const PUBLIC_DNS1: Ipv4Addr = Ipv4Addr::new(223, 5, 5, 5);
pub const PUBLIC_DNS2: Ipv4Addr = Ipv4Addr::new(119, 29, 29, 29);

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    esp_alloc::heap_allocator!(size: 96 * 1024);

    let hw = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let timg0 = TimerGroup::new(hw.TIMG0);
    let sw_int = SoftwareInterruptControl::new(hw.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("=== PocketGateway ESP32-S3 FOU gateway booting ===");

    state::init();
    let (ssid, pass) = state::net_copy();
    let (up_ssid, up_pass, up_on) = state::uplink_copy();
    let up_on = up_on && !up_ssid.is_empty();

    let ap_config = AccessPointConfig::default()
        .with_ssid(alloc::string::String::from(ssid.as_str()))
        .with_password(alloc::string::String::from(pass.as_str()))
        // 双模时 AP 必须跟着 STA 的信道走，0 = 自动跟随
        .with_channel(if up_on { 0 } else { 6 })
        .with_auth_method(AuthenticationMethod::Wpa2Personal)
        .with_max_connections(8);

    let wifi_config = if up_on {
        // 上行：AP + STA 双模，两个接口共用一块射频
        WifiConfig::AccessPointStation(
            esp_radio::wifi::sta::StationConfig::default()
                .with_ssid(alloc::string::String::from(up_ssid.as_str()))
                .with_password(alloc::string::String::from(up_pass.as_str())),
            ap_config,
        )
    } else {
        WifiConfig::AccessPoint(ap_config)
    };

    let (mut controller, interfaces) = esp_radio::wifi::new(
        hw.WIFI,
        ControllerConfig::default().with_initial_config(wifi_config),
    )
    .expect("WiFi 初始化失败");

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let net_config = NetConfig::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(GW_IP, GW_PREFIX),
        gateway: None,
        dns_servers: heapless::Vec::new(),
    });

    let ap_iface = interfaces.access_point;
    state::set_ap_mac(ap_iface.mac_address());
    let (stack, runner) = embassy_net::new(
        apdrv::ApDriver::new(ap_iface),
        net_config,
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        seed,
    );

    // 上行用的 station 协议栈：DHCP 客户端拿上游路由器的地址
    let (sta_stack, sta_runner) = uplink::new_station(interfaces.station, seed ^ 0x5a5a);

    info!("AP started: SSID={} gw={}", ssid.as_str(), GW_IP);

    // 默认只开了 Start/Stop/Connect 这几类事件，ProbeRequest 要自己打开，
    // 否则手机在扫但连不上时串口一片安静，看不出问题在哪
    {
        use esp_radio::wifi::event::{enable_wifi_events, WifiEvent};
        enable_wifi_events(
            WifiEvent::AccessPointProbeRequestReceived
                | WifiEvent::AccessPointStationConnected
                | WifiEvent::AccessPointStationDisconnected
                | WifiEvent::AccessPointStart
                | WifiEvent::AccessPointStop,
        );
    }

    match controller.channel() {
        Ok((ch, _)) => info!("AP channel = {}", ch),
        Err(e) => warn!("read AP channel failed: {:?}", e),
    }

    // 2.4G 默认只跑 HT20（72 Mbps PHY）。HT40 能翻倍到 150 Mbps，
    // 但会占 4~10 两个信道，邻居多的时候反而更慢 —— 所以做成可配，
    // 默认开 40M，干扰大时用网页关掉。
    //
    // esp-radio 0.18 只暴露 `set_bandwidths(Bandwidths)`，但 `Bandwidths` 既没实现
    // Default 也没有构造函数，外部根本造不出实例（这个开关等于死的），
    // 这里直接调 blob 的 FFI。
    {
        if state::wide_band() {
            unsafe {
                let mut bw = esp_wifi_sys_esp32s3::include::wifi_bandwidths_t {
                    ghz_2g: esp_wifi_sys_esp32s3::include::wifi_bandwidth_t_WIFI_BW_HT40,
                    ghz_5g: 0,
                };
                let r = esp_wifi_sys_esp32s3::include::esp_wifi_set_bandwidths(
                    esp_wifi_sys_esp32s3::include::wifi_interface_t_WIFI_IF_AP,
                    &mut bw,
                );
                if r == 0 {
                    info!("AP bandwidth = HT40");
                } else {
                    warn!("set HT40 failed: {}", r);
                }
            }
        }
        match controller.bandwidths() {
            Ok(bw) => info!("bandwidths = {:?}", bw),
            Err(e) => warn!("read bandwidth failed: {:?}", e),
        }
    }

    // controller 交给 uplink 任务持有（Wi-Fi 停不停取决于它活着）
    spawner.spawn(uplink::uplink_task(controller, sta_stack).expect("spawn uplink"));
    spawner.spawn(uplink::sta_net_task(sta_runner).expect("spawn sta_net"));
    spawner.spawn(uplink::sta_monitor_task(sta_stack).expect("spawn sta_monitor"));
    spawner.spawn(net_task(runner).expect("spawn net_task"));
    spawner.spawn(dhcp_task(stack).expect("spawn dhcp"));
    spawner.spawn(http::web_task(stack).expect("spawn http"));
    spawner.spawn(stat_task(stack).expect("spawn stat"));

    // 板载 1.54" ST7789：SCLK=38 MOSI=39 CS=21 DC=45 RST=40 BL=46
    // 这些脚和 Wi-Fi/USB 都不冲突（USB 是 19/20）
    spawner.spawn(
        display::display_task(
            hw.SPI2,
            hw.GPIO38.into(),
            hw.GPIO39.into(),
            hw.GPIO21.into(),
            hw.GPIO45.into(),
            hw.GPIO40.into(),
            hw.GPIO46.into(),
        )
        .expect("spawn display"),
    );

    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, apdrv::ApDriver<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    dhcp::run(stack).await
}

/// 每 5 秒把状态打到串口，方便不开网页时也能看。
/// 附带 5 秒窗口的隧道速率 —— 判断瓶颈全靠这两个数：
/// out 就是"手机 → ESP32 → 客户端"方向实际搬了多少字节。
#[embassy_executor::task]
async fn stat_task(stack: Stack<'static>) {
    let mut prev_out: u32 = 0;
    let mut prev_in: u32 = 0;
    loop {
        Timer::after(Duration::from_secs(5)).await;
        let out = state::TUN_BYTES_OUT.load(core::sync::atomic::Ordering::Relaxed);
        let inp = state::TUN_BYTES_IN.load(core::sync::atomic::Ordering::Relaxed);
        // 回绕安全：u32 减法天然按 2^32 取模
        let kbps_out = out.wrapping_sub(prev_out) / 5 / 1024;
        let kbps_in = inp.wrapping_sub(prev_in) / 5 / 1024;
        prev_out = out;
        prev_in = inp;

        let s = state::snapshot();
        info!(
            "gw={} fou={}:{} up={} client={} tx={} rx={} drop={} loop={} spd out={}KB/s in={}KB/s | air f={} arp={} ip={} dhcp={} oth={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            stack.config_v4().map(|c| c.address).unwrap_or(embassy_net::Ipv4Cidr::new(Ipv4Addr::UNSPECIFIED, 0)),
            state::fou_ip(),
            state::fou_port(),
            s.up,
            s.client_ip,
            s.fwd_out,
            s.fwd_in,
            s.dropped,
            s.loopback,
            kbps_out,
            kbps_in,
            s.rx_frames,
            s.rx_arp,
            s.rx_ip,
            s.rx_dhcp,
            s.rx_other,
            s.rx_mac[0],
            s.rx_mac[1],
            s.rx_mac[2],
            s.rx_mac[3],
            s.rx_mac[4],
            s.rx_mac[5],
        );
        info!(
            "  uplink on={} state={} ip={} ch={} target={} route={} path={}",
            s.up_on,
            s.up_state,
            if s.up_ip.is_unspecified() { 0 } else { u32::from(s.up_ip) },
            s.up_ch,
            s.up_target,
            s.route_mode,
            s.path
        );
        info!("  dhcp_rx: {}", state::last_dhcp(true).as_str());
        info!("  dhcp_tx: {}", state::last_dhcp(false).as_str());
    }
}
