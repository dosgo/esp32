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
use embassy_time::{with_timeout, Duration, WithTimeout};

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
        // socket 自身的超时保持 120 秒（只作为「连上之后对端突然消失」的兜底）。
        // 注意不要在 connect 成功之后再改这个字段 —— 实测会打乱 smoltcp 的
        // 状态，导致 SYN-ACK 回来也不回 ACK，连接永远建不起来。
        sock.set_timeout(Some(Duration::from_secs(120)));
        sock.set_keep_alive(Some(Duration::from_secs(20)));

        // 【建连不能套 with_timeout —— 这是「一直卡在 ws_loop 里什么都不打」的根因】
        //
        // `embassy_time::with_timeout` 内部是 `select(future, Timer)`，
        // 而这块板子上 select + Timer 的唤醒**不可靠**（主流程那段注释、
        // 显示页的 Signal+Timer 都踩过同一个坑）。
        //
        // 实测现象：设备连上 WiFi、DHCP 拿到 IP、进入 ws_loop，然后**串口再无
        // 任何输出** —— 连失败日志都没有，因为整个 with_timeout 就没返回过。
        //
        // 对照实验坐实了这一点：新建的 `esp32s3-wifi-min` 用
        // `set_timeout()` + 裸 `connect().await`（不套 with_timeout），
        // 连 192.168.88.160:8765 一次就通，连百度 80 连续 8 次全通。
        //
        // 所以超时改由 smoltcp 自己管：set_timeout 是「多久没有活动就 abort」，
        // 完全不依赖 embassy 的 Timer。
        sock.set_timeout(Some(Duration::from_secs(10)));
        if sock.connect((ip, port)).await.is_err() {
            return Err("TCP 连接失败/超时");
        }
        // 建连成功后恢复成大超时，避免正常收发被误判
        sock.set_timeout(Some(Duration::from_secs(120)));

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
        // 同样不用 with_timeout（理由见上面建连那段）；用 socket 层的超时。
        //
        // 【临时收紧到 5 秒，但必须在拿到 101 之后立刻放开】
        //
        // smoltcp 的 `set_timeout` **不是「单次读超时」，而是「整条连接空闲多久就
        // abort」**。这里只为了给握手加个上限才调到 5 秒，如果不恢复，
        // 连接就会变成「5 秒没有任何字节往来就断」——
        //
        // 实测症状：设备待机时一个字节都不发，于是**每 5.0 秒被自己踢一次**，
        // 网关侧看到 code=1006 异常断开，设备侧看到「连接已关闭」，
        // 然后 3 秒后重连……一整晚所有「忽然不理人 / 时好时坏」都是这一个 bug。
        // 会话时长日志能直接印证：不说话的会话 5.0s，传着音频的会话 45.2s。
        sock.set_timeout(Some(Duration::from_secs(5)));
        let mut resp = [0u8; 512];
        let mut rlen = 0usize;
        loop {
            if rlen >= resp.len() {
                return Err("响应头过长");
            }
            let got = sock
                .read(&mut resp[rlen..])
                .await
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
                // 【恢复成正常的大超时】——这才是连接期的值。
                // 忘了这一行，连接就会在空闲 5 秒后被 smoltcp 自己 abort。
                // 120 秒只是个「对端已经消失」的兜底：真正判断死链靠
                // main.rs 里 30 秒收不到任何下行数据主动重连。
                conn.sock.set_timeout(Some(Duration::from_secs(120)));
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

    /// 发送缓冲还塞得下吗？
    ///
    /// 【为什么必须查这个再发】
    /// 网关那侧只要慢一下（比如收到 hello 后重建 ASR/TTS 后端，要加载
    /// SenseVoice 1.5s + Piper 4.5s 才能继续读 socket），设备的 TCP 发送缓冲
    /// 几 KB 很快就满，而 `write_all` 会一直等下去 —— 整个会话循环被卡死，
    /// 既发不了心跳也读不了下行，最后要么被网关判死、要么自己 30 秒超时断开。
    /// 实测表现就是「设备刚连上 1 秒就掉，然后疯狂重连」。
    /// 所以每帧发送前先用这个判一下，塞不进去就丢掉这帧（音频丢一帧无所谓，
    /// 链路活着更重要）。
    pub fn can_send(&self) -> bool {
        self.sock.may_send()
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
/// 【放 PSRAM，把内部 RAM 让给必须内部的东西】
///
/// 这块板内部 RAM 堆只有 112KB，而 WiFi 驱动的 DMA 缓冲（static_rx/tx，
/// 射频直接写）**必须**在内部 RAM，搬不走。所以能搬的就该搬走：
/// smoltcp 的 socket rx/tx 缓冲只是任务上下文里的内存拷贝，不参与 DMA，
/// 放 8MB 的 PSRAM 完全安全。
///
/// 这里一次性从 PSRAM 分配、之后轮转复用（AtomicPtr 缓存），反复重连不会泄漏。
/// 注意：**不能每帧现分配现释放** —— 那样会很快把 PSRAM 切碎，
/// 而且连接期间缓冲的生命周期必须长于 socket。
///
/// 分配失败（PSRAM 没起来）时回退内部 RAM，保证还能跑。
///
/// 另：`std::alloc` 之外的这条路径绕开了 vendor/esp-alloc 的 malloc 补丁，
/// 所以不受那个 2KB 阈值影响。
fn mk_static_buf() -> &'static mut [u8] {
    use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
    const POOL: usize = 4;
    // 【保持 4096】曾经试过 8192（缓冲池变 32KB），静态内存冲到 188KB，
    // 内部 RAM 被吃穿、设备直接卡死。4KB 足够。
    const SZ: usize = 4096;

    static PTRS: [AtomicPtr<u8>; POOL] = [const { AtomicPtr::new(core::ptr::null_mut()) }; POOL];
    static IDX: AtomicUsize = AtomicUsize::new(0);

    let i = IDX.fetch_add(1, Ordering::Relaxed) % POOL;
    let mut p = PTRS[i].load(Ordering::Relaxed);

    if p.is_null() {
        let layout = unsafe { core::alloc::Layout::from_size_align_unchecked(SZ, 4) };
        // 优先 PSRAM
        p = unsafe {
            esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout)
        };
        if p.is_null() {
            // 回退：内部 RAM（至少还能跑）
            p = unsafe {
                esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::Internal.into(), layout)
            };
        }
        if p.is_null() {
            panic!("TCP 缓冲分配失败（PSRAM 和内部堆都不足）");
        }
        PTRS[i].store(p, Ordering::Relaxed);
    }

    unsafe { core::slice::from_raw_parts_mut(p, SZ) }
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
