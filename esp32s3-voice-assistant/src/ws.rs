//! 手写 WebSocket 客户端（no_std）
//!
//! 为什么不用现成的 crate：
//! - `tokio-tungstenite` 需要 std + tokio，ESP32 上不可行
//! - `embedded-websocket` 需要自己实现底层 IO trait，且 API 偏重
//! - 协议本身很简单（握手 + 6 字节头），手写 200 行反而是最省心的
//!
//! # 取消安全（这是整个设计的关键）
//!
//! 主循环用「先发后收」的轮询：先清空发送队列，再读最多 20ms 的 socket。
//! 读操作会被 `select`/`with_timeout` 打断，如果直接读到临时数组，
//! 被打断时已读到的半帧就丢了，之后整条流会错位。
//!
//! 所以读操作**直接读进累积缓冲 `acc`**：
//! `read(&mut acc[acc_len..])` 返回 Ready(n) 之后才 `acc_len += n`，
//! 两步之间没有 await 点，因此取消时要么没读到（数据还在 socket 里），
//! 要么完整读到了（数据已在 acc 里）。两种情况都不会丢字节。

#![allow(dead_code)]

use embedded_io_async::Write as _;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Ipv4Address, Stack};
use embassy_time::{Duration, WithTimeout};

/// `fill` 的两种"失败"：超时是正常节拍，断开要重连
#[derive(Debug, PartialEq, Eq)]
pub enum ReadErr {
    Timeout,
    Closed,
}

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

const ACC_SIZE: usize = 8192;

pub struct WsConn {
    sock: TcpSocket<'static>,
    acc: [u8; ACC_SIZE],
    acc_len: usize,
}

impl WsConn {
    /// 建立 TCP 连接 + HTTP Upgrade 握手
    pub async fn connect(
        stack: Stack<'static>,
        ip: IpAddress,
        port: u16,
        path: &str,
        device_id: &str,
        host_header: &str,
    ) -> Result<Self, &'static str> {
        let rx = mk_static_buf();
        let tx = mk_static_buf();
        let mut sock = TcpSocket::new(stack, rx, tx);
        // 【2026-10-28 改】原来是 10 秒，太短了。
        //
        // smoltcp 的 timeout 是按「最后一次**发送**数据」算的：设备在待机
        // （Idle）时一个字节都不往外发，于是连上 10 秒整就被底层 abort，
        // 表现是设备日志「[ws] 断开: 连接已关闭」+ 网关侧 code=1006，
        // 用户完全感觉不到，只是每次开口前都要先等 3 秒重连。
        //
        // 实测证据：拿一个什么都不做的极简 WS 服务器接设备，精确 10.0 秒断。
        // 现在放宽到 120 秒，并配合 main.rs 里 8 秒一次的应用层心跳，
        // 正常情况根本走不到这个超时；它只作为「网关真的死了」的兜底。
        sock.set_timeout(Some(Duration::from_secs(120)));
        sock.set_keep_alive(Some(Duration::from_secs(20)));

        sock.connect((ip, port))
            .await
            .map_err(|_| "TCP 连接失败")?;

        // ---- 握手 ----
        let key_bytes = rand_key();
        let mut key_b64 = [0u8; 24];
        base64_encode(&key_bytes, &mut key_b64);

        let mut req = [0u8; 320];
        let mut n = 0usize;
        n += put(&mut req[n..], b"GET ");
        n += put(&mut req[n..], path.as_bytes());
        n += put(&mut req[n..], b" HTTP/1.1\r\nHost: ");
        n += put(&mut req[n..], host_header.as_bytes());
        n += put(&mut req[n..], b"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ");
        n += put(&mut req[n..], &key_b64);
        n += put(
            &mut req[n..],
            b"\r\nSec-WebSocket-Version: 13\r\nDevice-Id: ",
        );
        n += put(&mut req[n..], device_id.as_bytes());
        n += put(&mut req[n..], b"\r\n\r\n");

        sock.write_all(&req[..n])
            .await
            .map_err(|_| "握手请求发送失败")?;

        // ---- 读响应头 ----
        let mut resp = [0u8; 512];
        let mut rlen = 0usize;
        loop {
            if rlen >= resp.len() {
                return Err("响应头过长");
            }
            let got = sock
                .read(&mut resp[rlen..])
                .with_timeout(Duration::from_secs(5))
                .await
                .map_err(|_| "握手超时")?
                .map_err(|_| "握手读取失败")?;
            if got == 0 {
                return Err("服务端关闭连接");
            }
            let old = rlen;
            rlen += got;
            // 找 \r\n\r\n
            if let Some(p) = find_seq(&resp[old.saturating_sub(3)..rlen], b"\r\n\r\n") {
                // 响应体（如果有）留在 socket 里由后续帧解析处理：
                // 这里把多读到的字节塞回 acc
                let body_start = old.saturating_sub(3) + p + 4;
                let extra = &resp[body_start..rlen];
                let mut conn = Self {
                    sock,
                    acc: [0u8; ACC_SIZE],
                    acc_len: 0,
                };
                let take = extra.len().min(ACC_SIZE);
                conn.acc[..take].copy_from_slice(&extra[..take]);
                conn.acc_len = take;

                // 状态码必须是 101
                if !resp[..body_start.min(rlen)].starts_with(b"HTTP/1.1 101") {
                    let code = resp
                        .get(9..12)
                        .and_then(|b| core::str::from_utf8(b).ok())
                        .unwrap_or("???");
                    if code == "101" {
                        return Ok(conn);
                    }
                    log::error!("[ws] 握手被拒: {code}");
                    return Err("服务端拒绝升级");
                }
                return Ok(conn);
            }
        }
    }

    /// 发一帧。客户端 → 服务端必须带 mask。
    pub async fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), ()> {
        let len = payload.len();
        let mut head = [0u8; 14];
        head[0] = 0x80 | opcode; // FIN=1
        let hlen;
        if len < 126 {
            head[1] = 0x80 | (len as u8);
            hlen = 2;
        } else if len < 65_536 {
            head[1] = 0x80 | 126;
            head[2..4].copy_from_slice(&(len as u16).to_be_bytes());
            hlen = 4;
        } else {
            head[1] = 0x80 | 127;
            head[2..10].copy_from_slice(&(len as u64).to_be_bytes());
            hlen = 10;
        }
        let mask = next_mask();
        head[hlen..hlen + 4].copy_from_slice(&mask);
        let total = hlen + 4;

        self.sock.write_all(&head[..total]).await.map_err(|_| ())?;

        // payload 分块加掩码后发出，避免为 1920 字节再开一块静态缓冲
        let mut chunk = [0u8; 256];
        let mut off = 0usize;
        while off < len {
            let n = (len - off).min(256);
            for i in 0..n {
                chunk[i] = payload[off + i] ^ mask[(off + i) % 4];
            }
            self.sock
                .write_all(&chunk[..n])
                .await
                .map_err(|_| ())?;
            off += n;
        }
        Ok(())
    }

    pub async fn send_text(&mut self, s: &str) -> Result<(), ()> {
        self.send(OP_TEXT, s.as_bytes()).await
    }

    pub async fn send_binary(&mut self, p: &[u8]) -> Result<(), ()> {
        self.send(OP_BINARY, p).await
    }

    /// 从 socket 再读一些数据进累积缓冲。被取消是安全的（见模块注释）。
    ///
    /// 超时和对端关闭必须分开：超时是主循环的正常节拍（没有下行数据时
    /// 每 20ms 就会超时一次），关闭则要立刻退出重连。
    pub async fn fill(&mut self, timeout: Duration) -> Result<(), ReadErr> {
        if self.acc_len >= ACC_SIZE {
            return Err(ReadErr::Closed); // 缓冲里堆了个超大帧，协议有问题
        }
        let n = match self
            .sock
            .read(&mut self.acc[self.acc_len..])
            .with_timeout(timeout)
            .await
        {
            Err(_) => return Err(ReadErr::Timeout),
            Ok(Err(_)) => return Err(ReadErr::Closed),
            Ok(Ok(n)) => n,
        };
        if n == 0 {
            return Err(ReadErr::Closed);
        }
        self.acc_len += n;
        Ok(())
    }

    /// 尝试从累积缓冲里切出一个完整帧。
    ///
    /// 返回 `(opcode, 数据在 acc 中的起始下标, 长度, 整帧总长)`。
    /// 调用方用完数据后必须调 `consume(total)` 把帧从缓冲里移走。
    pub fn peek_frame(&self) -> Option<(u8, usize, usize, usize)> {
        let a = &self.acc[..self.acc_len];
        if a.len() < 2 {
            return None;
        }
        let opcode = a[0] & 0x0F;
        let masked = a[1] & 0x80 != 0;
        let mut len = (a[1] & 0x7F) as usize;
        let mut p = 2usize;

        if len == 126 {
            if a.len() < 4 {
                return None;
            }
            len = u16::from_be_bytes([a[2], a[3]]) as usize;
            p = 4;
        } else if len == 127 {
            if a.len() < 10 {
                return None;
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&a[2..10]);
            len = u64::from_be_bytes(b) as usize;
            p = 10;
        }

        if masked {
            p += 4;
        }
        let total = p + len;
        if a.len() < total {
            return None;
        }
        Some((opcode, p, len, total))
    }

    /// 把一帧的原始数据（未解 mask）复制出来并解掩码
    pub fn read_frame_data(&self, start: usize, len: usize, out: &mut [u8]) -> usize {
        let n = len.min(out.len());
        out[..n].copy_from_slice(&self.acc[start..start + n]);
        // 服务端帧按 RFC 不带 mask，但保险起见按位处理
        if n >= 2 {
            let masked = self.acc[1] & 0x80 != 0;
            if masked {
                let mask_off = start - 4;
                let m = [
                    self.acc[mask_off],
                    self.acc[mask_off + 1],
                    self.acc[mask_off + 2],
                    self.acc[mask_off + 3],
                ];
                for i in 0..n {
                    out[i] ^= m[i % 4];
                }
            }
        }
        n
    }

    pub fn consume(&mut self, total: usize) {
        if total >= self.acc_len {
            self.acc_len = 0;
            return;
        }
        self.acc.copy_within(total..self.acc_len, 0);
        self.acc_len -= total;
    }

}

// ---------------- 辅助 ----------------

/// 拿一块 4KB 的 TCP 缓冲。
///
/// 不能用 `StaticCell::init` —— 它只能写一次，第二次（重连时）会 panic。
/// 这里用 4 块轮转：同一时刻只会有一个活动连接，轮转一圈回来时
/// 上一轮的 WsConn 早就 drop 了，不会有两个引用指向同一块。
fn mk_static_buf() -> &'static mut [u8] {
    use core::sync::atomic::{AtomicUsize, Ordering};
    const POOL: usize = 4;
    const SZ: usize = 4096;

    static mut POOL_BUF: [[u8; SZ]; POOL] = [[0u8; SZ]; POOL];
    static IDX: AtomicUsize = AtomicUsize::new(0);

    let i = IDX.fetch_add(1, Ordering::Relaxed) % POOL;
    // SAFETY: 索引轮转保证同一块缓冲在 4 次调用内不会被重复交出；
    // 而每次重连的间隔是 3 秒，旧连接一定已经释放。
    unsafe {
        let base = core::ptr::addr_of_mut!(POOL_BUF) as *mut u8;
        core::slice::from_raw_parts_mut(base.add(i * SZ), SZ)
    }
}

fn put(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(src: &[u8], out: &mut [u8]) -> usize {
    let mut i = 0usize;
    let mut o = 0usize;
    while i < src.len() && o + 4 <= out.len() {
        let b0 = src[i];
        let b1 = if i + 1 < src.len() { src[i + 1] } else { 0 };
        let b2 = if i + 2 < src.len() { src[i + 2] } else { 0 };
        out[o] = B64[(b0 >> 2) as usize];
        out[o + 1] = B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize];
        out[o + 2] = if i + 1 < src.len() {
            B64[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize]
        } else {
            b'='
        };
        out[o + 3] = if i + 2 < src.len() {
            B64[(b2 & 0x3F) as usize]
        } else {
            b'='
        };
        i += 3;
        o += 4;
    }
    o
}

/// 16 字节握手 key。不需要密码学强度（局域网 + 只用于握手），
/// 但必须有足够的随机性，用一个 xorshift 并混入系统时基。
fn rand_key() -> [u8; 16] {
    use core::sync::atomic::{AtomicU32, Ordering};
    static S: AtomicU32 = AtomicU32::new(0x1234_5678);
    let mut s = S.load(Ordering::Relaxed);
    let seed = embassy_time::Instant::now().as_ticks() as u32;
    s ^= seed | 1;
    let mut out = [0u8; 16];
    for b in &mut out {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        *b = (s & 0xFF) as u8;
    }
    S.store(s, Ordering::Relaxed);
    out
}

fn next_mask() -> [u8; 4] {
    use core::sync::atomic::{AtomicU32, Ordering};
    static S: AtomicU32 = AtomicU32::new(0x9E37_79B9);
    let mut s = S.load(Ordering::Relaxed);
    let mut m = [0u8; 4];
    for b in &mut m {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        *b = (s & 0xFF) as u8;
    }
    S.store(s, Ordering::Relaxed);
    m
}

/// 解析 "192.168.1.10" 这样的点分十进制
pub fn parse_ipv4(s: &str) -> Option<IpAddress> {
    let mut parts = s.split('.');
    let mut o = [0u8; 4];
    for i in 0..4 {
        o[i] = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(IpAddress::Ipv4(Ipv4Address::new(o[0], o[1], o[2], o[3])))
}
