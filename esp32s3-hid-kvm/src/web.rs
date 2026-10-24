//! 极简 HTTP + WebSocket 服务器（no_std，不引框架）。
//!
//! 路由：
//!   GET /ws                       -> WebSocket 升级，之后收键鼠指令
//!   GET /setup                    -> WiFi 配网表单
//!   GET /save?ssid=..&pass=..     -> 保存 STA 账号密码（写 flash + 通知 wifi_task）
//!   GET /status                   -> JSON，给配网页轮询连接结果
//!   其它任何 GET                  -> 返回控制页面（顺带兜住手机的俘虏门户探测）
//!
//! 为什么自己写：picoserve 之类框架带 proc-macro / serde / Rust 2024 edition，
//! 而这里我们只需要几个路由 + 一个 WebSocket，自写反而更少出编译问题。
//!
//! 线协议（文本，空格分隔，便于调试）：
//!   k <mod> <k1> <k2> <k3> <k4> <k5> <k6>   键盘状态（浏览器端维护 6KRO）
//!   m <btn> <dx> <dy> <wheel>               鼠标（相对位移）

use core::str;

use embassy_net::tcp::TcpSocket;
use embedded_io_async::Write;
use heapless::String;
use sha1::{Digest, Sha1};

use crate::store;

const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const PAGE: &str = include_str!("../index.html");
const SETUP_PAGE: &str = include_str!("../setup.html");

pub const HTTP_PORT: u16 = 80;

/// 一条连接处理完之后是什么类型
pub enum Served {
    /// 已经升级成 WebSocket，调用方接着收键鼠指令
    WebSocket,
    /// 返回了一个 HTTP 页面
    Page,
    /// 出错 / 读不到请求
    None,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// SHA1(key + GUID) 再 base64 —— WebSocket 握手的 Sec-WebSocket-Accept
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

// ---------------- 小工具 ----------------

async fn read_some(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, ()> {
    match s.read(buf).await {
        Ok(0) | Err(_) => Err(()),
        Ok(n) => Ok(n),
    }
}

/// 一直读到出现 \r\n\r\n（HTTP 头结束）
async fn read_headers(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, ()> {
    let mut total = 0usize;
    loop {
        let n = read_some(s, &mut buf[total..]).await?;
        total += n;
        if let Some(pos) = find_seq(&buf[..total], b"\r\n\r\n") {
            let _ = pos;
            return Ok(total);
        }
        if total + 1 >= buf.len() {
            return Err(());
        }
    }
}

fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// 取某个 header 的值（不区分大小写匹配名字）
fn header_value<'a>(req: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let start = find_seq(req, name)?;
    let rest = &req[start + name.len()..];
    if !rest.starts_with(b":") {
        return None;
    }
    let rest = &rest[1..];
    let end = find_seq(rest, b"\r\n")?;
    Some(rest[..end].trim_ascii())
}

// ---------------- WebSocket 帧 ----------------

pub const OP_TEXT: u8 = 0x1;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
#[allow(dead_code)]
pub const OP_PONG: u8 = 0xA;

/// 读一帧。payload 写进 `buf`，返回 (opcode, 长度)。
/// 客户端发的帧**必须**带 mask，这里做 unmask。
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

    let cap = if len < buf.len() { len } else { buf.len() - 1 };

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
    // 超出缓冲的余下字节直接丢掉（控制指令都很短，不会发生）
    let mut skip = len - cap;
    let mut tmp = [0u8; 32];
    while skip > 0 {
        let n = if skip < 32 { skip } else { 32 };
        read_all(s, &mut tmp[..n]).await?;
        skip -= n;
    }

    Ok((opcode, cap))
}

async fn read_all(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(), ()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = read_some(s, &mut buf[done..]).await?;
        done += n;
    }
    Ok(())
}

/// 发一帧文本（服务端 -> 客户端不需要 mask）
pub async fn ws_text(s: &mut TcpSocket<'_>, text: &str) -> Result<(), ()> {
    let b = text.as_bytes();
    let len = b.len();
    if len < 126 {
        let mut head = [0x81u8, len as u8];
        s.write_all(&mut head).await.map_err(|_| ())?;
    } else if len < 65536 {
        let mut head = [0x81u8, 126u8, 0, 0];
        head[2..].copy_from_slice(&(len as u16).to_be_bytes());
        s.write_all(&mut head).await.map_err(|_| ())?;
    } else {
        return Err(());
    }
    // write_all 需要 &mut，b 是共享引用，按块拷一份再发
    let mut chunk = [0u8; 128];
    let mut off = 0usize;
    while off < len {
        let n = if len - off < 128 { len - off } else { 128 };
        chunk[..n].copy_from_slice(&b[off..off + n]);
        s.write_all(&mut chunk[..n]).await.map_err(|_| ())?;
        off += n;
    }
    Ok(())
}

pub async fn ws_pong(s: &mut TcpSocket<'_>) -> Result<(), ()> {
    s.write_all(&mut [0x8A, 0x00]).await.map_err(|_| ())
}

// ---------------- 连接处理 ----------------

/// 取请求行里的路径（"GET /setup?x=1 HTTP/1.1" -> "/setup?x=1"）
fn request_path(req: &[u8]) -> &[u8] {
    let line_end = find_seq(req, b"\r\n").unwrap_or(req.len());
    let line = &req[..line_end];
    let start = match find_seq(line, b" ") {
        Some(i) => i + 1,
        None => 0,
    };
    let rest = &line[start..];
    let end = find_seq(rest, b" ").unwrap_or(rest.len());
    &rest[..end]
}

/// 从 query 串里取某个参数的值（不解码，调用方自己 urldecode）
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

/// %XX 和 '+' 解码。中文 SSID 浏览器会编成 UTF-8 百分号串，原样还原成字节。
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

/// 处理 /save：解出 ssid / 密码 → 写 flash → 通知 wifi_task 去连。
/// 返回是否保存成功。
async fn handle_save(path: &[u8]) -> bool {
    let query = match find_seq(path, b"?") {
        Some(i) => &path[i + 1..],
        None => return false,
    };

    let mut raw = [0u8; 256];
    let mut creds = store::Creds::default();

    for (key, dst) in [(b"ssid".as_slice(), 0usize), (b"pass".as_slice(), 1usize)] {
        let v = query_value(query, key).unwrap_or(b"");
        let n = urldecode(v, &mut raw);
        let s = match str::from_utf8(&raw[..n]) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let ok = if dst == 0 {
            creds.ssid.push_str(s).is_ok()
        } else {
            creds.pass.push_str(s).is_ok()
        };
        if !ok {
            return false;
        }
    }

    if !creds.is_set() {
        return false;
    }

    // flash 在 main 手里，锁一下拿出来用。擦扇区不能在中断里做，但这里本来就是任务上下文。
    let mut g = crate::FLASH.lock().await;
    let ok = match g.as_mut() {
        Some(f) => store::save(f, &creds),
        None => {
            log::warn!("[web] flash 不可用，配网不会被记住");
            false
        }
    };
    drop(g);

    if ok {
        log::info!("[web] 已保存 WiFi: {}", creds.ssid);
        crate::NEW_CREDS.signal(creds);
    }
    true
}

/// 处理一条 TCP 连接。
pub async fn serve_once(socket: &mut TcpSocket<'_>) -> Served {
    let mut buf = [0u8; 2048];
    let n = match read_headers(socket, &mut buf).await {
        Ok(n) => n,
        Err(_) => return Served::None,
    };
    let req = &buf[..n];
    let path = request_path(req);

    // ---- 配网：保存账号密码 ----
    if find_seq(path, b"/save").is_some() {
        let ok = handle_save(path).await;
        let body = if ok {
            SAVED_OK
        } else {
            SAVED_FAIL
        };
        let _ = write_html(socket, body).await;
        return Served::Page;
    }

    // ---- 配网：状态查询（配网页轮询用）----
    if find_seq(path, b"/status").is_some() {
        let _ = write_status(socket).await;
        return Served::Page;
    }

    // ---- 配网：表单页 ----
    if find_seq(path, b"/setup").is_some() {
        let _ = write_html(socket, SETUP_PAGE).await;
        return Served::Page;
    }

    // ---- 恢复出厂设置 ----
    if find_seq(path, b"/factory-reset").is_some() {
        log::warn!("[web] 收到恢复出厂设置请求");
        crate::set_hint("factory reset...").await;
        if !crate::factory_reset_flash().await {
            log::error!("[web] 恢复出厂失败（flash 擦不掉）");
        }
        let _ = write_html(socket, RESET_OK).await;
        // 先把页面发出去，再重启
        embassy_time::Timer::after(embassy_time::Duration::from_millis(600)).await;
        esp_hal::system::software_reset();
    }

    let is_ws = find_seq(path, b"/ws").is_some()
        && header_value(req, b"Upgrade")
            .map_or(false, |v| find_seq(v, b"websocket").is_some());

    if !is_ws {
        // 任何别的请求都返回控制页面，省得处理 404
        // （顺带把手机的俘虏门户探测也引到这里）
        let _ = write_html(socket, PAGE).await;
        return Served::Page;
    }

    let key = match header_value(req, b"Sec-WebSocket-Key") {
        Some(k) => k,
        None => return Served::None,
    };

    let mut accept = [0u8; 28];
    ws_accept(key, &mut accept);

    let mut resp = [0u8; 200];
    let body = build_upgrade(&accept, &mut resp);
    if socket.write_all(body).await.is_err() {
        return Served::None;
    }
    let _ = socket.flush().await;
    Served::WebSocket
}

const SAVED_OK: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>已保存</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#3ad06a'>已保存，正在连接…</h2>\
<p>ESP32 正在尝试连接这个 WiFi。约 10~20 秒后回来看看状态：</p>\
<p><a href='/setup' style='color:#40c8e0'>查看连接结果</a></p>\
<p style='color:#889099'>连上之后，屏幕会显示局域网 IP；热点 192.168.4.1 始终可用。</p>";

const RESET_OK: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>已恢复出厂</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#f0b040'>已恢复出厂设置，正在重启…</h2>\
<p>已保存的 WiFi 全部清掉了。约 8 秒后重新连接热点 <b>ESP32-KVM</b> \
（密码 <b>kvm12345</b>），再打开 http://192.168.4.1/setup 重新配网。</p>\
<p style='color:#889099'>如果热点没马上出现，等几秒刷新一下 WiFi 列表。</p>";

const SAVED_FAIL: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>保存失败</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#e04040'>保存失败</h2>\
<p>可能是名字太长（最多 32 字符）或密码太长（最多 64 字符）。</p>\
<p><a href='/setup' style='color:#40c8e0'>返回重填</a></p>";

/// 当前网络状态 JSON，配网页轮询它来判断有没有连上
async fn write_status(socket: &mut TcpSocket<'_>) -> Result<(), ()> {
    let mut body: String<160> = String::new();
    {
        let st = crate::lcd::STATUS.lock().await;
        let state = st.sta_state.label();
        let _ = core::fmt::Write::write_fmt(
            &mut body,
            format_args!("{{\"sta\":\"{state}\""),
        );
        match st.sta_ip {
            Some(ip) => {
                let _ = core::fmt::Write::write_fmt(
                    &mut body,
                    format_args!(",\"ip\":\"{}.{}.{}.{}\"", ip[0], ip[1], ip[2], ip[3]),
                );
            }
            None => {
                let _ = core::fmt::Write::write_fmt(&mut body, format_args!(",\"ip\":\"\""));
            }
        }
        let _ = core::fmt::Write::write_fmt(
            &mut body,
            format_args!(",\"ap\":\"{}.{}.{}.{}\"", 192, 168, 4, 1),
        );
        let _ = core::fmt::Write::write_fmt(&mut body, format_args!("}}"));
    }
    write_html(socket, &body).await
}

fn build_upgrade<'a>(accept: &'a [u8; 28], out: &'a mut [u8; 200]) -> &'a [u8] {
    let mut w = Cursor { buf: out, pos: 0 };
    w.push(b"HTTP/1.1 101 Switching Protocols\r\n");
    w.push(b"Upgrade: websocket\r\n");
    w.push(b"Connection: Upgrade\r\n");
    w.push(b"Sec-WebSocket-Accept: ");
    w.push(accept);
    w.push(b"\r\n\r\n");
    w.done()
}

struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn push(&mut self, b: &[u8]) {
        let n = if b.len() < self.buf.len() - self.pos {
            b.len()
        } else {
            self.buf.len() - self.pos
        };
        self.buf[self.pos..self.pos + n].copy_from_slice(&b[..n]);
        self.pos += n;
    }
    fn done(self) -> &'a [u8] {
        &self.buf[..self.pos]
    }
}

/// 返回一段 HTML。/status 也走这个（内容是 JSON，但 Content-Type 无所谓，
/// 前端用 fetch + JSON.parse 解析纯文本）。
async fn write_html(socket: &mut TcpSocket<'_>, page: &str) -> Result<(), ()> {
    let mut head = [0u8; 128];
    let hdr = {
        let mut w = Cursor {
            buf: &mut head,
            pos: 0,
        };
        w.push(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: ");
        let mut num = [0u8; 12];
        let s = itoa(page.len(), &mut num);
        w.push(s);
        w.push(b"\r\nConnection: close\r\n\r\n");
        w.done().len()
    };
    socket.write_all(&mut head[..hdr]).await.map_err(|_| ())?;

    // 分块写出，避免一次占太多栈
    let b = page.as_bytes();
    let mut off = 0usize;
    let mut chunk = [0u8; 512];
    while off < b.len() {
        let n = if b.len() - off < 512 { b.len() - off } else { 512 };
        chunk[..n].copy_from_slice(&b[off..off + n]);
        socket.write_all(&mut chunk[..n]).await.map_err(|_| ())?;
        off += n;
    }
    let _ = socket.flush().await;
    Ok(())
}

fn itoa(mut v: usize, out: &mut [u8; 12]) -> &[u8] {
    let mut i = out.len();
    if v == 0 {
        i -= 1;
        out[i] = b'0';
        return &out[i..];
    }
    while v > 0 && i > 0 {
        i -= 1;
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &out[i..]
}
