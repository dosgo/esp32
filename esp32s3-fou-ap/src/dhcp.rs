//! 自带的极简 DHCP 服务器。
//!
//! 为什么不用 `esp-hal-dhcp-server`：它把 socket 绑在 `0.0.0.0:67`，而 smoltcp 在
//! `local_address` 为空时会拿 socket 的绑定地址当源地址，于是 OFFER/ACK 的源 IP
//! 变成 `0.0.0.0`。手机内核把源地址为 0.0.0.0 的入站包当 martian 直接丢掉，
//! 现象就是「能搜到 AP、一直转圈连不上」，而抓包看 OFFER 明明已经发出去了。
//!
//! 这里改成发送时显式指定 `local_address = 192.168.4.1`，其余编解码仍用 edge-dhcp。

use core::net::Ipv4Addr;

use edge_dhcp::{server::Action, DhcpOption, MessageType, Options, Packet};
use embassy_net::udp::{PacketMetadata, UdpSocket, UdpMetadata};
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_time::{Duration, Instant};

use esp_hal_dhcp_server::simple_leaser::SimpleDhcpLeaser;
use esp_hal_dhcp_server::structs::DhcpLeaser;

use crate::{GW_IP, INNER_MTU, POOL_END, POOL_START, PUBLIC_DNS1, PUBLIC_DNS2};

const SERVER_PORT: u16 = 67;
const CLIENT_PORT: u16 = 68;
const LEASE_SECS: u32 = 7200;
const SUBNET: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const BUF: usize = 1024;

pub async fn run(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 8];
    let mut tx_meta = [PacketMetadata::EMPTY; 8];
    let mut rx_buffer = [0u8; BUF];
    let mut tx_buffer = [0u8; BUF];
    let mut sock = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );

    // 必须绑 0.0.0.0:67 —— DISCOVER 是广播到 255.255.255.255，
    // 绑到具体地址就收不到包了
    if let Err(e) = sock.bind(SERVER_PORT) {
        log::error!("DHCP bind failed: {:?}", e);
        loop {
            embassy_time::Timer::after(Duration::from_secs(3600)).await;
        }
    }

    let mut leaser = SimpleDhcpLeaser {
        start: POOL_START,
        end: POOL_END,
        leases: Default::default(),
    };

    let mut buf = [0u8; BUF];
    loop {
        let (n, _meta) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Err(_e) = handle(&mut sock, &buf[..n], &mut leaser).await {
            log::warn!("DHCP handle failed: {:?}", _e);
        }
    }
}

async fn handle(
    sock: &mut UdpSocket<'_>,
    data: &[u8],
    leaser: &mut SimpleDhcpLeaser,
) -> Result<(), ()> {
    let packet = Packet::decode(data).map_err(|_| ())?;
    if packet.reply {
        return Ok(());
    }

    let Some(action) = action_for(&packet) else {
        return Ok(());
    };

    match action {
        Action::Discover(_want, mac) => {
            let ip = leaser.get_lease(*mac).map(|l| l.ip).or_else(|| leaser.next_lease());
            log::info!("DHCP DISCOVER from {} -> offer {}", fmt_mac(mac), fmt_ip(&ip));
            if ip.is_some() {
                send(sock, &packet, MessageType::Offer, ip).await;
            }
        }
        Action::Request(want, mac) => {
            // 客户端在 INIT-REBOOT 时可能报一个我们网段外的地址，必须拒
            let available = match leaser.get_lease(*mac) {
                Some(l) => l.ip == want || Instant::now() > l.expires,
                None => true,
            };
            let ok = available
                && leaser.add_lease(
                    want,
                    *mac,
                    Instant::now() + Duration::from_secs(LEASE_SECS as u64),
                );
            let ip = if ok { Some(want) } else { None };
            log::info!(
                "DHCP REQUEST {} want={} -> {}",
                fmt_mac(mac),
                fmt_ip(&Some(want)),
                if ok { "ACK" } else { "NAK" }
            );
            send(
                sock,
                &packet,
                if ok { MessageType::Ack } else { MessageType::Nak },
                ip,
            )
            .await;
        }
        Action::Release(_ip, mac) | Action::Decline(_ip, mac) => {
            leaser.remove_lease(*mac);
        }
    }
    Ok(())
}

/// 从 options 里判断这是哪种请求，顺便过滤掉不是发给本机的
fn action_for<'b>(packet: &'b Packet<'b>) -> Option<Action<'b>> {
    let mt = packet.options.iter().find_map(|o| match o {
        DhcpOption::MessageType(t) => Some(t),
        _ => None,
    })?;

    let sid = packet.options.iter().find_map(|o| match o {
        DhcpOption::ServerIdentifier(ip) => Some(ip),
        _ => None,
    });
    if let Some(s) = sid {
        if s != GW_IP {
            return None;
        }
    }

    let requested = packet.options.iter().find_map(|o| match o {
        DhcpOption::RequestedIpAddress(ip) => Some(ip),
        _ => None,
    });

    match mt {
        MessageType::Discover => Some(Action::Discover(requested, &packet.chaddr)),
        MessageType::Request => {
            let want = requested.or({
                if packet.ciaddr.is_unspecified() {
                    None
                } else {
                    Some(packet.ciaddr)
                }
            })?;
            Some(Action::Request(want, &packet.chaddr))
        }
        MessageType::Release => Some(Action::Release(packet.yiaddr, &packet.chaddr)),
        MessageType::Decline => Some(Action::Decline(packet.yiaddr, &packet.chaddr)),
        _ => None,
    }
}

async fn send(sock: &mut UdpSocket<'_>, packet: &Packet<'_>, mt: MessageType, ip: Option<Ipv4Addr>) {
    let mut buf = [0u8; BUF];
    let len = build_reply(&mut buf, packet, mt, ip);

    // 关键：显式给出源地址，否则 smoltcp 会用 socket 的 0.0.0.0
    let meta = UdpMetadata {
        endpoint: IpEndpoint::new(IpAddress::v4(255, 255, 255, 255), CLIENT_PORT),
        local_address: Some(IpAddress::v4(
            GW_IP.octets()[0],
            GW_IP.octets()[1],
            GW_IP.octets()[2],
            GW_IP.octets()[3],
        )),
        meta: Default::default(),
    };
    if sock.send_to(&buf[..len], meta).await.is_err() {
        log::warn!("DHCP reply send failed");
    }
}

/// 手工拼 DHCP 回包。
///
/// 不用 edge_dhcp 的 `options.reply()`：它只在客户端 PRL 里列了某项才回对应 option，
/// 而且整包长度往往不到 300 字节。手机对「缺 subnet mask / 短包」的处理是静默丢弃。
/// 这里固定写 message type / server id / lease / subnet / router / dns / broadcast，
/// 并 pad 到 300 字节（RFC 2131 要求的最小长度）。
fn build_reply(buf: &mut [u8], packet: &Packet<'_>, mt: MessageType, ip: Option<Ipv4Addr>) -> usize {
    let code = match mt {
        MessageType::Offer => 2u8,
        MessageType::Ack => 5u8,
        MessageType::Nak => 6u8,
        _ => 2u8,
    };

    for b in buf.iter_mut() {
        *b = 0;
    }

    buf[0] = 2; // BOOTREPLY
    buf[1] = 1; // Ethernet
    buf[2] = 6; // MAC 长度
    buf[4..8].copy_from_slice(&packet.xid.to_be_bytes());
    // secs 保持 0
    if packet.broadcast {
        buf[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
    }
    // ciaddr 保持 0：我们是广播回包，客户端还没配上地址
    if let Some(i) = ip {
        buf[16..20].copy_from_slice(&i.octets()); // yiaddr
    }
    buf[20..24].copy_from_slice(&GW_IP.octets()); // siaddr
    buf[28..44].copy_from_slice(&packet.chaddr);
    buf[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]); // magic cookie

    let mut o = 240usize;
    let mut opt = |buf: &mut [u8], o: &mut usize, code: u8, val: &[u8]| {
        buf[*o] = code;
        buf[*o + 1] = val.len() as u8;
        buf[*o + 2..*o + 2 + val.len()].copy_from_slice(val);
        *o += 2 + val.len();
    };

    opt(buf, &mut o, 53, &[code]);
    opt(buf, &mut o, 54, &GW_IP.octets());
    opt(buf, &mut o, 51, &LEASE_SECS.to_be_bytes());

    if !matches!(mt, MessageType::Nak) {
        opt(buf, &mut o, 1, &SUBNET.octets());
        opt(buf, &mut o, 3, &GW_IP.octets());
        let mut dns = [0u8; 8];
        dns[0..4].copy_from_slice(&PUBLIC_DNS1.octets());
        dns[4..8].copy_from_slice(&PUBLIC_DNS2.octets());
        opt(buf, &mut o, 6, &dns);
        opt(buf, &mut o, 28, &[192, 168, 4, 255]);
        // 接口 MTU：外层 FOU 要 28 字节头，客户端用 1400 就不会触发 IP 分片
        opt(buf, &mut o, 26, &INNER_MTU.to_be_bytes());
    }

    buf[o] = 255; // end
    o += 1;

    // 补到 300 字节，多余位置是 PAD(0)，客户端会忽略
    o.max(300)
}

fn fmt_ip(ip: &Option<Ipv4Addr>) -> alloc::string::String {
    match ip {
        Some(i) => alloc::format!("{}", i),
        None => alloc::string::String::from("none"),
    }
}

fn fmt_mac(m: &[u8; 16]) -> alloc::string::String {
    alloc::format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}
