//! 全局配置与运行统计。
//!
//! 数据面每收一个包都要读 FOU 目标地址/端口，所以热路径上的东西全部用原子变量，
//! 只有 SSID/密码这类"改了要重启"的东西才放在临界区里。

use core::cell::RefCell;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use critical_section::Mutex;
use heapless::{String, Vec};

/// FOU 隧道对端（手机）的地址，默认按 DHCP 第一个租约 192.168.4.2
static FOU_IP: AtomicU32 = AtomicU32::new(u32::from_be_bytes([192, 168, 4, 2]));
static FOU_PORT: AtomicU32 = AtomicU32::new(5555);
/// 转发总开关
static ENABLED: AtomicBool = AtomicBool::new(true);
/// 是否用 HT40（40 MHz）。默认开，附近 2.4G 拥挤时关掉反而更快
static WIDE_BAND: AtomicBool = AtomicBool::new(true);

/// IP -> MAC，最多记 8 个客户端，够用了（8 条线性查比哈希还快）
pub static ARP: Mutex<RefCell<Vec<(u32, [u8; 6]), 8>>> = Mutex::new(RefCell::new(Vec::new()));

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
    critical_section::with(|cs| {
        let mut t = ARP.borrow_ref_mut(cs);
        for e in t.iter_mut() {
            if e.0 == key {
                if e.1 != mac {
                    e.1 = mac;
                }
                return;
            }
        }
        if t.push((key, mac)).is_err() {
            // 满了丢最早那条
            t.remove(0);
            let _ = t.push((key, mac));
        }
    });
}

pub fn arp_lookup(ip: Ipv4Addr) -> Option<[u8; 6]> {
    let key = u32::from(ip);
    critical_section::with(|cs| {
        ARP.borrow_ref(cs)
            .iter()
            .find(|e| e.0 == key)
            .map(|e| e.1)
    })
}

pub fn arp_macs() -> Vec<(Ipv4Addr, [u8; 6]), 8> {
    let mut out = Vec::new();
    critical_section::with(|cs| {
        for (ip, mac) in ARP.borrow_ref(cs).iter() {
            let _ = out.push((Ipv4Addr::from(*ip), *mac));
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
    }
}
