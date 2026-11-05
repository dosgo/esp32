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
mod config;
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
    // 驱动 RX/TX 队列和 AMPDU BA 窗口都调大了，给 WiFi 驱动留足堆。
    esp_alloc::heap_allocator!(size: 160 * 1024);

    let hw = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    // Flash 外设先拿出来：配置要在 Wi-Fi 初始化之前恢复
    let mut flash = hw.FLASH;
    let timg0 = TimerGroup::new(hw.TIMG0);
    let sw_int = SoftwareInterruptControl::new(hw.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("=== PocketGateway ESP32-S3 FOU gateway booting ===");

    state::init();
    config::load_early(&mut flash);
    let (ssid, pass) = state::net_copy();
    let (up_ssid, up_pass, up_on) = state::uplink_copy();
    let up_on = up_on && !up_ssid.is_empty();

    let ap_config = AccessPointConfig::default()
        .with_ssid(alloc::string::String::from(ssid.as_str()))
        .with_password(alloc::string::String::from(pass.as_str()))
        // 双模时 AP 必须跟着 STA 的信道走，0 = 自动跟随
        .with_channel(if up_on { 0 } else { state::ap_channel() })
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

    // esp-radio 默认值是给低吞吐场景的：tx_queue_size=3 / rx_queue_size=5。
    // AP 转发每包都要 RX+TX，而 rx_token() 只有在 tx_token() 拿到额度时才返回，
    // 3 个在途帧会把整条数据面（连收包一起）锁死；RX 队列满 5 帧还会直接丢包。
    let radio_cfg = ControllerConfig::default()
        .with_tx_queue_size(16)
        .with_rx_queue_size(24)
        .with_static_rx_buf_num(16)
        .with_rx_ba_win(12)
        .with_initial_config(wifi_config);
    let (mut controller, interfaces) =
        esp_radio::wifi::new(hw.WIFI, radio_cfg).expect("WiFi 初始化失败");

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

    // ---- 空口初始化 ----------------------------------------------------
    // 之前这里只调了 set_bandwidths(HT40)，从没调
    // set_channel(ch, SECOND_CHAN_ABOVE)。IDF 里 HT40 的扩展信道位置是由
    // set_channel 的第二个参数决定的，缺了它驱动只跑主 20MHz —— 日志会显示
    // HT40，实际速率却还是 HT20。而且 apply_rf() 以前只有网页点保存才调用，
    // 持久化恢复的信道/带宽配置在重启后完全没作用到驱动。
    // 这里统一执行一次，让上电即生效。
    {
        use esp_wifi_sys_esp32s3::include as wifi;

        // AP 不需要省电。开着会让驱动对 STA 做 TIM/DTIM 缓冲，白白增加延迟。
        let r = unsafe { wifi::esp_wifi_set_ps(wifi::wifi_ps_type_t_WIFI_PS_NONE) };
        info!("wifi power save off: {}", r);

        // 20 dBm（参数单位是 0.25 dBm）。AP 发射功率直接决定客户端能用的 MCS。
        let r = unsafe { wifi::esp_wifi_set_max_tx_power(80) };
        info!("wifi tx power 20dBm: {}", r);

        // 试过用 esp_wifi_set_protocol 去掉 11b、免掉 ERP protection 开销，
        // 但 IDF 规定 2.4G 的协议位图必须含 11b（不带就返回 0x102
        // INVALID_ARG）。好消息是 protection 只在真有 11b-only STA 关联时才
        // 启用，这里全是 11n 手机，本来就没这笔开销，所以保持默认即可。

        apply_rf(state::ap_channel(), state::wide_band());
    }

    match controller.channel() {
        Ok((ch, second)) => info!("AP channel = {} secondary = {:?}", ch, second),
        Err(e) => warn!("read AP channel failed: {:?}", e),
    }
    match controller.bandwidths() {
        Ok(bw) => info!("bandwidths = {:?}", bw),
        Err(e) => warn!("read bandwidth failed: {:?}", e),
    }

    // controller 交给 uplink 任务持有（Wi-Fi 停不停取决于它活着）
    spawner.spawn(uplink::uplink_task(controller, sta_stack).expect("spawn uplink"));
    spawner.spawn(uplink::sta_net_task(sta_runner).expect("spawn sta_net"));
    spawner.spawn(uplink::sta_monitor_task(sta_stack).expect("spawn sta_monitor"));
    spawner.spawn(net_task(runner).expect("spawn net_task"));
    spawner.spawn(dhcp_task(stack).expect("spawn dhcp"));
    spawner.spawn(http::web_task(stack).expect("spawn http"));
    spawner.spawn(stat_task(stack).expect("spawn stat"));
    spawner.spawn(config::config_task(flash).expect("spawn config"));

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

/// 运行时切 2.4G 信道 / 带宽，网页保存后立即生效。
/// AP+STA 双模时两块接口共用一块射频，信道必须跟 STA 一致，
/// 所以只改带宽，不动信道。
pub fn apply_rf(ch: u8, wide: bool) {
    use esp_wifi_sys_esp32s3::include as wifi;

    if !state::uplink_on() {
        let second = if wide {
            wifi::wifi_second_chan_t_WIFI_SECOND_CHAN_ABOVE
        } else {
            wifi::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE
        };
        let r = unsafe { wifi::esp_wifi_set_channel(ch, second) };
        if r == 0 {
            info!("AP channel -> {}", ch);
        } else {
            warn!("set channel {} failed: {}", ch, r);
        }
    }

    let mut bw = wifi::wifi_bandwidths_t {
        ghz_2g: if wide {
            wifi::wifi_bandwidth_t_WIFI_BW_HT40
        } else {
            wifi::wifi_bandwidth_t_WIFI_BW_HT20
        },
        ghz_5g: 0,
    };
    let r = unsafe {
        wifi::esp_wifi_set_bandwidths(wifi::wifi_interface_t_WIFI_IF_AP, &mut bw)
    };
    if r == 0 {
        info!("AP bandwidth -> {}", if wide { "HT40" } else { "HT20" });
    } else {
        warn!("set bandwidth failed: {}", r);
    }
}

/// 每 5 秒把状态打到串口，方便不开网页时也能看。
/// 附带 5 秒窗口的隧道速率 —— 判断瓶颈全靠这两个数：
/// out 就是"手机 → ESP32 → 客户端"方向实际搬了多少字节。
#[embassy_executor::task]
async fn stat_task(stack: Stack<'static>) {
    let mut prev_out: u32 = 0;
    let mut prev_in: u32 = 0;
    // 串口是同步阻塞写，一次约 500B @115200 ≈ 45ms。原来的 5 秒周期会每 5 秒
    // 卡住 executor 一次，降成 30 秒，流量计数按 30 秒窗口算平均。
    const PERIOD: u32 = 30;
    loop {
        Timer::after(Duration::from_secs(PERIOD as u64)).await;
        let out = state::TUN_BYTES_OUT.load(core::sync::atomic::Ordering::Relaxed);
        let inp = state::TUN_BYTES_IN.load(core::sync::atomic::Ordering::Relaxed);
        // 回绕安全：u32 减法天然按 2^32 取模
        let kbps_out = out.wrapping_sub(prev_out) / PERIOD / 1024;
        let kbps_in = inp.wrapping_sub(prev_in) / PERIOD / 1024;
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
        // 空口诊断：确认 HT40 / 11n 是否真的协商上了，以及信号强度。
        use esp_wifi_sys_esp32s3::include as wifi;
        let mut list: wifi::wifi_sta_list_t = unsafe { core::mem::zeroed() };
        let r = unsafe { wifi::esp_wifi_ap_get_sta_list(&mut list) };
        if r == 0 {
            let n = (list.num.max(0) as usize).min(4);
            let mut d = alloc::string::String::new();
            for i in 0..n {
                let st = &list.sta[i];
                let _ = core::fmt::Write::write_fmt(
                    &mut d,
                    format_args!(
                        " [{}] rssi={}dBm 11n={} 11g={} 11b={}",
                        i,
                        st.rssi,
                        st.phy_11n(),
                        st.phy_11g(),
                        st.phy_11b()
                    ),
                );
            }
            info!("  air sta={}{}", list.num, d.as_str());
        } else {
            info!("  air sta list failed: {}", r);
        }

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
