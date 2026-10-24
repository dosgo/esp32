//! 极简 HTTP 服务器（no_std，不引框架）—— 只用来配网和恢复出厂。
//!
//! 路由：
//!   GET /setup                    -> WiFi 配网表单
//!   GET /save?ssid=..&pass=..     -> 保存 STA 账号密码（写 flash + 通知 wifi_task）
//!   GET /status                   -> JSON，给配网页轮询连接结果
//!   GET /factory-reset            -> 恢复出厂设置（清配置区 + 重启）
//!   其它任何 GET                  -> 也返回配网页面（兜住手机的俘虏门户探测）
//!
//! 没有 WebSocket —— 这个项目控制走 Blinker/MQTT，网页只负责配网，所以不需要 sha1 握手。

use core::str;

use embassy_net::tcp::TcpSocket;
use embedded_io_async::Write;
use heapless::String;

use crate::store;

const SETUP_PAGE: &str = include_str!("../setup.html");

pub const HTTP_PORT: u16 = 80;

fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

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
        if find_seq(&buf[..total], b"\r\n\r\n").is_some() {
            return Ok(total);
        }
        if total + 1 >= buf.len() {
            return Err(());
        }
    }
}

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

/// 处理 /save：解出 ssid / 密码 → 写 flash → 通知 wifi_task 去连
/// 解析 /save-blinker?dev=..&iot=..&token=..，写进 flash。
async fn handle_save_blinker(path: &[u8]) -> bool {
    let query = match find_seq(path, b"?") {
        Some(i) => &path[i + 1..],
        None => return false,
    };

    let mut raw = [0u8; 128];
    let mut c = store::BlinkerCreds::new();

    for (key, slot) in [(b"dev".as_slice(), 0u8), (b"iot".as_slice(), 1), (b"token".as_slice(), 2)] {
        let v = query_value(query, key).unwrap_or(b"");
        let n = urldecode(v, &mut raw);
        let s = match str::from_utf8(&raw[..n]) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let ok = match slot {
            0 => c.device.push_str(s).is_ok(),
            1 => c.iot_id.push_str(s).is_ok(),
            _ => c.token.push_str(s).is_ok(),
        };
        if !ok {
            return false;
        }
    }

    if !c.is_set() {
        return false;
    }
    log::info!("[web] 保存 Blinker 凭据: {}", c.device);
    crate::save_blinker_cfg(c).await
}

/// 解析 /save?ssid=..&pass=..，落盘，然后**只返回**解析出来的凭据。
/// 通知 wifi_task 的活儿交给调用方，必须在 HTTP 响应发完之后再做。
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

    // flash 在 main 手里，锁一下拿出来用
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
    } else {
        // 存不住也让这次先连上，至少能用
        log::warn!("[web] WiFi 没写进 flash，本次仍然尝试连接: {}", creds.ssid);
    }
    Some(creds)
}

/// 处理一条 TCP 连接
pub async fn serve_once(socket: &mut TcpSocket<'_>) {
    let mut buf = [0u8; 2048];
    let n = match read_headers(socket, &mut buf).await {
        Ok(n) => n,
        Err(_) => return,
    };
    let req = &buf[..n];
    let path = request_path(req);

    // ---- 保存 Blinker 凭据 ----
    // 必须排在 /save 之前：find_seq 是子串匹配，"/save-blinker" 里也含 "/save"。
    if find_seq(path, b"/save-blinker").is_some() {
        let ok = handle_save_blinker(path).await;
        if ok {
            let _ = write_html(socket, BLINKER_SAVED).await;
            // 凭据是任务启动时读的，改完重启一次最省事（也保证 topic 一起换）
            embassy_time::Timer::after(embassy_time::Duration::from_millis(700)).await;
            esp_hal::system::software_reset();
        } else {
            let _ = write_html(socket, SAVED_FAIL).await;
        }
        return;
    }

    // ---- 保存账号密码 ----
    // 顺序很关键：**先把响应发回去，再通知 wifi_task 去连**。
    // 反过来的话，射频重配/重连会把这半截 HTTP 掐断，手机页面就一直转圈。
    if find_seq(path, b"/save").is_some() {
        match handle_save(path).await {
            Some(creds) => {
                let _ = write_html(socket, SAVED_OK).await;
                // 给协议栈一点时间把数据和 FIN 真正发出去
                embassy_time::Timer::after(embassy_time::Duration::from_millis(400)).await;
                crate::NEW_CREDS.signal(creds);
            }
            None => {
                let _ = write_html(socket, SAVED_FAIL).await;
            }
        }
        return;
    }

    // ---- 状态查询（配网页轮询用）----
    if find_seq(path, b"/status").is_some() {
        let _ = write_status(socket).await;
        return;
    }

    // ---- 诊断日志（USB 唤醒排查用）----
    if find_seq(path, b"/log").is_some() {
        let _ = write_log(socket).await;
        return;
    }

    // ---- 电源控制：唤醒 / 睡眠（热点侧、局域网侧共用）----
    if find_seq(path, b"/wake").is_some() {
        log::info!("[web] 收到唤醒请求");
        // 先把响应发回去，再去拉总线：唤醒后如果主机重新枚举，这条连接会断，
        // 没发完的话手机上会看到"发送失败"，体验上像没成功。
        let _ = write_html(socket, WAKE_SENT).await;
        crate::request_wake().await;
        return;
    }
    if find_seq(path, b"/sleep").is_some() {
        log::info!("[web] 收到睡眠请求");
        let _ = write_html(socket, SLEEP_SENT).await;
        crate::request_sleep().await;
        return;
    }

    // ---- 恢复出厂设置 ----
    if find_seq(path, b"/factory-reset").is_some() {
        log::warn!("[web] 收到恢复出厂设置请求");
        if !crate::factory_reset_flash().await {
            log::error!("[web] 恢复出厂失败（flash 擦不掉）");
        }
        let _ = write_html(socket, RESET_OK).await;
        // 先把页面发出去，再重启
        embassy_time::Timer::after(embassy_time::Duration::from_millis(600)).await;
        esp_hal::system::software_reset();
    }

    // ---- 其它一切路径都返回配网页 ----
    let _ = write_html(socket, SETUP_PAGE).await;
}

const WAKE_SENT: &str = "已发送唤醒指令";
const SLEEP_SENT: &str = "已发送睡眠指令";

const BLINKER_SAVED: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>Blinker 已保存</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#3ad06a'>Blinker 凭据已保存，设备正在重启…</h2>\
<p>约 8 秒后重新连上热点 <b>ESP32-POWER</b>，再打开 <a href='/' style='color:#40c8e0'>控制页</a> 即可。</p>\
<p style='color:#889099'>重启后会自动用新凭据连 Blinker，App 里应能看到设备在线。</p>";

const SAVED_OK: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>已保存</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#3ad06a'>已保存，正在连接…</h2>\
<p>ESP32 正在尝试连接这个 WiFi，连上才能接上 Blinker 云。</p>\
<p><a href='/setup' style='color:#40c8e0'>查看连接结果</a></p>\
<p style='color:#889099'>热点 ESP32-POWER（192.168.4.1）一直可用，随时能回来改。</p>";

const SAVED_FAIL: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>保存失败</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#e04040'>保存失败</h2>\
<p>可能是名字太长（最多 32 字符）或密码太长（最多 64 字符）。</p>\
<p><a href='/setup' style='color:#40c8e0'>返回重填</a></p>";

const RESET_OK: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>已恢复出厂</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#f0b040'>已恢复出厂设置，正在重启…</h2>\
<p>已保存的 WiFi 全部清掉了。约 8 秒后重新连接热点 <b>ESP32-POWER</b> \
（密码 <b>power1234</b>），再打开 http://192.168.4.1 重新配网。</p>";

/// 当前网络状态 JSON，配网页轮询它来判断有没有连上
async fn write_status(socket: &mut TcpSocket<'_>) -> Result<(), ()> {
    let mut body: String<220> = String::new();
    {
        let info = crate::NET.lock().await;
        let state = match info.state {
            1 => "connecting",
            2 => "connected",
            3 => "failed",
            _ => "idle",
        };
        let _ = core::fmt::Write::write_fmt(&mut body, format_args!("{{\"sta\":\"{state}\""));
        match info.ip {
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
        // 连接失败的原因，页面上直接显示（转义一下引号，别把 JSON 拆坏）
        let _ = core::fmt::Write::write_fmt(&mut body, format_args!(",\"err\":\""));
        for ch in info.err.chars() {
            let c = if ch == '"' { '\'' } else { ch };
            let _ = core::fmt::Write::write_char(&mut body, c);
        }
        let _ = core::fmt::Write::write_fmt(&mut body, format_args!("\""));
    }
    // 当前生效的 Blinker 设备名（flash 里存过就是那个，否则是固件内置的）
    {
        let b = crate::BLINKER_CFG.lock().await;
        let _ = core::fmt::Write::write_fmt(&mut body, format_args!(",\"bdev\":\""));
        if b.is_set() {
            for ch in b.device.chars() {
                let c = if ch == '"' { '\'' } else { ch };
                let _ = core::fmt::Write::write_char(&mut body, c);
            }
        } else {
            let _ = core::fmt::Write::write_fmt(&mut body, format_args!("（固件内置）"));
        }
        let _ = core::fmt::Write::write_fmt(&mut body, format_args!("\"}}"));
    }
    write_html(socket, &body).await
}

/// 把 main.rs 里的诊断环形缓冲以纯文本吐出来。
///
/// 注意：必须带 Content-Length。之前这版没写长度、想靠"关连接表示结束"，
/// 结果手机浏览器一直等 EOF、服务端在等下一个 accept，互相等死，
/// 整个网页服务都被堵住了。
async fn write_log(socket: &mut TcpSocket<'_>) -> Result<(), ()> {
    let g = crate::LOGS.lock().await;
    let (start, count) = g.order();

    // 先算正文长度（每行后面补一个 '\n'）
    let mut total = 0usize;
    for i in 0..count {
        let n = g.len[(start + i) % crate::LOG_LINES] as usize;
        if n > 0 {
            total += n + 1;
        }
    }

    let mut head = [0u8; 160];
    let hdr = {
        let mut w = Cursor {
            buf: &mut head,
            pos: 0,
        };
        w.push(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: ");
        let mut num = [0u8; 12];
        let s = itoa(total, &mut num);
        w.push(s);
        w.push(b"\r\nConnection: close\r\n\r\n");
        w.done().len()
    };
    socket.write_all(&mut head[..hdr]).await.map_err(|_| ())?;

    for i in 0..count {
        let idx = (start + i) % crate::LOG_LINES;
        let n = g.len[idx] as usize;
        if n == 0 {
            continue;
        }
        socket.write_all(&g.buf[idx][..n]).await.map_err(|_| ())?;
        socket.write_all(b"\n").await.map_err(|_| ())?;
    }
    drop(g);

    let _ = socket.flush().await;
    Ok(())
}

/// 返回一段 HTML。/status 也走这个（内容是 JSON，前端用 fetch 解析纯文本）。
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
