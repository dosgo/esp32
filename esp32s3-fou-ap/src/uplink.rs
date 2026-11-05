//! 上行 Wi-Fi（STA）+ 隧道桥接。
//!
//! 工作方式：
//!   - ESP32 用 AP+STA 双模，一边给客户端当 AP，一边连到上游路由器；
//!   - 客户端的 IP 包仍然被 FOU 封装，但封装后的 UDP 从 station 网卡发出，
//!     目标是上游 LAN 里那台跑 PocketGateway 的手机；
//!   - 手机上解封装后照常出站，回包原路返回到 ESP32 的 station 网卡，
//!     这里再解封装、按 ARP 表组以太网帧交给 AP 发给客户端。
//!
//! 为什么不用 AP 网卡发：上行模式下 FOU 节点不在 AP 网段里，它的 MAC 只能
//! 通过上游网段的 ARP 得到，交给 embassy-net 的 station 协议栈处理最省事。

use alloc::string::String as AllocString;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::ap::AccessPointConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethod, Config as WifiConfig, WifiController};
use log::{info, warn};

use crate::state;

const STA_RX_META: usize = 8;
const STA_TX_META: usize = 8;
const STA_RX_BUF: usize = 4096;
const STA_TX_BUF: usize = 4096;

/// 上行连不上时的重试间隔
const RETRY: Duration = Duration::from_secs(8);

fn sta_buffers() -> (
    &'static mut [PacketMetadata],
    &'static mut [u8],
    &'static mut [PacketMetadata],
    &'static mut [u8],
) {
    static RX_META: static_cell::StaticCell<[PacketMetadata; STA_RX_META]> =
        static_cell::StaticCell::new();
    static RX_BUF: static_cell::StaticCell<[u8; STA_RX_BUF]> = static_cell::StaticCell::new();
    static TX_META: static_cell::StaticCell<[PacketMetadata; STA_TX_META]> =
        static_cell::StaticCell::new();
    static TX_BUF: static_cell::StaticCell<[u8; STA_TX_BUF]> = static_cell::StaticCell::new();
    (
        RX_META.init([PacketMetadata::EMPTY; STA_RX_META]),
        RX_BUF.init([0; STA_RX_BUF]),
        TX_META.init([PacketMetadata::EMPTY; STA_TX_META]),
        TX_BUF.init([0; STA_TX_BUF]),
    )
}

fn resources() -> &'static mut StackResources<4> {
    static CELL: static_cell::StaticCell<StackResources<4>> = static_cell::StaticCell::new();
    CELL.init(StackResources::<4>::new())
}

/// 在 main 里建 station 协议栈（DHCP 客户端），返回 stack 和 runner
pub fn new_station(iface: esp_radio::wifi::Interface<'static>, seed: u64) -> (Stack<'static>, Runner<'static, esp_radio::wifi::Interface<'static>>) {
    let cfg = embassy_net::Config::dhcpv4(Default::default());
    embassy_net::new(iface, cfg, resources(), seed)
}

/// DHCP 客户端：只管把拿到的地址写进 state，别的任务从 state 读
#[embassy_executor::task]
pub async fn sta_net_task(mut runner: Runner<'static, esp_radio::wifi::Interface<'static>>) {
    runner.run().await
}

/// 盯着 station 的地址变化，同步到 state
#[embassy_executor::task]
pub async fn sta_monitor_task(stack: Stack<'static>) {
    let mut last = core::net::Ipv4Addr::UNSPECIFIED;
    loop {
        let now = stack
            .config_v4()
            .map(|c| c.address.address())
            .unwrap_or(core::net::Ipv4Addr::UNSPECIFIED);
        if now != last {
            last = now;
            state::set_uplink_ip(now);
            if now.is_unspecified() {
                info!("uplink: no address");
            } else {
                info!("uplink: got address {}", now);
            }
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

fn ap_config(ssid: &str, pass: &str, channel: u8) -> AccessPointConfig {
    AccessPointConfig::default()
        .with_ssid(AllocString::from(ssid))
        .with_password(AllocString::from(pass))
        .with_channel(channel)
        .with_auth_method(AuthenticationMethod::Wpa2Personal)
        .with_max_connections(8)
}

fn sta_config(ssid: &str, pass: &str) -> StationConfig {
    let mut c = StationConfig::default()
        .with_ssid(AllocString::from(ssid))
        .with_password(AllocString::from(pass));
    c = c.with_auth_method(if pass.is_empty() {
        AuthenticationMethod::None
    } else {
        AuthenticationMethod::Wpa2Personal
    });
    c
}

/// 主任务：按配置切换「只当 AP」和「AP+STA」，连上后跑桥接
#[embassy_executor::task]
pub async fn uplink_task(controller: WifiController<'static>, stack: Stack<'static>) {
    let mut controller = controller;
    // 记住当前生效的模式，避免每次循环都重配（重配会把客户端踢下线）
    let mut mode_applied: Option<bool> = None;

    loop {
        let (ssid, pass, want) = state::uplink_copy();
        let want = want && !ssid.is_empty();

        if !want {
            state::set_uplink_on(false);
            state::set_uplink_ip(core::net::Ipv4Addr::UNSPECIFIED);
            state::set_uplink_state(0);
            if mode_applied == Some(true) {
                // 从双模退回单 AP
                let (ap_ssid, ap_pass) = state::net_copy();
                let cfg = WifiConfig::AccessPoint(ap_config(&ap_ssid, &ap_pass, 6));
                if let Err(e) = controller.set_config(&cfg) {
                    warn!("uplink: back to AP-only failed: {:?}", e);
                } else {
                    info!("uplink: disabled, AP only");
                }
                mode_applied = Some(false);
            }
            state::UP_DIRTY.wait().await;
            continue;
        }

        // AP 信道设 0 = 跟随 STA：双模只有一块射频，两者必须同信道
        let (ap_ssid, ap_pass) = state::net_copy();
        let cfg = WifiConfig::AccessPointStation(
            sta_config(&ssid, &pass),
            ap_config(&ap_ssid, &ap_pass, 0),
        );
        if let Err(e) = controller.set_config(&cfg) {
            warn!("uplink: apply APSTA failed: {:?}", e);
            state::set_uplink_state(3);
            Timer::after(RETRY).await;
            continue;
        }
        mode_applied = Some(true);
        state::set_uplink_on(true);
        state::set_uplink_state(1);
        info!("uplink: connecting to {}", ssid.as_str());

        // 连接（带超时，避免一个错的密码把任务卡死）
        let joined = match select(
            controller.connect_async(),
            Timer::after(Duration::from_secs(20)),
        )
        .await
        {
            Either::First(Ok(info_)) => {
                info!("uplink: joined {:?}", info_);
                true
            }
            Either::First(Err(e)) => {
                warn!("uplink: connect failed: {:?}", e);
                false
            }
            Either::Second(_) => {
                warn!("uplink: connect timeout");
                false
            }
        };

        if !joined {
            state::set_uplink_state(3);
            Timer::after(RETRY).await;
            continue;
        }

        state::set_uplink_state(2);
        match controller.channel() {
            Ok((ch, _)) => {
                state::set_uplink_channel(ch);
                info!("uplink: channel {} (AP follows)", ch);
            }
            Err(e) => warn!("uplink: read channel failed: {:?}", e),
        }

        // 桥接：一直跑到链路掉线或配置被改
        let keep = bridge(stack).await;
        state::set_uplink_state(if keep { 1 } else { 3 });
        state::set_uplink_ip(core::net::Ipv4Addr::UNSPECIFIED);
        if !keep {
            Timer::after(RETRY).await;
        }
    }
}

/// 隧道双向搬运。返回 false 表示链路掉了，需要上层重连
async fn bridge(stack: Stack<'static>) -> bool {
    let (rx_meta, rx_buf, tx_meta, tx_buf) = sta_buffers();
    let mut sock = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);

    // 收发都用同一个端口：手机把回包发回给"它收到的那个源端口"
    let port = state::fou_port();
    if sock.bind(port).is_err() {
        warn!("uplink: bind {} failed", port);
        return false;
    }
    info!("uplink: bridge ready on 0.0.0.0:{}", port);

    let mut pkt = [0u8; 1500];
    loop {
        if !stack.is_link_up() || state::UP_DIRTY.signaled() {
            state::UP_DIRTY.reset();
            return false;
        }

        // 三路等待：手机回包 / 客户端出向包 / 3 秒超时（顺便探链路）
        match select3(
            sock.recv_from(&mut pkt),
            state::UP_Q.receive(),
            Timer::after(Duration::from_secs(3)),
        )
        .await
        {
            // 手机 → ESP32：解封装后交给 AP 侧组装以太网帧
            Either3::First(Ok((n, _from))) => {
                if n < 20 || pkt[0] >> 4 != 4 {
                    continue;
                }
                state::FWD_IN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                state::TUN_BYTES_IN.fetch_add(n as u32, core::sync::atomic::Ordering::Relaxed);
                let mut v: heapless::Vec<u8, 1500> = heapless::Vec::new();
                if v.extend_from_slice(&pkt[..n]).is_ok() {
                    if state::AP_RX_Q.try_send(v).is_err() {
                        state::DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            Either3::First(Err(_)) => return false,
            // ESP32 → 手机：客户端流量封装好后从这里发出
            Either3::Second(pkt) => {
                let ip = state::fou_ip();
                let o = ip.octets();
                let ep = IpEndpoint::new(IpAddress::v4(o[0], o[1], o[2], o[3]), state::fou_port());
                if sock.send_to(&pkt, ep).await.is_err() {
                    state::DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                }
                state::TUN_BYTES_OUT
                    .fetch_add(pkt.len() as u32, core::sync::atomic::Ordering::Relaxed);
            }
            Either3::Third(_) => {
                if !stack.is_link_up() {
                    return false;
                }
            }
        }
    }
}
