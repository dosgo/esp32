//! 热点模式下配套的迷你 DNS：把**所有** A 查询都解析成 ESP32 自己（192.168.4.1）。
//!
//! 为什么需要：手机连上一个没有外网的热点后会做"俘虏门户"探测——
//! iOS 访问 captive.apple.com、安卓访问 connectivitycheck.gstatic.com。
//! 没人应答 DNS 的话，手机可能直接判定"此网络无法上网"然后断开。
//! 把域名指回自己之后，HTTP 服务会（对任何路径都）返回控制页面，
//! 手机就会弹"登录到此网络"，点进去就是键盘页面。
//!
//! 只实现必要的部分：标准查询 + A 记录 + IN 类，其它一律忽略。

use core::sync::atomic::{AtomicU8, Ordering};

use embassy_net::{
    IpAddress, Ipv4Address, Stack,
    udp::{PacketMetadata, UdpSocket},
};
use heapless::Vec;
use log::warn;

const DNS_PORT: u16 = 53;

/// 见过多少个不同的客户端 IP —— 屏幕右上角显示"连进热点的设备数"。
/// 手机一连上热点就会发 DNS 探测，所以用这个近似统计，够用了。
static DEVICES: AtomicU8 = AtomicU8::new(0);

pub fn device_count() -> u8 {
    DEVICES.load(Ordering::Relaxed)
}

pub async fn run(stack: Stack<'static>, our_ip: Ipv4Address) {
    let mut seen: Vec<[u8; 4], 8> = Vec::new();
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 512];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_buf = [0u8; 512];
    let mut sock = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buf,
        &mut tx_meta,
        &mut tx_buf,
    );

    if let Err(e) = sock.bind(DNS_PORT) {
        warn!("[dns] 绑定 53 端口失败: {e:?}");
        return;
    }

    let mut buf = [0u8; 512];
    let mut out = [0u8; 512];

    loop {
        let (n, ep) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if n < 12 {
            continue;
        }

        // 记一下是谁在问，用来数连进热点的设备
        // （只开了 proto-ipv4，所以 IpAddress 只有 Ipv4 一个变体，这里是穷尽的）
        let IpAddress::Ipv4(v) = ep.endpoint.addr;
        let ip = v.octets();
        if !seen.contains(&ip) && seen.push(ip).is_ok() {
            DEVICES.store(seen.len() as u8, Ordering::Relaxed);
        }

        // 不是查询（QR=1）就不理
        if buf[2] & 0x80 != 0 {
            continue;
        }
        let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
        if qdcount == 0 {
            continue;
        }

        // 跳过 QNAME（一串 length 前缀的 label，以 0 结束）
        let mut i = 12usize;
        let mut ok = false;
        while i < n {
            let l = buf[i] as usize;
            if l == 0 {
                i += 1;
                ok = true;
                break;
            }
            i += l + 1;
        }
        if !ok || i + 4 > n {
            continue;
        }
        let qtype = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let qclass = u16::from_be_bytes([buf[i + 2], buf[i + 3]]);
        let qend = i + 4;
        // 只答 A 记录 / IN 类
        if qtype != 1 || qclass != 1 {
            continue;
        }

        let total = qend + 16;
        if total > out.len() {
            continue;
        }

        out[..12].copy_from_slice(&[
            buf[0],
            buf[1], // 事务 ID 原样带回
            0x81,
            0x80, // QR=1, RD 保留, RA=1
            buf[4],
            buf[5], // QDCOUNT
            0,
            1, // ANCOUNT = 1
            0,
            0, // NSCOUNT
            0,
            0, // ARCOUNT
        ]);
        out[12..qend].copy_from_slice(&buf[12..qend]);

        // Answer：名字用指针指回 question 里的 QNAME（0xC00C）
        let a = &mut out[qend..qend + 16];
        a[0] = 0xC0;
        a[1] = 0x0C;
        a[2] = 0x00;
        a[3] = 0x01; // TYPE  = A
        a[4] = 0x00;
        a[5] = 0x01; // CLASS = IN
        a[6] = 0x00;
        a[7] = 0x00;
        a[8] = 0x00;
        a[9] = 0x3C; // TTL = 60s（短一点，免得手机缓存住）
        a[10] = 0x00;
        a[11] = 0x04; // RDLENGTH = 4
        a[12..16].copy_from_slice(&our_ip.octets());

        if let Err(e) = sock.send_to(&out[..total], ep).await {
            warn!("[dns] 回包失败: {e:?}");
        }
    }
}
