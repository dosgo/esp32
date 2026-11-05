//! 全局配置与运行统计。
//!
//! 数据面每收一个包都要读 FOU 目标地址/端口，所以热路径上的东西全部用原子变量，
//! 只有 SSID/密码这类"改了要重启"的东西才放在临界区里。

use core::cell::RefCell;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use heapless::{String, Vec};

/// FOU 隧道对端（手机）的地址，默认按 DHCP 第一个租约 192.168.4.2
static FOU_IP: AtomicU32 = AtomicU32::new(u32::from_be_bytes([192, 168, 4, 2]));
static FOU_PORT: AtomicU32 = AtomicU32::new(5555);
/// 转发总开关
static ENABLED: AtomicBool = AtomicBool::new(true);
/// 是否用 HT40（40 MHz）。默认开，附近 2.4G 拥挤时关掉反而更快
static WIDE_BAND: AtomicBool = AtomicBool::new(true);

/// ARP 表项。除了 IP/MAC 还要记"最后一次听到它的时间"，
/// 这张表在双出口（AP 内手机 / 上行网段的手机）时是选路依据：
/// 手机从热点切到上游 Wi-Fi 后，旧表项如果永不过期，自动选路会一直
/// 往空气里发给一个已经不在的邻居，看起来就像"隧道通了但上不了网"。
#[derive(Clone, Copy)]
pub struct ArpEntry {
    pub ip: u32,
    pub mac: [u8; 6],
    pub seen: u32,
}

/// 表项多久没听到就当掉线。客户端持续有流量时会被不断刷新
const ARP_TTL_SECS: u32 = 120;

/// IP -> MAC，最多记 8 个客户端，够用了（8 条线性查比哈希还快）
pub static ARP: Mutex<RefCell<Vec<ArpEntry, 8>>> = Mutex::new(RefCell::new(Vec::new()));

fn now_secs() -> u32 {
    embassy_time::Instant::now().as_secs() as u32
}

/// 统计
pub static FWD_OUT: AtomicU32 = AtomicU32::new(0);
pub static FWD_IN: AtomicU32 = AtomicU32::new(0);
/// 隧道内层字节数（用于算速率）。u32 会回绕，但速率只看 5 秒增量，回绕也只错一次
pub static TUN_BYTES_OUT: AtomicU32 = AtomicU32::new(0);
pub static TUN_BYTES_IN: AtomicU32 = AtomicU32::new(0);
pub static DROPPED: AtomicU32 = AtomicU32::new(0);
pub static LOOPBACK: AtomicU32 = AtomicU32::new(0);
pub static NO_MAC: AtomicU32 = AtomicU32::new(0);
/// 最近一条隧道内层包的源/目的，串口/网页都能看到当前在转发什么
static FLOW_SRC: AtomicU32 = AtomicU32::new(0);
static FLOW_DST: AtomicU32 = AtomicU32::new(0);

/// 空中收帧统计。rx_frames 为 0 说明手机压根没把帧发上来（没关联上/密码错），
/// 不是转发逻辑的问题 —— 这是排查"连不上"最快的一眼。
pub static RX_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static RX_ARP: AtomicU32 = AtomicU32::new(0);
pub static RX_IP: AtomicU32 = AtomicU32::new(0);
pub static RX_DHCP: AtomicU32 = AtomicU32::new(0);
pub static RX_OTHER: AtomicU32 = AtomicU32::new(0);
/// 最近一个收帧的源 MAC，用来确认到底是谁连上来了（全 0 表示还没收到任何帧）
pub static RX_LAST_MAC: Mutex<RefCell<[u8; 6]>> = Mutex::new(RefCell::new([0; 6]));

/// AP 自己的 MAC。上行模式下要给客户端组帧，源 MAC 必须是 AP 的
static AP_MAC: Mutex<RefCell<[u8; 6]>> = Mutex::new(RefCell::new([0; 6]));

pub fn set_ap_mac(m: [u8; 6]) {
    critical_section::with(|cs| *AP_MAC.borrow_ref_mut(cs) = m);
}

pub fn ap_mac() -> [u8; 6] {
    critical_section::with(|cs| *AP_MAC.borrow_ref(cs))
}

pub fn note_rx(ethertype: u16, is_dhcp: bool, src_mac: &[u8; 6]) {
    RX_FRAMES.fetch_add(1, Ordering::Relaxed);
    match ethertype {
        0x0806 => {
            RX_ARP.fetch_add(1, Ordering::Relaxed);
        }
        0x0800 => {
            RX_IP.fetch_add(1, Ordering::Relaxed);
            if is_dhcp {
                RX_DHCP.fetch_add(1, Ordering::Relaxed);
            }
        }
        _ => {
            RX_OTHER.fetch_add(1, Ordering::Relaxed);
        }
    }
    critical_section::with(|cs| {
        *RX_LAST_MAC.borrow_ref_mut(cs) = *src_mac;
    });
    // 收到它的帧就说明它还在，ARP 表项续期
    arp_touch(src_mac);
}

/// 最近一次 DHCP 收/发摘要。串口晚打开会错过瞬时日志，这里留一份给状态行。
pub static LAST_DHCP_RX: Mutex<RefCell<heapless::String<120>>> =
    Mutex::new(RefCell::new(heapless::String::new()));
pub static LAST_DHCP_TX: Mutex<RefCell<heapless::String<120>>> =
    Mutex::new(RefCell::new(heapless::String::new()));

pub fn set_last_dhcp(is_rx: bool, s: &str) {
    critical_section::with(|cs| {
        let cell = if is_rx { &LAST_DHCP_RX } else { &LAST_DHCP_TX };
        let mut slot = cell.borrow_ref_mut(cs);
        slot.clear();
        let _ = slot.push_str(s);
    });
}

pub fn last_dhcp(is_rx: bool) -> heapless::String<120> {
    critical_section::with(|cs| {
        let cell = if is_rx { &LAST_DHCP_RX } else { &LAST_DHCP_TX };
        let mut out = heapless::String::new();
        let _ = out.push_str(cell.borrow_ref(cs).as_str());
        out
    })
}

pub fn rx_last_mac() -> [u8; 6] {
    critical_section::with(|cs| *RX_LAST_MAC.borrow_ref(cs))
}

pub struct Net {
    pub ssid: String<32>,
    pub pass: String<64>,
}

static NET: Mutex<RefCell<Net>> = Mutex::new(RefCell::new(Net {
    ssid: String::new(),
    pass: String::new(),
}));

pub fn init() {
    critical_section::with(|cs| {
        let mut n = NET.borrow_ref_mut(cs);
        n.ssid.clear();
        let _ = n.ssid.push_str("PocketGW-ESP32");
        n.pass.clear();
        let _ = n.pass.push_str("gateway888");
    });
}

// ---------------- 配置读写 ----------------

/// 取一份配置拷贝（启动 Wi-Fi 时用；锁里不能返回引用）
pub fn net_copy() -> (String<32>, String<64>) {
    let mut s = String::new();
    let mut p = String::new();
    critical_section::with(|cs| {
        let n = NET.borrow_ref(cs);
        let _ = s.push_str(n.ssid.as_str());
        let _ = p.push_str(n.pass.as_str());
    });
    (s, p)
}

pub fn set_net(ssid: &str, pass: &str) -> bool {
    if ssid.is_empty() || ssid.len() > 31 || pass.len() < 8 || pass.len() > 63 {
        return false;
    }
    critical_section::with(|cs| {
        let mut n = NET.borrow_ref_mut(cs);
        n.ssid.clear();
        n.pass.clear();
        n.ssid.push_str(ssid).is_ok() && n.pass.push_str(pass).is_ok()
    })
}

// ---------------- 上行 Wi-Fi（STA）----------------
//
// 双模（AP+STA）：ESP32 一边给客户端当 AP，一边连到上游路由器。
// 此时 FOU 节点不在 AP 网段里，而是在上游 LAN 里 —— 封装后的 UDP 得从
// station 网卡发出去，回包也由 station 网卡收回来再注入 AP。

/// 上行开关 + 配置
static UP_ON: AtomicBool = AtomicBool::new(false);
/// 上行拿到的 IP（0 = 没连上）
static UP_IP: AtomicU32 = AtomicU32::new(0);
/// 0=未启用 1=连接中 2=已连接 3=连接失败
static UP_STATE: AtomicU8 = AtomicU8::new(0);
/// 上行 AP 实际信道（串口/网页展示用）
static UP_CH: AtomicU8 = AtomicU8::new(0);

pub struct Uplink {
    pub ssid: String<32>,
    pub pass: String<64>,
    pub on: bool,
}

static UPLINK: Mutex<RefCell<Uplink>> = Mutex::new(RefCell::new(Uplink {
    ssid: String::new(),
    pass: String::new(),
    on: false,
}));

/// 网页改了上行配置后通知 uplink 任务重连
pub static UP_DIRTY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// AP → station 方向：封装好的内层 IP 包（交给 uplink 任务发 UDP）
pub static UP_Q: Channel<CriticalSectionRawMutex, Vec<u8, 1500>, 16> = Channel::new();
/// station → AP 方向：解封装后的内层 IP 包（由 apdrv 组装成以太网帧发给客户端）
pub static AP_RX_Q: Channel<CriticalSectionRawMutex, Vec<u8, 1500>, 16> = Channel::new();

pub fn uplink_copy() -> (String<32>, String<64>, bool) {
    let mut s = String::new();
    let mut p = String::new();
    let on = critical_section::with(|cs| {
        let u = UPLINK.borrow_ref(cs);
        let _ = s.push_str(u.ssid.as_str());
        let _ = p.push_str(u.pass.as_str());
        u.on
    });
    (s, p, on)
}

pub fn set_uplink(ssid: &str, pass: &str, on: bool) -> bool {
    if ssid.len() > 31 || pass.len() > 63 {
        return false;
    }
    if on && ssid.is_empty() {
        return false;
    }
    // 有密码就必须是合法长度（开放网络允许空密码）
    if !pass.is_empty() && pass.len() < 8 {
        return false;
    }
    let ok = critical_section::with(|cs| {
        let mut u = UPLINK.borrow_ref_mut(cs);
        u.ssid.clear();
        u.pass.clear();
        u.on = on;
        u.ssid.push_str(ssid).is_ok() && u.pass.push_str(pass).is_ok()
    });
    if ok {
        UP_DIRTY.signal(());
    }
    ok
}

pub fn uplink_on() -> bool {
    UP_ON.load(Ordering::Relaxed)
}

pub fn set_uplink_on(v: bool) {
    UP_ON.store(v, Ordering::Relaxed);
}

/// 上行真的能用来发隧道包了吗（既要连上，也要没被别人改配置）
pub fn uplink_up() -> bool {
    UP_ON.load(Ordering::Relaxed) && UP_IP.load(Ordering::Relaxed) != 0
}

pub fn uplink_ip() -> Ipv4Addr {
    Ipv4Addr::from(UP_IP.load(Ordering::Relaxed))
}

pub fn set_uplink_ip(ip: Ipv4Addr) {
    // 只动地址。UP_ON 是"配置上启用了上行"，由 uplink 任务显式管理 ——
    // 两者分开，隧道才不会在断线瞬间误把流量发回 AP 里的客户端
    UP_IP.store(u32::from(ip), Ordering::Relaxed);
}

pub fn uplink_state() -> u8 {
    UP_STATE.load(Ordering::Relaxed)
}

pub fn set_uplink_state(v: u8) {
    UP_STATE.store(v, Ordering::Relaxed);
}

pub fn uplink_channel() -> u8 {
    UP_CH.load(Ordering::Relaxed)
}

pub fn set_uplink_channel(c: u8) {
    UP_CH.store(c, Ordering::Relaxed);
}

pub fn fou_ip() -> Ipv4Addr {
    Ipv4Addr::from(FOU_IP.load(Ordering::Relaxed))
}

pub fn fou_ip_raw() -> u32 {
    FOU_IP.load(Ordering::Relaxed)
}

pub fn set_fou_ip(ip: Ipv4Addr) {
    FOU_IP.store(u32::from(ip), Ordering::Relaxed);
}

pub fn fou_port() -> u16 {
    FOU_PORT.load(Ordering::Relaxed) as u16
}

pub fn set_fou_port(p: u16) {
    FOU_PORT.store(p as u32, Ordering::Relaxed);
}

// ---------------- 两种出口同时支持 ----------------
//
// 出口一：AP 模式 —— 手机就连在本热点里（默认 192.168.4.2），
//         封装成以太网帧直接发给它的 MAC，路径最短。
// 出口二：上行模式 —— 手机在上游路由器的 LAN 里，封装成 UDP 从 station 网卡发。
//
// 两个目标各存一份，自动模式下"谁通就用谁"：ARP 里有 AP 目标就用 AP 路径，
// 否则用上行。手机从热点挪到上游（或反过来）时不需要改配置。

/// 上行模式的目标（手机在上游 LAN 的 IP）。0 = 没填，退回 AP 目标
static UP_TARGET: AtomicU32 = AtomicU32::new(0);
/// 选路策略：0=自动 1=只用 AP 内手机 2=只用上行
static ROUTE_MODE: AtomicU8 = AtomicU8::new(0);

pub fn up_target() -> Ipv4Addr {
    Ipv4Addr::from(UP_TARGET.load(Ordering::Relaxed))
}

pub fn up_target_raw() -> u32 {
    UP_TARGET.load(Ordering::Relaxed)
}

pub fn set_up_target(ip: Ipv4Addr) {
    UP_TARGET.store(u32::from(ip), Ordering::Relaxed);
}

/// 隧道最终目标：填了上行目标就用它，否则沿用 AP 目标。
/// 这样"只填一个手机 IP"的老用法（把 AP 的 IP 改成上游地址）也还能工作。
pub fn tunnel_target() -> Ipv4Addr {
    let t = up_target();
    if t.is_unspecified() {
        fou_ip()
    } else {
        t
    }
}

pub fn route_mode() -> u8 {
    ROUTE_MODE.load(Ordering::Relaxed)
}

pub fn set_route_mode(v: u8) {
    ROUTE_MODE.store(v.min(2), Ordering::Relaxed);
}

fn in_ap_subnet(ip: Ipv4Addr) -> bool {
    u32::from(ip) & 0xffffff00 == u32::from_be_bytes([192, 168, 4, 0])
}

/// 当前实际在用的出口：0=没有 1=AP 内的手机 2=上行 Wi-Fi
pub fn active_path() -> u8 {
    if !enabled() {
        return 0;
    }
    let ap = fou_ip();
    let ap_ok = in_ap_subnet(ap) && arp_lookup(ap).is_some();
    let up_ok = uplink_up() && !tunnel_target().is_unspecified() && !uplink_ip().is_unspecified();
    match route_mode() {
        1 => {
            if ap_ok {
                1
            } else {
                0
            }
        }
        2 => {
            if up_ok {
                2
            } else {
                0
            }
        }
        _ => {
            if ap_ok {
                1
            } else if up_ok {
                2
            } else {
                0
            }
        }
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub fn wide_band() -> bool {
    WIDE_BAND.load(Ordering::Relaxed)
}

pub fn set_wide_band(v: bool) {
    WIDE_BAND.store(v, Ordering::Relaxed);
}

pub fn set_last_flow(src: u32, dst: u32) {
    FLOW_SRC.store(src, Ordering::Relaxed);
    FLOW_DST.store(dst, Ordering::Relaxed);
}

pub fn last_flow() -> Option<(Ipv4Addr, Ipv4Addr)> {
    let src = FLOW_SRC.load(Ordering::Relaxed);
    let dst = FLOW_DST.load(Ordering::Relaxed);
    if src == 0 && dst == 0 {
        None
    } else {
        Some((Ipv4Addr::from(src), Ipv4Addr::from(dst)))
    }
}

// ---------------- ARP 表 ----------------

pub fn arp_insert(ip: Ipv4Addr, mac: [u8; 6]) {
    let key = u32::from(ip);
    let now = now_secs();
    critical_section::with(|cs| {
        let mut t = ARP.borrow_ref_mut(cs);
        for e in t.iter_mut() {
            if e.ip == key {
                e.mac = mac;
                e.seen = now;
                return;
            }
        }
        if t.push(ArpEntry { ip: key, mac, seen: now }).is_err() {
            // 满了丢最早那条
            t.remove(0);
            let _ = t.push(ArpEntry { ip: key, mac, seen: now });
        }
    });
}

/// 续期：收到某块 MAC 的帧时调用
pub fn arp_touch(mac: &[u8; 6]) {
    if *mac == [0; 6] {
        return;
    }
    let now = now_secs();
    critical_section::with(|cs| {
        for e in ARP.borrow_ref_mut(cs).iter_mut() {
            if e.mac == *mac {
                e.seen = now;
            }
        }
    });
}

pub fn arp_lookup(ip: Ipv4Addr) -> Option<[u8; 6]> {
    let key = u32::from(ip);
    let now = now_secs();
    critical_section::with(|cs| {
        ARP.borrow_ref(cs)
            .iter()
            .find(|e| e.ip == key && now.wrapping_sub(e.seen) < ARP_TTL_SECS)
            .map(|e| e.mac)
    })
}

pub fn arp_macs() -> Vec<(Ipv4Addr, [u8; 6]), 8> {
    let mut out = Vec::new();
    let now = now_secs();
    critical_section::with(|cs| {
        for e in ARP.borrow_ref(cs).iter() {
            if now.wrapping_sub(e.seen) < ARP_TTL_SECS {
                let _ = out.push((Ipv4Addr::from(e.ip), e.mac));
            }
        }
    });
    out
}

// ---------------- 状态快照 ----------------

pub struct Snapshot {
    pub up: bool,
    pub client_ip: Ipv4Addr,
    pub client_mac: Option<[u8; 6]>,
    pub fwd_out: u32,
    pub fwd_in: u32,
    pub dropped: u32,
    pub no_mac: u32,
    pub loopback: u32,
    pub flow: Option<(Ipv4Addr, Ipv4Addr)>,
    pub rx_frames: u32,
    pub rx_arp: u32,
    pub rx_ip: u32,
    pub rx_dhcp: u32,
    pub rx_other: u32,
    pub rx_mac: [u8; 6],
    pub up_on: bool,
    pub up_ip: Ipv4Addr,
    pub up_state: u8,
    pub up_ch: u8,
    /// 上行模式的隧道目标
    pub up_target: Ipv4Addr,
    /// 0 自动 / 1 只用 AP / 2 只用上行
    pub route_mode: u8,
    /// 当前实际出口：0 无 / 1 AP / 2 上行
    pub path: u8,
}

pub fn snapshot() -> Snapshot {
    let ip = fou_ip();
    Snapshot {
        up: enabled(),
        client_ip: ip,
        client_mac: arp_lookup(ip),
        fwd_out: FWD_OUT.load(Ordering::Relaxed),
        fwd_in: FWD_IN.load(Ordering::Relaxed),
        dropped: DROPPED.load(Ordering::Relaxed),
        no_mac: NO_MAC.load(Ordering::Relaxed),
        loopback: LOOPBACK.load(Ordering::Relaxed),
        flow: last_flow(),
        rx_frames: RX_FRAMES.load(Ordering::Relaxed),
        rx_arp: RX_ARP.load(Ordering::Relaxed),
        rx_ip: RX_IP.load(Ordering::Relaxed),
        rx_dhcp: RX_DHCP.load(Ordering::Relaxed),
        rx_other: RX_OTHER.load(Ordering::Relaxed),
        rx_mac: rx_last_mac(),
        up_on: uplink_up(),
        up_ip: uplink_ip(),
        up_state: uplink_state(),
        up_ch: uplink_channel(),
        up_target: up_target(),
        route_mode: route_mode(),
        path: active_path(),
    }
}
