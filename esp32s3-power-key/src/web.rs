//! 极简 HTTP + WebSocket 服务器（no_std，不引框架）。
//!
//! 路由：
//!   GET /                         -> 设置页（WiFi / Blinker / 日志 / 恢复出厂）**主页**
//!   GET /kvm                      -> 键鼠控制页（虚拟键盘 + 鼠标 + 唤醒按钮）
//!   GET /ws                       -> WebSocket 升级，之后收键鼠 + 电源指令
//!   GET /save?ssid=..&pass=..     -> 保存 STA 账号密码（写 flash + 通知 wifi_task）
//!   GET /save-blinker?dev=..      -> 保存 Blinker 凭据
//!   GET /status                   -> JSON，给设置页轮询连接结果
//!   GET /log                      -> 诊断日志纯文本
//!   GET /wake | /sleep            -> 电源控制（不用开 WebSocket 的快捷入口）
//!   GET /clear-bond               -> 只清蓝牙配对（WiFi/Blinker 保留）+ 重启
//!   GET /factory-reset            -> 恢复出厂设置（清配置区 + 重启）
//!
//! WebSocket 这块是从 esp32s3-hid-kvm 搬过来的（sha1 握手 + 帧解析），
//! 因为电源键原来只有 HTTP，不带 WS。

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
}

// ---------------- WebSocket 握手 ----------------

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

/// 取某个 header 的值。
///
/// **必须按"行首 = 名字"精确匹配**，不能用 `find_seq` 在整段请求里搜子串 ——
/// Chrome 发 WebSocket 握手时 `Connection: Upgrade` 排在 `Upgrade: websocket`
/// **前面**，子串匹配会先命中前者，取到的不是 header 值，于是 `is_ws` 被判成
/// false，服务端拿 200 + 21KB 页面去回应 WebSocket 握手，浏览器直接断开。
fn header_value<'a>(req: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut lines = req.split(|&b| b == b'\n');
    lines.next()?; // 跳过请求行
    for raw in lines {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            break; // 空行 = header 结束
        }
        if line.len() > name.len()
            && line[..name.len()].eq_ignore_ascii_case(name)
            && line[name.len()] == b':'
        {
            return Some(line[name.len() + 1..].trim_ascii());
        }
    }
    None
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

// ---------------- WebSocket 帧 ----------------

pub const OP_TEXT: u8 = 0x1;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
#[allow(dead_code)]
pub const OP_PONG: u8 = 0xA;

async fn read_all(s: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(), ()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = read_some(s, &mut buf[done..]).await?;
        done += n;
    }
    Ok(())
}

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

pub fn hex_val(c: u8) -> Option<u8> {
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
pub async fn serve_once(socket: &mut TcpSocket<'_>) -> Served {
    let mut buf = [0u8; 2048];
    let n = match read_headers(socket, &mut buf).await {
        Ok(n) => n,
        Err(_) => return Served::Page,
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
        return Served::Page;
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
        return Served::Page;
    }

    // ---- 状态查询（配网页轮询用）----
    if find_seq(path, b"/status").is_some() {
        let _ = write_status(socket).await;
        return Served::Page;
    }

    // ---- 诊断日志（USB 唤醒排查用）----
    if find_seq(path, b"/log").is_some() {
        let _ = write_log(socket).await;
        return Served::Page;
    }

    // ---- 电源控制：唤醒 / 睡眠（热点侧、局域网侧共用）----
    // 这是「不开 WebSocket 也能按」的快捷入口，给 curl / 脚本用；
    // 控制页顶栏那两个按钮走 WS，省一次握手（现在会话槽有 4 个，走 HTTP 也接得上）。
    if find_seq(path, b"/wake").is_some() {
        log::info!("[web] 收到唤醒请求");
        // 先把响应发回去，再去拉总线：唤醒后如果主机重新枚举，这条连接会断，
        // 没发完的话手机上会看到"发送失败"，体验上像没成功。
        let _ = write_html(socket, WAKE_SENT).await;
        crate::request_wake().await;
        return Served::Page;
    }
    if find_seq(path, b"/sleep").is_some() {
        log::info!("[web] 收到睡眠请求");
        let _ = write_html(socket, SLEEP_SENT).await;
        crate::request_sleep().await;
        return Served::Page;
    }

    // ---- 清空蓝牙配对（不动 WiFi / Blinker）----
    //
    // 必须重启：trouble 的 bond 在内存里还有一份，光擦 flash 不重启的话，
    // 当前这条连接还能继续用，只有重新上电才会真的"忘掉"。
    if find_seq(path, b"/clear-bond").is_some() {
        log::warn!("[web] 收到清空蓝牙配对请求");
        let ok = crate::clear_bond_flash().await;
        let _ = write_html(
            socket,
            if ok {
                BOND_CLEARED
            } else {
                BOND_CLEAR_FAILED
            },
        )
        .await;
        // 先把页面发出去，再重启
        embassy_time::Timer::after(embassy_time::Duration::from_millis(600)).await;
        esp_hal::system::software_reset();
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

    // ---- 键鼠控制页（不在主页：主页留给配置）----
    if find_seq(path, b"/kvm").is_some() {
        let _ = write_html(socket, PAGE).await;
        return Served::Page;
    }

    // ---- WebSocket 升级（键鼠 + 电源指令都走这条长连接）----
    let is_ws = find_seq(path, b"/ws").is_some()
        && header_value(req, b"Upgrade").map_or(false, |v| v.eq_ignore_ascii_case(b"websocket"));

    if !is_ws {
        // 其它一切路径（`/`、`/setup`、手机的俘虏门户探测）都返回**设置页**。
        // 手机连上没有外网的热点时会自己弹"登录到此网络"，落到配置页正是想要的。
        let _ = write_html(socket, SETUP_PAGE).await;
        return Served::Page;
    }

    let key = match header_value(req, b"Sec-WebSocket-Key") {
        Some(k) => k,
        None => return Served::Page,
    };

    let mut accept = [0u8; 28];
    ws_accept(key, &mut accept);

    let mut resp = [0u8; 200];
    let body = build_upgrade(&accept, &mut resp);
    if socket.write_all(body).await.is_err() {
        return Served::Page;
    }
    let _ = socket.flush().await;
    Served::WebSocket
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

const BOND_CLEARED: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>蓝牙配对已清空</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#3ad06a'>蓝牙配对已清空，正在重启…</h2>\
<p>WiFi 和 Blinker 配置都还在，只擦掉了配对信息。</p>\
<p style='color:#f0b040'><b>手机上还要做一步</b>：手机里仍然记着这台设备，\
必须先「忘记 / 取消配对」<b>ESP32-POWER-BLE</b>，否则重连会一直 Authentication Failure\
（板子已经不认这个旧密钥了）。</p>\
<p style='color:#889099'>约 8 秒后打开 <a href='/' style='color:#40c8e0'>设置页</a> 重新搜索配对即可。</p>";

const BOND_CLEAR_FAILED: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>清空失败</title><body style='font:16px system-ui;background:#0c1016;color:#e8eef7;padding:24px'>\
<h2 style='color:#e05a5a'>清空蓝牙配对失败</h2>\
<p>flash 擦除出错了，配对信息还在。可以试试长按 BOOT 3 秒恢复出厂（会连 WiFi 一起清掉）。</p>";

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
        let _ = core::fmt::Write::write_fmt(&mut body, format_args!("\""));
    }
    // 有没有蓝牙配对：设置页显示状态，也用来提示"要不要先清空再配对新设备"
    let _ = core::fmt::Write::write_fmt(
        &mut body,
        format_args!(
            ",\"bond\":{}}}",
            crate::BOND_PRESENT.load(core::sync::atomic::Ordering::Relaxed)
        ),
    );
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
