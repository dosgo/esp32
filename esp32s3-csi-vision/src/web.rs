//! 极简 HTTP + WebSocket（no_std，不引框架）。
//!
//! 路由：
//!   GET /                        -> CSI 可视化主页
//!   GET /setup                   -> 配网页面
//!   GET /save?ssid=..&pass=..    -> 保存 Wi-Fi 凭据（写 flash + 重启去连）
//!   GET /clear                   -> 忘掉已存的 Wi-Fi
//!   GET /status                  -> JSON：连接状态 / IP / RSSI / 温度 / 帧数
//!   GET /ws                      -> WebSocket 升级，之后双向推 CSI 数据
//!
//! WebSocket 握手（SHA1 + base64）和帧解析是从已验证工程搬过来的。

use core::str;
use core::sync::atomic::Ordering;

use embassy_net::tcp::TcpSocket;
use embedded_io_async::Write;
use sha1::{Digest, Sha1};

use crate::store;

const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// 两个页面都编进 flash（16MB，完全不心疼）。
const PAGE: &str = include_str!("../index.html");
const SETUP_PAGE: &str = include_str!("../setup.html");

pub enum Served {
    /// 已升级成 WebSocket，调用方接手数据收发
    WebSocket,
    /// 这一轮只是普通 HTTP
    Page,
}

// ---------------- WebSocket 握手 ----------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn ws_accept(key: &[u8], out: &mut [u8; 28]) {
    let mut h = Sha1::new();
    h.update(key);
    h.update(WS_GUID);
    let d = h.finalize(); // 20 字节

    let mut i = 0usize;
    let mut o = 0usize;
    while i < 20 {
        let n = if 20 - i >= 3 { 3 } else { 20 - i };
        let b0 = d[i];
        let b1 = if n > 1 { d[i + 1] } else { 0 };
        let b2 = if n > 2 { d[i + 2] } else { 0 };
        out[o] = B64[(b0 >> 2) as usize];
        out[o + 1] = B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize];
        out[o + 2] = if n > 1 {
            B64[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize]
        } else {
            b'='
        };
        out[o + 3] = if n > 2 { B64[(b2 & 0x3F) as usize] } else { b'=' };
        i += 3;
        o += 4;
    }
}

/// 取 header 值。**必须按「行首 = 名字」精确匹配**：Chrome 发握手时
/// `Connection: Upgrade` 排在 `Upgrade: websocket` 前面，子串搜索会先命中前者。
fn header_value<'a>(req: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut lines = req.split(|&b| b == b'\n');
    lines.next()?;
    for raw in lines {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            break;
        }
        if line.len() > name.len()
            && line[..name.len()].eq_ignore_ascii_case(name)
            && line[name.len()] == b':'
        {
            return Some(trim_ascii(&line[name.len() + 1..]));
        }
    }
    None
}

fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let Some((b, rest)) = s.split_first() {
        if b.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let Some((b, rest)) = s.split_last() {
        if b.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn request_path(req: &[u8]) -> &[u8] {
    let line_end = find_seq(req, b"\r\n").unwrap_or(req.len());
    let line = &req[..line_end];
    let start = find_seq(line, b" ").map(|i| i + 1).unwrap_or(0);
    let rest = &line[start..];
    let end = find_seq(rest, b" ").unwrap_or(rest.len());
    &rest[..end]
}

/// `path` 是否就是 `name`，或者 `name?query`。
///
/// 不能用子串匹配：`/save` 会命中 `/save-xxx`，而且 `/setup` 和 `/set` 也分不开。
fn route_is(path: &[u8], name: &[u8]) -> bool {
    path == name || (path.len() > name.len() && path.starts_with(name) && path[name.len()] == b'?')
}

/// 从 query 串里取某个参数（不 urldecode，调用方自己处理）
fn query_value<'a>(query: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut rest = query;
    loop {
        let amp = find_seq(rest, b"&").unwrap_or(rest.len());
        let part = &rest[..amp];
        if let Some(eq) = find_seq(part, b"=") {
            if &part[..eq] == key {
                return Some(&part[eq + 1..]);
            }
        }
        if amp >= rest.len() {
            return None;
        }
        rest = &rest[amp + 1..];
    }
}

/// %XX 和 '+' 解码。中文 SSID 浏览器会编成 UTF-8 百分号串，这里原样还原。
fn urldecode(src: &[u8], out: &mut [u8]) -> usize {
    let mut i = 0usize;
    let mut n = 0usize;
    while i < src.len() && n < out.len() {
        match src[i] {
            b'%' if i + 2 < src.len() => match (hex_val(src[i + 1]), hex_val(src[i + 2])) {
                (Some(h), Some(l)) => {
                    out[n] = (h << 4) | l;
                    n += 1;
                    i += 3;
                }
                _ => {
                    out[n] = src[i];
                    n += 1;
                    i += 1;
                }
            },
            b'+' => {
                out[n] = b' ';
                n += 1;
                i += 1;
            }
            c => {
                out[n] = c;
                n += 1;
                i += 1;
            }
        }
    }
    n
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---------------- 底层收发 ----------------

async fn read_some(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, ()> {
    match s.read(buf).await {
        Ok(0) | Err(_) => Err(()),
        Ok(n) => Ok(n),
    }
}

async fn read_all(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(), ()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = read_some(s, &mut buf[done..]).await?;
        done += n;
    }
    Ok(())
}

async fn read_headers(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, ()> {
    let mut total = 0usize;
    loop {
        let n = read_some(s, &mut buf[total..]).await?;
        total += n;
        if find_seq(&buf[..total], b"\r\n\r\n").is_some() {
            return Ok(total);
        }
        if total + 1 >= buf.len() {
            return Err(());
        }
    }
}

/// 发一段带 Content-Type 的响应。页面 20 多 KB，socket 缓冲只有几 KB，必须分块写。
async fn write_body(socket: &mut TcpSocket<'_>, ctype: &str, body: &[u8]) -> Result<(), ()> {
    let mut head = heapless::String::<128>::new();
    use core::fmt::Write as _;
    let _ = write!(
        head,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        ctype,
        body.len()
    );
    socket.write_all(head.as_bytes()).await.map_err(|_| ())?;

    let mut chunk = [0u8; 512];
    let mut off = 0usize;
    while off < body.len() {
        let n = (body.len() - off).min(512);
        chunk[..n].copy_from_slice(&body[off..off + n]);
        socket.write_all(&chunk[..n]).await.map_err(|_| ())?;
        off += n;
    }
    socket.flush().await.map_err(|_| ())
}

async fn write_html(socket: &mut TcpSocket<'_>, body: &str) -> Result<(), ()> {
    write_body(socket, "text/html; charset=utf-8", body.as_bytes()).await
}

async fn write_redirect(socket: &mut TcpSocket<'_>, to: &str) -> Result<(), ()> {
    let mut head = heapless::String::<128>::new();
    use core::fmt::Write as _;
    let _ = write!(
        head,
        "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        to
    );
    socket.write_all(head.as_bytes()).await.map_err(|_| ())?;
    socket.flush().await.map_err(|_| ())
}

// ---------------- WebSocket 帧 ----------------

pub const OP_TEXT: u8 = 0x1;
pub const OP_BIN: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
#[allow(dead_code)]
pub const OP_PONG: u8 = 0xA;

/// 读一帧。客户端发的帧必须带 mask，这里做 unmask。
pub async fn ws_read(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(u8, usize), ()> {
    let mut head = [0u8; 2];
    read_all(s, &mut head).await?;

    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7F) as usize;

    if len == 126 {
        let mut ext = [0u8; 2];
        read_all(s, &mut ext).await?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        read_all(s, &mut ext).await?;
        len = u64::from_be_bytes(ext) as usize;
    }

    let cap = len.min(buf.len());
    let mask = if masked {
        let mut m = [0u8; 4];
        read_all(s, &mut m).await?;
        m
    } else {
        [0u8; 4]
    };

    read_all(s, &mut buf[..cap]).await?;
    if masked {
        for i in 0..cap {
            buf[i] ^= mask[i % 4];
        }
    }
    let mut skip = len - cap;
    let mut tmp = [0u8; 32];
    while skip > 0 {
        let n = skip.min(32);
        read_all(s, &mut tmp[..n]).await?;
        skip -= n;
    }

    Ok((opcode, cap))
}

async fn frame_head(s: &mut TcpSocket<'_>, opcode: u8, len: usize) -> Result<(), ()> {
    let mut head = [0u8; 4];
    head[0] = 0x80 | opcode;
    if len < 126 {
        head[1] = len as u8;
        s.write_all(&head[..2]).await.map_err(|_| ())
    } else if len < 65536 {
        head[1] = 126;
        head[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        s.write_all(&head[..4]).await.map_err(|_| ())
    } else {
        Err(())
    }
}

/// 发一帧文本（服务端 -> 客户端不需要 mask）
pub async fn ws_text(s: &mut TcpSocket<'_>, text: &str) -> Result<(), ()> {
    let b = text.as_bytes();
    frame_head(s, OP_TEXT, b.len()).await?;
    let mut chunk = [0u8; 128];
    let mut off = 0usize;
    while off < b.len() {
        let n = (b.len() - off).min(128);
        chunk[..n].copy_from_slice(&b[off..off + n]);
        s.write_all(&chunk[..n]).await.map_err(|_| ())?;
        off += n;
    }
    Ok(())
}

/// 发一帧二进制。CSI 帧 8+56=64 字节，一次写完。
pub async fn ws_bin(s: &mut TcpSocket<'_>, payload: &[u8]) -> Result<(), ()> {
    frame_head(s, OP_BIN, payload.len()).await?;
    s.write_all(payload).await.map_err(|_| ())
}

pub async fn ws_pong(s: &mut TcpSocket<'_>) -> Result<(), ()> {
    s.write_all(&[0x8A, 0x00]).await.map_err(|_| ())
}

// ---------------- 状态 JSON ----------------

async fn write_status(socket: &mut TcpSocket<'_>) {
    let up = crate::STA_UP.load(Ordering::Relaxed);
    let ip = crate::STA_IP.load(Ordering::Relaxed).to_be_bytes();
    let rssi = crate::STA_RSSI.load(Ordering::Relaxed);
    let temp = crate::TEMP_X10.load(Ordering::Relaxed);
    let frames = crate::csi::FRAMES.load(Ordering::Relaxed);

    let mut s = heapless::String::<256>::new();
    use core::fmt::Write as _;
    if up {
        let _ = write!(
            s,
            "{{\"sta\":true,\"ip\":\"{}.{}.{}.{}\",\"rssi\":{},\"temp\":{},\"frames\":{}}}",
            ip[0], ip[1], ip[2], ip[3], rssi, temp, frames
        );
    } else {
        let _ = write!(
            s,
            "{{\"sta\":false,\"ip\":\"\",\"rssi\":0,\"temp\":{},\"frames\":{}}}",
            temp, frames
        );
    }
    let _ = write_body(socket, "application/json", s.as_bytes()).await;
}

// ---------------- 配网 ----------------

/// 解析 /save?ssid=..&pass=..，返回解析出来的凭据。
async fn handle_save(path: &[u8]) -> Option<store::Creds> {
    let query = match find_seq(path, b"?") {
        Some(i) => &path[i + 1..],
        None => return None,
    };

    let mut raw = [0u8; 256];
    let mut creds = store::Creds::default();

    for (key, is_ssid) in [(b"ssid".as_slice(), true), (b"pass".as_slice(), false)] {
        let v = query_value(query, key).unwrap_or(b"");
        let n = urldecode(v, &mut raw);
        let s = match str::from_utf8(&raw[..n]) {
            Ok(s) => s,
            Err(_) => return None,
        };
        let ok = if is_ssid {
            creds.ssid.push_str(s).is_ok()
        } else {
            creds.pass.push_str(s).is_ok()
        };
        if !ok {
            return None;
        }
    }

    if !creds.is_set() {
        return None;
    }
    Some(creds)
}

// ---------------- 路由 ----------------

/// 处理一次 HTTP 连接。返回 WebSocket 表示已经升级，调用方接着收发。
pub async fn serve_once(socket: &mut TcpSocket<'_>) -> Served {
    let mut buf = [0u8; 1024];
    let n = match read_headers(socket, &mut buf).await {
        Ok(n) => n,
        Err(_) => return Served::Page,
    };
    let req = &buf[..n];
    let path = request_path(req);

    // ---- WebSocket 升级 ----
    if route_is(path, b"/ws")
        && header_value(req, b"Upgrade").map_or(false, |v| v.eq_ignore_ascii_case(b"websocket"))
    {
        let key = match header_value(req, b"Sec-WebSocket-Key") {
            Some(k) => k,
            None => {
                let _ = write_redirect(socket, "/").await;
                return Served::Page;
            }
        };

        let mut accept = [0u8; 28];
        ws_accept(key, &mut accept);

        let mut resp = [0u8; 160];
        let mut w = 0usize;
        for part in [
            &b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: "[..],
            &accept[..],
            &b"\r\n\r\n"[..],
        ] {
            resp[w..w + part.len()].copy_from_slice(part);
            w += part.len();
        }
        if socket.write_all(&resp[..w]).await.is_err() {
            return Served::Page;
        }
        let _ = socket.flush().await;
        return Served::WebSocket;
    }

    // ---- 保存 Wi-Fi ----
    //
    // 顺序很关键：**先把凭据写进 flash，再回 302**。
    // 回完立刻重启 —— 重启是最省事的做法：Wi-Fi 模式要从
    // 「APSTA + 占位 STA」变成「APSTA + 真实 STA」，中间射频要重配、
    // 栈要重新 DHCP，热更新路径的边界情况远比重启多。
    if route_is(path, b"/save") {
        match handle_save(path).await {
            Some(creds) => {
                let ok = crate::save_creds(creds).await;
                let _ = write_redirect(socket, if ok { "/setup?saved=1" } else { "/setup?saved=0" }).await;
                // 给协议栈 400ms 把响应和 FIN 真正发出去，再重启
                embassy_time::Timer::after(embassy_time::Duration::from_millis(400)).await;
                if ok {
                    crate::restart_now();
                }
            }
            None => {
                let _ = write_redirect(socket, "/setup?saved=0").await;
            }
        }
        return Served::Page;
    }

    // ---- 清除已存 Wi-Fi ----
    if route_is(path, b"/clear") {
        crate::clear_creds().await;
        let _ = write_body(socket, "text/plain", b"cleared").await;
        embassy_time::Timer::after(embassy_time::Duration::from_millis(500)).await;
        crate::restart_now();
    }

    // ---- 状态查询 ----
    if route_is(path, b"/status") {
        write_status(socket).await;
        return Served::Page;
    }

    // ---- 配网页 ----
    if route_is(path, b"/setup") {
        let _ = write_html(socket, SETUP_PAGE).await;
        return Served::Page;
    }

    // ---- 其它一切路径 ----
    //
    // `/` 永远给主页（手动输 IP 也能直接看 CSI 成像）。
    // 陌生路径（含 /favicon.ico 和手机系统的联网探测 /generate_204）：
    //   - 还没配网 → 302 到 /setup。安卓/iOS 连上无外网的热点时会自动
    //     请求这些地址，跳转到配网页后系统会直接弹出「登录网络」窗口，
    //     用户不用手动敲 192.168.4.1
    //   - 配过网 → 302 回主页
    if path == b"/" || path.is_empty() {
        let _ = write_html(socket, PAGE).await;
    } else {
        let target = if crate::HAS_CREDS.load(Ordering::Relaxed) {
            "/"
        } else {
            "/setup"
        };
        let _ = write_redirect(socket, target).await;
    }
    Served::Page
}
