//! 极简 DHCP 服务器（只做 AP 配网需要的那一小部分）。
//!
//! # 为什么要自己写
//!
//! 原来用的是 `esp-hal-dhcp-server` 的 `run_dhcp_server`，结果是：
//! 手机连上热点后一直停在「正在获取 IP 地址」，网页也打不开，
//! 而且**串口一行日志都没有** —— 既不知道它有没有收到包，也不知道它有没有回。
//!
//! 排查这类问题最需要的是可观测性，所以这里手写一个 200 行的版本，
//! 收到的每个包、发出的每个应答都打日志。
//!
//! # 协议要点（只实现够用的部分）
//!
//! ```text
//! 手机                                    设备
//!  |-- DISCOVER (0.0.0.0:68 -> 255.255.255.255:67) -->|
//!  |<-- OFFER   (192.168.4.1:67 -> 255.255.255.255:68) -|
//!  |-- REQUEST --------------------------------------->|
//!  |<-- ACK --------------------------------------------|
//! ```
//!
//! 关键点：客户端此时**还没有 IP**，所以应答必须发到广播地址
//! `255.255.255.255:68`。发到它将来的地址（yiaddr）在很多实现里会被丢掉，
//! 因为那个地址上的 ARP 还没建立。
//!
//! # 报文格式
//!
//! 固定头 236 字节 + magic cookie 4 字节，之后是 TLV 选项。
//! 我们不关心的大多数字段直接清零或原样复制请求里的值。

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Stack};
use embassy_time::Duration;

/// 设备在热点网段里的地址
const SERVER_IP: [u8; 4] = [192, 168, 4, 1];
/// 子网掩码
const NETMASK: [u8; 4] = [255, 255, 255, 0];
/// 广播地址（少数客户端会读 option 28）
const BROADCAST: [u8; 4] = [192, 168, 4, 255];
/// 地址池起止（含）
const POOL_START: u8 = 2;
const POOL_END: u8 = 10;
/// 租约时长（秒）
const LEASE_SECS: u32 = 3600;

/// 固定头长度（到 chaddr/file 结束，不含 cookie）
const BOOTP_LEN: usize = 236;
/// magic cookie
const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

/// DHCP 消息类型
const DHCPDISCOVER: u8 = 1;
const DHCPOFFER: u8 = 2;
const DHCPREQUEST: u8 = 3;
const DHCPACK: u8 = 5;

/// 最多记这么多个租约，超了就把新 MAC 复用最早的槽位
const MAX_LEASES: usize = 8;

/// MAC → IP 的简单映射。不做过期回收：配网阶段只有手机一个客户端，
/// 而且配完网热点就不再用了，没必要实现完整租约管理。
#[derive(Clone, Copy)]
struct Lease {
    mac: [u8; 6],
    last_octet: u8,
    used: bool,
}

impl Lease {
    const fn empty() -> Self {
        Self {
            mac: [0; 6],
            last_octet: 0,
            used: false,
        }
    }
}

#[embassy_executor::task]
pub async fn dhcp_task(stack: Stack<'static>) {
    // 这些缓冲放在任务栈上（主栈 111KB）。2KB 收发 + 元数据约 5KB。
    let mut rx_meta = [PacketMetadata::EMPTY; 8];
    let mut rx_buf = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 8];
    let mut tx_buf = [0u8; 1024];

    let mut sock = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buf,
        &mut tx_meta,
        &mut tx_buf,
    );

    // 先确认链路。AP 侧的 link_state 在 esp-radio 里等价于
    // 「access_point_state() == Started」，没起来的话下面收不到任何包。
    let mut waited = 0u32;
    while !stack.is_link_up() && waited < 50 {
        embassy_time::Timer::after(Duration::from_millis(100)).await;
        waited += 1;
    }
    log::info!(
        "[dhcp] 任务启动 link_up={} has_ipv4={}",
        stack.is_link_up(),
        stack.config_v4().is_some()
    );

    if let Err(e) = sock.bind(67u16) {
        log::error!("[dhcp] 绑定 67 端口失败: {e:?}（DHCP 不可用，手机将拿不到 IP）");
        return;
    }
    log::info!(
        "[dhcp] 已监听 0.0.0.0:67，地址池 192.168.4.{}~{}",
        POOL_START,
        POOL_END
    );

    let mut leases = [Lease::empty(); MAX_LEASES];
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 576];
    let mut pkt_count = 0u32;

    loop {
        let (len, meta) = match sock.recv_from(&mut rx).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("[dhcp] 收包出错: {e:?}");
                continue;
            }
        };
        pkt_count += 1;

        // ---- 基本校验 ----
        if len < BOOTP_LEN + 4 || rx[0] != 1 {
            // 不是 BOOTREQUEST，或其他程序的噪音
            log::debug!("[dhcp] 忽略非法包 len={len} op={}", rx[0]);
            continue;
        }
        if rx[BOOTP_LEN..BOOTP_LEN + 4] != COOKIE {
            log::warn!("[dhcp] magic cookie 不对，跳过");
            continue;
        }

        let msg_type = match find_opt(&rx[..len], 53).and_then(|v| v.first().copied()) {
            Some(t) => t,
            None => {
                log::warn!("[dhcp] 包里没有 option 53，跳过");
                continue;
            }
        };

        let mac: [u8; 6] = rx[28..34].try_into().unwrap();
        // xid 用来看请求/应答是否配对
        let xid = u32::from_be_bytes([rx[4], rx[5], rx[6], rx[7]]);

        log::info!(
            "[dhcp] 收到 {} 包 #{}  来自 {}  mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  xid={:#010x}  len={}",
            type_name(msg_type),
            pkt_count,
            meta.endpoint,
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
            xid,
            len
        );

        // ---- 选地址 ----
        let octet = match lease_for(&mut leases, mac) {
            Some(o) => o,
            None => {
                log::warn!("[dhcp] 地址池已满（{MAX_LEASES} 个租约），无法分配");
                continue;
            }
        };
        let yiaddr = [192, 168, 4, octet];

        let reply_type = match msg_type {
            DHCPDISCOVER => DHCPOFFER,
            DHCPREQUEST => DHCPACK,
            // RELEASE / DECLINE / INFORM 我们不需要
            other => {
                log::debug!("[dhcp] 忽略消息类型 {other}");
                continue;
            }
        };

        let n = build_reply(&rx[..len], reply_type, yiaddr, &mut tx);
        let dst = IpEndpoint::new(IpAddress::v4(255, 255, 255, 255), 68);

        match sock.send_to(&tx[..n], dst).await {
            Ok(()) => log::info!(
                "[dhcp] 发出 {} → 192.168.4.{} ({} 字节)",
                type_name(reply_type),
                octet,
                n
            ),
            Err(e) => log::error!("[dhcp] 应答发送失败: {e:?}"),
        }
    }
}

fn type_name(t: u8) -> &'static str {
    match t {
        DHCPDISCOVER => "DISCOVER",
        DHCPOFFER => "OFFER",
        DHCPREQUEST => "REQUEST",
        DHCPACK => "ACK",
        4 => "DECLINE",
        6 => "NAK",
        7 => "RELEASE",
        8 => "INFORM",
        _ => "UNKNOWN",
    }
}

/// 查这台设备的租约；没有就分配一个新的。
fn lease_for(leases: &mut [Lease; MAX_LEASES], mac: [u8; 6]) -> Option<u8> {
    // 已有租约直接复用
    for l in leases.iter() {
        if l.used && l.mac == mac {
            return Some(l.last_octet);
        }
    }
    // 找空槽 + 找一个没被占的末位
    // 用下标遍历，避免 `iter_mut` 与 `iter` 同时借用
    for i in 0..leases.len() {
        if leases[i].used {
            continue;
        }
        for cand in POOL_START..=POOL_END {
            let taken = leases.iter().any(|l| l.used && l.last_octet == cand);
            if !taken {
                leases[i].used = true;
                leases[i].mac = mac;
                leases[i].last_octet = cand;
                return Some(cand);
            }
        }
        return None;
    }
    None
}

/// 在 TLV 选项区里找某个 code 的值。
fn find_opt(pkt: &[u8], code: u8) -> Option<&[u8]> {
    let mut i = BOOTP_LEN + 4;
    while i + 1 < pkt.len() {
        let c = pkt[i];
        if c == 255 {
            return None; // END
        }
        if c == 0 {
            i += 1; // PAD
            continue;
        }
        let l = pkt[i + 1] as usize;
        if i + 2 + l > pkt.len() {
            return None;
        }
        if c == code {
            return Some(&pkt[i + 2..i + 2 + l]);
        }
        i += 2 + l;
    }
    None
}

/// 构造应答报文，返回长度。
fn build_reply(req: &[u8], msg_type: u8, yiaddr: [u8; 4], out: &mut [u8]) -> usize {
    debug_assert!(out.len() >= 300);
    out[..BOOTP_LEN].fill(0);

    out[0] = 2; // BOOTREPLY
    out[1] = req[1]; // htype（1 = 以太网）
    out[2] = req[2]; // hlen（6）
    out[3] = 0; // hops

    out[4..8].copy_from_slice(&req[4..8]); // xid 原样回
    out[8..10].copy_from_slice(&[0, 0]); // secs

    // flags：强制置 broadcast 位。客户端此刻没有 IP，单播很可能收不到。
    out[10] = 0x80;
    out[11] = 0x00;

    out[12..16].copy_from_slice(&[0; 4]); // ciaddr
    out[16..20].copy_from_slice(&yiaddr); // yiaddr = 我们分配的地址
    out[20..24].copy_from_slice(&SERVER_IP); // siaddr
    out[24..28].copy_from_slice(&[0; 4]); // giaddr

    // chaddr：16 字节，前 6 字节是 MAC，后面补零
    out[28..34].copy_from_slice(&req[28..34]);
    // sname(64) / file(128) 保持清零

    out[BOOTP_LEN..BOOTP_LEN + 4].copy_from_slice(&COOKIE);

    let mut o = BOOTP_LEN + 4;

    // option 53：消息类型
    out[o] = 53;
    out[o + 1] = 1;
    out[o + 2] = msg_type;
    o += 3;

    // option 54：服务器标识
    out[o] = 54;
    out[o + 1] = 4;
    out[o + 2..o + 6].copy_from_slice(&SERVER_IP);
    o += 6;

    // option 51：租约时长
    out[o] = 51;
    out[o + 1] = 4;
    out[o + 2..o + 6].copy_from_slice(&LEASE_SECS.to_be_bytes());
    o += 6;

    // option 1：子网掩码
    out[o] = 1;
    out[o + 1] = 4;
    out[o + 2..o + 6].copy_from_slice(&NETMASK);
    o += 6;

    // option 3：网关（指向自己，配网页就挂在这上面）
    out[o] = 3;
    out[o + 1] = 4;
    out[o + 2..o + 6].copy_from_slice(&SERVER_IP);
    o += 6;

    // option 6：DNS（指向自己，查不到就查不到，主要是避免客户端卡住）
    out[o] = 6;
    out[o + 1] = 4;
    out[o + 2..o + 6].copy_from_slice(&SERVER_IP);
    o += 6;

    // option 28：广播地址
    out[o] = 28;
    out[o + 1] = 4;
    out[o + 2..o + 6].copy_from_slice(&BROADCAST);
    o += 6;

    // option 255：结束
    out[o] = 255;
    o += 1;

    o
}

/// 让 `Ipv4Address` 常量在别处也能用（main 里配静态 IP 用得到）
pub const fn server_addr() -> Ipv4Address {
    Ipv4Address::new(SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3])
}
