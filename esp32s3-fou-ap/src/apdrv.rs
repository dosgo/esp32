//! 自定义 Wi-Fi AP 驱动：在以太网帧层把「发给网关的外部流量」整条塞进 FOU。
//!
//! esp-radio 的 `Interface` 本身就是一个 `embassy_net_driver::Driver`，这里包一层：
//!
//! ```text
//! 收帧 ──┬─ 广播（DHCP/ARP）──────────────► 交给 embassy-net
//!        ├─ 目标 MAC=本机，IP=本机 ────────► 交给 embassy-net（HTTP/DHCP）
//!        │   └─ UDP:5555 ──────────────────► FOU 解封装 → 发给对应客户端
//!        ├─ 目标 MAC=本机，IP=外网 ────────► FOU 封装 → 发给手机
//!        └─ 其他 ──────────────────────────► 丢弃
//! ```

use core::net::Ipv4Addr;
use core::sync::atomic::Ordering;
use core::task::Context;

use embassy_net_driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_time::Instant;
use esp_radio::wifi::{Interface, WifiTxToken};

use crate::state;
use crate::{GW_IP, INNER_MTU};

const FRAME_MAX: usize = 1600;
const BROADCAST: [u8; 6] = [0xff; 6];
const ETH_IPV4: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;

pub struct ApDriver<'d> {
    inner: Interface<'d>,
    mac: [u8; 6],
    /// 收帧落地缓冲
    buf: [u8; FRAME_MAX],
    /// 需要自己发出去的帧（转发/ICMP/ARP）
    out: [u8; FRAME_MAX],
    ip_id: u16,
    last_arp_ms: u64,
}

impl<'d> ApDriver<'d> {
    pub fn new(inner: Interface<'d>) -> Self {
        let mac = inner.mac_address();
        Self {
            inner,
            mac,
            buf: [0; FRAME_MAX],
            out: [0; FRAME_MAX],
            ip_id: 1,
            last_arp_ms: 0,
        }
    }
}

enum Plan {
    /// 交给 embassy-net（DHCP、HTTP、ARP 应答）
    ToHost,
    /// 自己发一帧，长度已写进 out
    Send(usize),
    /// 丢弃
    Drop,
}

pub struct SliceRx<'a>(&'a mut [u8]);

impl RxToken for SliceRx<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.0)
    }
}

/// 发出去的帧包一层，专门盯 DHCP 回包（dst port 68）有没有真的落到空中
pub struct ApTx {
    inner: WifiTxToken,
}

impl TxToken for ApTx {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.inner.consume(len, |d| {
            let r = f(d);
            if is_dhcp_frame(d, 68) {
                dump_dhcp("TX", d, len);
            }
            r
        })
    }
}

impl Driver for ApDriver<'_> {
    type RxToken<'a>
        = SliceRx<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = ApTx
    where
        Self: 'a;

    fn receive(&mut self, cx: &mut Context<'_>) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let Self {
            inner,
            mac,
            buf,
            out,
            ip_id,
            last_arp_ms,
        } = self;

        loop {
            let (rx, tx) = Driver::receive(inner, cx)?;

            let mut n = 0usize;
            rx.consume(|d| {
                n = d.len().min(buf.len());
                buf[..n].copy_from_slice(&d[..n]);
            });
            if n < 14 {
                continue;
            }

            // 先记账再分流：手机连不上时，rx 计数和源 MAC 是唯一能看出
            // "帧到底有没有上来"的证据
            let ethertype = u16::from_be_bytes([buf[12], buf[13]]);
            let src_mac = [buf[6], buf[7], buf[8], buf[9], buf[10], buf[11]];
            let dhcp_in = ethertype == ETH_IPV4 && is_dhcp_frame(&buf[..n], 67);
            state::note_rx(ethertype, dhcp_in, &src_mac);
            if dhcp_in {
                dump_dhcp("RX", &buf[..n], n);
            }

            match plan(&mut buf[..n], *mac, out, ip_id, last_arp_ms) {
                Plan::ToHost => return Some((SliceRx(&mut buf[..n]), ApTx { inner: tx })),
                Plan::Send(len) => {
                    tx.consume(len, |d| {
                        let l = len.min(d.len());
                        d[..l].copy_from_slice(&out[..l]);
                    });
                    continue;
                }
                Plan::Drop => continue,
            }
        }
    }

    fn transmit(&mut self, cx: &mut Context<'_>) -> Option<Self::TxToken<'_>> {
        Driver::transmit(&mut self.inner, cx).map(|t| ApTx { inner: t })
    }

    fn link_state(&mut self, cx: &mut Context<'_>) -> LinkState {
        self.inner.link_state(cx)
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}

// ---------------------------------------------------------------- 分流逻辑

/// 是不是发给 DHCP 服务器(67)的 UDP 包
fn is_dhcp_frame(f: &[u8], port: u16) -> bool {
    if f.len() < 14 + 20 + 8 {
        return false;
    }
    let ip = 14;
    if f[ip] >> 4 != 4 {
        return false;
    }
    let ihl = (f[ip] & 0x0f) as usize * 4;
    if ihl < 20 || f.len() < ip + ihl + 8 {
        return false;
    }
    if f[ip + 9] != 17 {
        return false;
    }
    let dp = u16::from_be_bytes([f[ip + ihl + 2], f[ip + ihl + 3]]);
    dp == port
}

fn fmt_mac(m: &[u8; 6]) -> alloc::string::String {
    alloc::format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

/// 把 DHCP 帧的关键字段打出来：以太网目的 MAC、IP 头、BOOTP 的 xid/flags/yiaddr。
/// 手机收到 OFFER 不认，多半是这几个字段里有一个对不上。
fn dump_dhcp(tag: &str, d: &[u8], len: usize) {
    if d.len() < 42 {
        return;
    }
    let ip = 14;
    if d[ip] >> 4 != 4 {
        return;
    }
    let ihl = (d[ip] & 0x0f) as usize * 4;
    let dh = ip + ihl + 8;
    if d.len() < dh + 20 {
        return;
    }
    let dm = &d[0..6];
    let sm = &d[6..12];
    let sp = u16::from_be_bytes([d[ip + ihl], d[ip + ihl + 1]]);
    let dp = u16::from_be_bytes([d[ip + ihl + 2], d[ip + ihl + 3]]);
    let xid = u32::from_be_bytes([d[dh + 4], d[dh + 5], d[dh + 6], d[dh + 7]]);
    let secs = u16::from_be_bytes([d[dh + 8], d[dh + 9]]);
    let flags = u16::from_be_bytes([d[dh + 10], d[dh + 11]]);
    let ciaddr = [d[dh + 12], d[dh + 13], d[dh + 14], d[dh + 15]];
    let yiaddr = [d[dh + 16], d[dh + 17], d[dh + 18], d[dh + 19]];
    let siaddr = [d[dh + 20], d[dh + 21], d[dh + 22], d[dh + 23]];
    let msg = dhcp_msg_type(d, dh);
    log::info!(
        "{} DHCP len={} dmac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} smac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} {}:{}->{}:{} iplen={} xid={:08x} secs={} flags={:04x} type={} ci={}.{}.{}.{} yi={}.{}.{}.{} si={}.{}.{}.{}",
        tag, len,
        dm[0], dm[1], dm[2], dm[3], dm[4], dm[5],
        sm[0], sm[1], sm[2], sm[3], sm[4], sm[5],
        fmt_ip(&d[ip + 12..]), sp, fmt_ip(&d[ip + 16..]), dp,
        u16::from_be_bytes([d[ip + 2], d[ip + 3]]),
        xid, secs, flags, msg,
        ciaddr[0], ciaddr[1], ciaddr[2], ciaddr[3],
        yiaddr[0], yiaddr[1], yiaddr[2], yiaddr[3],
        siaddr[0], siaddr[1], siaddr[2], siaddr[3],
    );
    // 精简一份存起来，状态行会带出来
    let short = alloc::format!(
        "{} len={} dmac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} xid={:08x} flags={:04x} type={} yi={}.{}.{}.{} {}.{}.{}.{}->{}.{}.{}.{}:{}",
        tag, len,
        dm[0], dm[1], dm[2], dm[3], dm[4], dm[5],
        xid, flags, msg,
        yiaddr[0], yiaddr[1], yiaddr[2], yiaddr[3],
        d[ip + 12], d[ip + 13], d[ip + 14], d[ip + 15],
        d[ip + 16], d[ip + 17], d[ip + 18], d[ip + 19],
        dp,
    );
    state::set_last_dhcp(tag == "RX", &short);
}

fn fmt_ip(b: &[u8]) -> alloc::string::String {
    alloc::format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

/// 从 DHCP options 里找 message type(53)
fn dhcp_msg_type(d: &[u8], dh: usize) -> u8 {
    let mut i = dh + 240;
    while i + 1 < d.len() {
        let opt = d[i];
        if opt == 255 {
            break;
        }
        if opt == 0 {
            i += 1;
            continue;
        }
        let l = d[i + 1] as usize;
        if opt == 53 && l >= 1 && i + 2 < d.len() {
            return d[i + 2];
        }
        i += 2 + l;
    }
    0
}

fn plan(f: &mut [u8], mac: [u8; 6], out: &mut [u8], ip_id: &mut u16, last_arp: &mut u64) -> Plan {
    let ethertype = u16::from_be_bytes([f[12], f[13]]);
    let dst_mac: [u8; 6] = f[0..6].try_into().unwrap();
    let src_mac: [u8; 6] = f[6..12].try_into().unwrap();

    if ethertype == ETH_ARP {
        learn_arp(&f[14..], src_mac);
        return Plan::ToHost;
    }
    if ethertype != ETH_IPV4 {
        return Plan::ToHost;
    }

    let ip = &mut f[14..];
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return Plan::Drop;
    }
    let ihl = ((ip[0] & 0x0f) as usize) * 4;
    if ihl < 20 || ip.len() < ihl {
        return Plan::Drop;
    }

    let proto = ip[9];
    let src = u32::from_be_bytes([ip[12], ip[13], ip[14], ip[15]]);
    let dst = u32::from_be_bytes([ip[16], ip[17], ip[18], ip[19]]);

    // 从客户端来的包顺手学 MAC（DHCP 阶段源地址是 0.0.0.0，别记）
    if src != 0 && (!dst_mac_is_local(dst_mac, mac)) {
        state::arp_insert(Ipv4Addr::from(src), src_mac);
    }

    // 广播：DHCP DISCOVER/REQUEST、ARP 请求
    if dst_mac == BROADCAST || dst_mac[0] & 1 == 1 {
        return Plan::ToHost;
    }
    if dst_mac != mac {
        return Plan::ToHost;
    }

    let gw = u32::from(GW_IP);

    if dst == gw {
        // 发给网关自己：先看是不是 FOU 回包
        if proto == 17 && ip.len() >= ihl + 8 {
            let dport = u16::from_be_bytes([ip[ihl + 2], ip[ihl + 3]]);
            if dport == state::fou_port() {
                return decap(ip, ihl, mac, out);
            }
        }
        return Plan::ToHost;
    }

    // 单播但 IP 不是网关 = 要转发出去
    if !state::enabled() {
        bump_drop();
        return Plan::Drop;
    }
    if is_local(dst) {
        // 同网段其他客户端：交给上层没用，直接丢（AP 下设备互访不支持）
        bump_drop();
        return Plan::Drop;
    }

    // 环路保护：FOU 出口节点自己的包不能再塞回隧道。
    // 手机连在本热点上时没有别的出口，它自己的出站包会经默认路由回到这里；
    // 不拦就是「手机解封装 → 出站 → 回到 ESP32 → 再封装」的无限乒乓，
    // 隧道上流量一直在涨，却一个包都没上过网。
    if src == u32::from(state::fou_ip()) {
        state::LOOPBACK.fetch_add(1, Ordering::Relaxed);
        return Plan::Drop;
    }

    encap(ip, ihl, proto, src, dst, mac, out, ip_id, last_arp)
}

fn dst_mac_is_local(dst_mac: [u8; 6], mac: [u8; 6]) -> bool {
    dst_mac == mac || dst_mac == BROADCAST
}

fn is_local(ip: u32) -> bool {
    ip & 0xffffff00 == u32::from_be_bytes([192, 168, 4, 0])
}

fn bump_drop() {
    state::DROPPED.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------- ARP

fn learn_arp(arp: &[u8], src_mac: [u8; 6]) {
    if arp.len() < 28 {
        return;
    }
    // htype=1 ptype=0x0800 才认
    if u16::from_be_bytes([arp[0], arp[1]]) != 1 || u16::from_be_bytes([arp[2], arp[3]]) != ETH_IPV4
    {
        return;
    }
    let sender_ip = u32::from_be_bytes([arp[14], arp[15], arp[16], arp[17]]);
    if sender_ip != 0 {
        let mut m = [0u8; 6];
        m.copy_from_slice(&arp[8..14]);
        if m != [0; 6] && m == src_mac {
            state::arp_insert(Ipv4Addr::from(sender_ip), m);
        }
    }
}

/// 构造一个"谁是 target"的 ARP 请求，返回帧长
fn arp_request(target: u32, mac: [u8; 6], gw: [u8; 4], out: &mut [u8]) -> usize {
    let n = 42;
    out[..6].copy_from_slice(&BROADCAST);
    out[6..12].copy_from_slice(&mac);
    out[12..14].copy_from_slice(&ETH_ARP.to_be_bytes());
    let a = &mut out[14..n];
    a[0..2].copy_from_slice(&1u16.to_be_bytes()); // htype
    a[2..4].copy_from_slice(&ETH_IPV4.to_be_bytes()); // ptype
    a[4] = 6; // hlen
    a[5] = 4; // plen
    a[6..8].copy_from_slice(&1u16.to_be_bytes()); // request
    a[8..14].copy_from_slice(&mac); // sha
    a[14..18].copy_from_slice(&gw); // spa
    a[18..24].copy_from_slice(&[0; 6]); // tha
    a[24..28].copy_from_slice(&target.to_be_bytes()); // tpa
    n
}

// ---------------------------------------------------------------- FOU 封装

#[allow(clippy::too_many_arguments)]
fn encap(
    ip: &mut [u8],
    ihl: usize,
    proto: u8,
    src: u32,
    dst: u32,
    mac: [u8; 6],
    out: &mut [u8],
    ip_id: &mut u16,
    last_arp: &mut u64,
) -> Plan {
    // SYN 里把 MSS 压到 1360，客户端就不会发超过 1400 的段
    if proto == 6 && clamp_mss(ip, ihl) {
        // 改过内容，重算 TCP 校验和
        fix_tcp_checksum(ip, ihl);
    }

    let total = 14 + 20 + 8 + ip.len();
    if total > out.len() {
        return frag_needed(ip, ihl, mac, out);
    }

    let peer = state::fou_ip();
    let Some(peer_mac) = state::arp_lookup(peer) else {
        // 还不知道手机 MAC，发个 ARP 请求问一下（1 秒最多一次）
        state::NO_MAC.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now().as_millis();
        if now.saturating_sub(*last_arp) >= 1000 {
            *last_arp = now;
            return Plan::Send(arp_request(
                u32::from(peer),
                mac,
                GW_IP.octets(),
                out,
            ));
        }
        return Plan::Drop;
    };

    state::set_last_flow(src, dst);
    state::FWD_OUT.fetch_add(1, Ordering::Relaxed);

    // 以太网头
    out[0..6].copy_from_slice(&peer_mac);
    out[6..12].copy_from_slice(&mac);
    out[12..14].copy_from_slice(&ETH_IPV4.to_be_bytes());

    // 外层 IPv4
    let ip_len = 20 + 8 + ip.len();
    let oip = &mut out[14..34];
    oip[0] = 0x45;
    oip[1] = 0;
    oip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
    *ip_id = ip_id.wrapping_add(1);
    oip[4..6].copy_from_slice(&ip_id.to_be_bytes());
    oip[6..8].copy_from_slice(&0u16.to_be_bytes()); // DF=0
    oip[8] = 64; // TTL
    oip[9] = 17; // UDP
    oip[10..12].copy_from_slice(&0u16.to_be_bytes());
    oip[12..16].copy_from_slice(&GW_IP.octets());
    oip[16..20].copy_from_slice(&peer.octets());
    let ck = checksum(&out[14..34]);
    out[24..26].copy_from_slice(&ck.to_be_bytes());

    // UDP 头（校验和填 0，IPv4 下合法）
    let udp_len = 8 + ip.len();
    let u = &mut out[34..42];
    u[0..2].copy_from_slice(&state::fou_port().to_be_bytes());
    u[2..4].copy_from_slice(&state::fou_port().to_be_bytes());
    u[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    u[6..8].copy_from_slice(&0u16.to_be_bytes());

    out[42..42 + ip.len()].copy_from_slice(ip);
    Plan::Send(14 + ip_len)
}

/// 内层包太大：回一个 ICMP "需要分片"(type 3 code 4)，告诉客户端把 MTU 降到 1400
fn frag_needed(ip: &[u8], ihl: usize, mac: [u8; 6], out: &mut [u8]) -> Plan {
    let Some(client_mac) = state::arp_lookup(Ipv4Addr::from(u32::from_be_bytes([
        ip[12], ip[13], ip[14], ip[15],
    ]))) else {
        bump_drop();
        return Plan::Drop;
    };

    let copy_len = (ihl + 8).min(ip.len());
    let icmp_len = 8 + copy_len;
    let total = 14 + 20 + icmp_len;

    out[0..6].copy_from_slice(&client_mac);
    out[6..12].copy_from_slice(&mac);
    out[12..14].copy_from_slice(&ETH_IPV4.to_be_bytes());

    let oip = &mut out[14..34];
    oip[0] = 0x45;
    oip[1] = 0;
    oip[2..4].copy_from_slice(&((20 + icmp_len) as u16).to_be_bytes());
    oip[4..6].copy_from_slice(&0u16.to_be_bytes());
    oip[6..8].copy_from_slice(&0u16.to_be_bytes());
    oip[8] = 64;
    oip[9] = 1; // ICMP
    oip[10..12].copy_from_slice(&0u16.to_be_bytes());
    oip[12..16].copy_from_slice(&GW_IP.octets());
    oip[16..20].copy_from_slice(&ip[12..16]);
    let ck = checksum(&out[14..34]);
    out[24..26].copy_from_slice(&ck.to_be_bytes());

    let ic = &mut out[34..34 + icmp_len];
    ic[0] = 3; // destination unreachable
    ic[1] = 4; // fragmentation needed
    ic[2..4].copy_from_slice(&0u16.to_be_bytes());
    ic[4..6].copy_from_slice(&0u16.to_be_bytes());
    ic[6..8].copy_from_slice(&INNER_MTU.to_be_bytes());
    ic[8..8 + copy_len].copy_from_slice(&ip[..copy_len]);
    let ck = checksum(ic);
    out[36..38].copy_from_slice(&ck.to_be_bytes());

    state::DROPPED.fetch_add(1, Ordering::Relaxed);
    Plan::Send(total)
}

// ---------------------------------------------------------------- FOU 解封装

fn decap(ip: &[u8], ihl: usize, mac: [u8; 6], out: &mut [u8]) -> Plan {
    let udp = &ip[ihl..];
    if udp.len() < 8 {
        return Plan::Drop;
    }
    let inner = &udp[8..];
    if inner.len() < 20 || inner[0] >> 4 != 4 {
        if inner.len() >= 20 {
            // 外层里塞的不是 IPv4，说明端口被别的东西占了
            state::LOOPBACK.fetch_add(1, Ordering::Relaxed);
        }
        return Plan::Drop;
    }

    let idst = u32::from_be_bytes([inner[16], inner[17], inner[18], inner[19]]);
    let Some(dmac) = state::arp_lookup(Ipv4Addr::from(idst)) else {
        bump_drop();
        return Plan::Drop;
    };

    let total = 14 + inner.len();
    if total > out.len() {
        bump_drop();
        return Plan::Drop;
    }

    out[0..6].copy_from_slice(&dmac);
    out[6..12].copy_from_slice(&mac);
    out[12..14].copy_from_slice(&ETH_IPV4.to_be_bytes());
    out[14..total].copy_from_slice(inner);

    state::FWD_IN.fetch_add(1, Ordering::Relaxed);
    Plan::Send(total)
}

// ---------------------------------------------------------------- TCP MSS

/// 把 SYN 里的 MSS 压到 INNER_MTU-40，返回是否改动过
fn clamp_mss(ip: &mut [u8], ihl: usize) -> bool {
    if ip[9] != 6 {
        return false;
    }
    let tcp = &mut ip[ihl..];
    if tcp.len() < 20 {
        return false;
    }
    if tcp[13] & 0x02 == 0 {
        return false; // 不是 SYN
    }
    let hl = ((tcp[12] >> 4) as usize) * 4;
    if hl < 20 || tcp.len() < hl {
        return false;
    }

    let want = INNER_MTU - 40;
    let mut i = 20usize;
    while i < hl {
        let kind = tcp[i];
        if kind == 0 {
            break; // EOL
        }
        if kind == 1 {
            i += 1; // NOP
            continue;
        }
        if i + 1 >= hl {
            break;
        }
        let len = tcp[i + 1] as usize;
        if len < 2 || i + len > hl {
            break;
        }
        if kind == 2 && len == 4 {
            let cur = u16::from_be_bytes([tcp[i + 2], tcp[i + 3]]);
            if cur > want {
                tcp[i + 2..i + 4].copy_from_slice(&want.to_be_bytes());
                return true;
            }
            return false;
        }
        i += len;
    }
    false
}

/// 重算 TCP 校验和（含伪头）。MSS 改动后必须重算。
fn fix_tcp_checksum(ip: &mut [u8], ihl: usize) {
    let total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    if total_len < ihl + 20 || total_len > ip.len() {
        return;
    }
    let tcp_len = total_len - ihl;

    // 先清零再累加
    ip[ihl + 16] = 0;
    ip[ihl + 17] = 0;

    let mut sum: u32 = 0;
    // 伪头：src(4) + dst(4) + zero/proto(2) + tcp_len(2)
    sum += u16::from_be_bytes([ip[12], ip[13]]) as u32;
    sum += u16::from_be_bytes([ip[14], ip[15]]) as u32;
    sum += u16::from_be_bytes([ip[16], ip[17]]) as u32;
    sum += u16::from_be_bytes([ip[18], ip[19]]) as u32;
    sum += 6;
    sum += tcp_len as u32;

    let mut i = ihl;
    let end = ihl + tcp_len;
    while i + 1 < end {
        sum += u16::from_be_bytes([ip[i], ip[i + 1]]) as u32;
        i += 2;
    }
    if i < end {
        sum += (ip[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let ck = !(sum as u16);
    let ck = if ck == 0 { 0xffff } else { ck };
    ip[ihl + 16..ihl + 18].copy_from_slice(&ck.to_be_bytes());
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
