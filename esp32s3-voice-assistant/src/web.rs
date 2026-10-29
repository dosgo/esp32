//! 网页配置界面（AP 热点 192.168.4.1 + 局域网 IP 都能访问）
//!
//! # 路由
//!
//! | 方法 | 路径 | 作用 |
//! |------|------|------|
//! | GET  | `/` | 配置页（回显当前配置） |
//! | POST | `/save` | 保存表单 → 写 flash → 重启 |
//! | GET  | `/status` | 状态 JSON（给页面轮询用） |
//! | GET  | `/reset` | 恢复出厂（擦配置 + 重启） |
//!
//! # 几个设计取舍
//!
//! **API Key 不回显**：页面里 key 字段的 value 永远是空，placeholder 写
//! "已设置，留空则不修改"。这样别人拿到你的手机打开配置页也看不到 key。
//! 想清空只能点「恢复出厂」。
//!
//! **保存后重启**：WiFi 参数和热点名都是射频启动时用的，热改要处理
//! "客户端被踢下线"、"两条 stack 重建" 一堆边界情况。重启一次 10 秒，
//! 换来的是零状态残留 —— 而且用户改配置本来就不频繁。
//!
//! **统一用 Connection: close**：不搞 keep-alive，一次请求一条连接。
//! 配置页只有几个请求，开销可以忽略，代码简单得多。

use core::fmt::Write as _;

use embedded_io_async::Write as _;
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    mutex::Mutex,
};
use embassy_time::{Duration, Timer};
use esp_storage::FlashStorage;
use heapless::String;

use crate::store::{self, Config};

pub const HTTP_PORT: u16 = 80;

/// 全局配置。启动时从 flash 读进来，网页保存时更新。
pub static CFG: Mutex<CriticalSectionRawMutex, Config> = Mutex::new(Config::new());

/// flash 句柄。启动时塞进来，网页保存时取出来写。
pub static FLASH: Mutex<CriticalSectionRawMutex, Option<FlashStorage<'static>>> = Mutex::new(None);

/// 单次请求最大字节数（表单 + 头）
pub const MAX_REQ: usize = 1536;
/// 页面缓冲。
///
/// 整个配置页（HTML + CSS + JS）大约 4.3~4.6KB，之前设 4096 是**刚好会截断**的，
/// 表现为页面底部缺一截（保存按钮没了）。放大到 8KB 留足余量，
/// 这块缓冲从 PSRAM 分配，不占内部 RAM。
/// `render()` 末尾有截断检测，真超了会在串口报出来。
pub const PAGE_BUF: usize = 8192;

// ============================================================================
// 配置读写
// ============================================================================

/// 启动时调用：从 flash 装载配置到 `CFG`
pub async fn load_into_memory(flash: FlashStorage<'static>) {
    let mut f = flash;
    let cfg = store::load(&mut f).unwrap_or_default();
    *FLASH.lock().await = Some(f);
    *CFG.lock().await = cfg;
}

/// 取当前配置的副本
pub async fn snapshot() -> Config {
    CFG.lock().await.clone()
}

/// 保存配置到内存 + flash
async fn persist(new: Config) -> bool {
    let snapshot = new.clone();
    *CFG.lock().await = new;
    let mut guard = FLASH.lock().await;
    match guard.as_mut() {
        Some(f) => store::save(f, &snapshot),
        None => false,
    }
}

// ============================================================================
// HTTP 小工具
// ============================================================================

fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// "GET /save?x=1 HTTP/1.1" → "/save?x=1"
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

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// %XX 和 '+' 解码。中文 SSID 会被浏览器编成 UTF-8 百分号串。
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

/// 从 urlencoded 串里取某个 key 的值（已解码），写入 `dst`
fn form_get(form: &[u8], key: &str, dst: &mut [u8]) -> Option<usize> {
    let kb = key.as_bytes();
    let mut rest = form;
    loop {
        let amp = find_seq(rest, b"&").unwrap_or(rest.len());
        let part = &rest[..amp];
        if let Some(eq) = find_seq(part, b"=") {
            if &part[..eq] == kb {
                return Some(urldecode(&part[eq + 1..], dst));
            }
        }
        if amp >= rest.len() {
            return None;
        }
        rest = &rest[amp + 1..];
    }
}

/// 取表单字段为 heapless::String。**空值表示"不修改"**，由调用方决定是否保留旧值。
fn field<const N: usize>(form: &[u8], key: &str) -> String<N> {
    let mut tmp = [0u8; 256];
    let mut s = String::new();
    if let Some(n) = form_get(form, key, &mut tmp) {
        if let Ok(v) = core::str::from_utf8(&tmp[..n]) {
            let _ = s.push_str(v);
        }
    }
    s
}

/// 空则沿用旧值
fn keep_if_empty<const N: usize>(new: String<N>, old: &String<N>) -> String<N> {
    if new.is_empty() {
        old.clone()
    } else {
        new
    }
}

/// 解析 Content-Length
fn content_length(head: &[u8]) -> usize {
    let mut i = 0;
    while i < head.len() {
        let line_end = match find_seq(&head[i..], b"\r\n") {
            Some(e) => i + e,
            None => break,
        };
        let line = &head[i..line_end];
        // 大小写不敏感地找 "content-length:"
        if line.len() > 15 {
            let lower_ok = line[..15]
                .iter()
                .zip(b"content-length:".iter())
                .all(|(a, b)| a.to_ascii_lowercase() == *b);
            if lower_ok {
                let mut n = 0usize;
                for &b in &line[15..] {
                    if b.is_ascii_digit() {
                        n = n.saturating_mul(10).saturating_add((b - b'0') as usize);
                    } else if b != b' ' {
                        break;
                    }
                }
                return n;
            }
        }
        i = line_end + 2;
    }
    0
}

/// 读完整请求（header + body）
async fn read_request(sock: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, ()> {
    let mut total = 0usize;
    loop {
        let n = match sock.read(&mut buf[total..]).await {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => n,
        };
        total += n;
        if let Some(hdr_end) = find_seq(&buf[..total], b"\r\n\r\n") {
            let cl = content_length(&buf[..hdr_end + 2]);
            if total >= hdr_end + 4 + cl {
                return Ok(total);
            }
        }
        if total + 1 >= buf.len() {
            return Ok(total);
        }
    }
}

// ============================================================================
// 响应写出
// ============================================================================

async fn write_head(sock: &mut TcpSocket<'_>, ctype: &str, len: usize) -> Result<(), ()> {
    let mut h = heapless::String::<160>::new();
    let _ = write!(
        h,
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n"
    );
    sock.write_all(h.as_bytes()).await.map_err(|_| ())
}

async fn write_page(sock: &mut TcpSocket<'_>, body: &[u8]) -> Result<(), ()> {
    write_head(sock, "text/html; charset=utf-8", body.len()).await?;
    sock.write_all(body).await.map_err(|_| ())
}

/// 简单的整页响应（提示页）
async fn write_simple(sock: &mut TcpSocket<'_>, title: &str, msg: &str, body: &mut [u8]) {
    let mut w = W::new(body);
    w.s("<html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>");
    w.s("<style>body{font-family:system-ui,-apple-system,'Segoe UI',sans-serif;background:#0f172a;color:#e2e8f0;margin:0;padding:40px 20px;text-align:center}h1{font-size:20px;font-weight:500;margin:0 0 12px}p{color:#94a3b8;font-size:14px;line-height:1.7}a{color:#5eead4}</style>");
    w.s("</head><body><h1>");
    w.s(title);
    w.s("</h1><p>");
    w.s(msg);
    w.s("</p><p><a href='/'>返回配置页</a></p></body></html>");
    let n = w.pos;
    let _ = write_page(sock, &body[..n]).await;
}

// ============================================================================
// 页面拼装器
// ============================================================================

struct W<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> W<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// 原样写（源码里的字面量，不含用户数据）
    fn s(&mut self, t: &str) {
        let b = t.as_bytes();
        let n = b.len().min(self.buf.len() - self.pos);
        self.buf[self.pos..self.pos + n].copy_from_slice(&b[..n]);
        self.pos += n;
    }

    /// 写用户数据，做 HTML 转义（WiFi 名可能带 & < "）
    fn esc(&mut self, t: &str) {
        for c in t.chars() {
            match c {
                '&' => self.s("&amp;"),
                '<' => self.s("&lt;"),
                '>' => self.s("&gt;"),
                '"' => self.s("&quot;"),
                '\'' => self.s("&#39;"),
                _ => {
                    let mut tmp = [0u8; 4];
                    self.s(c.encode_utf8(&mut tmp));
                }
            }
        }
    }

    fn num(&mut self, v: u16) {
        let mut tmp = heapless::String::<6>::new();
        let _ = write!(tmp, "{v}");
        self.s(tmp.as_str());
    }

    /// 输出一个 select 的 option，命中当前值就带 selected
    fn opt(&mut self, value: &str, label: &str, cur: &str) {
        self.s("<option value=\"");
        self.esc(value);
        self.s("\"");
        if cur == value {
            self.s(" selected");
        }
        self.s(">");
        self.s(label);
        self.s("</option>");
    }
}

/// 渲染配置页
fn render(cfg: &Config, buf: &mut [u8]) -> usize {
    let mut w = W::new(buf);

    w.s("<!DOCTYPE html><html lang=zh-CN><head><meta charset=utf-8>");
    w.s("<meta name=viewport content='width=device-width,initial-scale=1'>");
    w.s("<title>语音助手配置</title><style>");
    w.s("*{box-sizing:border-box}");
    w.s("body{font-family:system-ui,-apple-system,'Segoe UI',sans-serif;background:#0f172a;color:#e2e8f0;margin:0;padding:20px 14px 40px}");
    w.s(".top{display:flex;align-items:center;justify-content:space-between;margin-bottom:6px}");
    w.s("h1{font-size:19px;font-weight:500;margin:0}");
    w.s(".temp{font-size:14px;font-weight:500;color:#5eead4;font-variant-numeric:tabular-nums;padding:3px 9px;background:#1e293b;border-radius:999px}");
    w.s(".sub{color:#64748b;font-size:12px;margin:0 0 20px;line-height:1.6}");
    w.s("section{background:#1e293b;border-radius:12px;padding:14px 14px 4px;margin-bottom:12px}");
    w.s("h2{font-size:13px;font-weight:500;color:#5eead4;margin:0 0 12px;letter-spacing:.5px}");
    w.s("label{display:block;margin-bottom:12px;font-size:12px;color:#94a3b8}");
    w.s("input,select{display:block;width:100%;margin-top:5px;padding:9px 10px;font-size:14px;color:#e2e8f0;background:#0f172a;border:1px solid #334155;border-radius:8px;font-family:inherit}");
    w.s("input:focus,select:focus{outline:none;border-color:#2dd4bf}");
    w.s("input::placeholder{color:#475569}");
    w.s("button{width:100%;padding:14px;font-size:15px;font-weight:500;color:#042f2e;background:#2dd4bf;border:0;border-radius:10px;margin-top:6px;font-family:inherit}");
    w.s(".hint{color:#475569;font-size:11px;margin:8px 0 0;line-height:1.6}");
    w.s(".row{display:flex;gap:10px}.row>label{flex:1}");
    w.s("</style></head><body>");

    w.s("<div class=top><h1>语音助手</h1><span id=temp class=temp>--℃</span></div>");
    w.s("<p class=sub>设备热点 <b>");
    w.esc(cfg.ap_ssid_or_default());
    w.s("</b> · 保存后设备重启，约 10 秒后生效</p>");

    w.s("<form method=post action=/save>");

    // ---- 家庭网络 ----
    w.s("<section><h2>家庭网络</h2>");
    w.s("<label>WiFi 名称（必须是 2.4G 频段）<input name=wifi_ssid value=\"");
    w.esc(cfg.wifi_ssid.as_str());
    w.s("\" placeholder=\"家里 WiFi 的名字\"></label>");
    w.s("<label>WiFi 密码<input name=wifi_pass type=password value=\"");
    w.esc(cfg.wifi_pass.as_str());
    w.s("\" placeholder=\"留空表示开放网络\"></label>");
    w.s("</section>");

    // ---- 网关 ----
    w.s("<section><h2>语音网关</h2><div class=row>");
    w.s("<label>地址（跑 gateway.py 的电脑）<input name=gw_host value=\"");
    w.esc(cfg.gw_host.as_str());
    w.s("\" placeholder=\"192.168.1.100\"></label>");
    w.s("<label style='max-width:96px'>端口<input name=gw_port value=\"");
    w.num(cfg.gw_port);
    w.s("\"></label></div>");
    w.s("<p class=hint>电脑上运行 server/gateway.py，ipconfig 看它的局域网 IP</p>");
    w.s("</section>");

    // ---- 大模型 ----
    w.s("<section><h2>大模型</h2>");
    w.s("<label>服务商<select name=llm_backend>");
    w.opt("deepseek", "DeepSeek（便宜，推荐）", cfg.llm_backend_or_default());
    w.opt("siliconflow", "硅基流动", cfg.llm_backend_or_default());
    w.s("</select></label>");
    w.s("<label>API Key<input name=llm_key type=password value=\"\" placeholder=\"");
    if cfg.llm_key.is_empty() {
        w.s("未设置，去 platform.deepseek.com 申请");
    } else {
        w.s("已设置（留空则不修改）");
    }
    w.s("\"></label>");
    w.s("<label>模型<input name=llm_model value=\"");
    w.esc(cfg.llm_model_or_default());
    w.s("\"></label>");
    w.s("<label>接口地址<input name=llm_base value=\"");
    w.esc(cfg.llm_base_or_default());
    w.s("\"></label>");
    w.s("</section>");

    // ---- ASR ----
    w.s("<section><h2>语音识别 ASR</h2>");
    w.s("<label>优先级<select name=asr_chain>");
    w.opt(store::ASR_CHAIN_DEFAULT, "自动（本地优先，失败转云端）", cfg.asr_chain_or_default());
    w.opt("siliconflow", "只用云端（硅基流动）", cfg.asr_chain_or_default());
    w.opt("local,local-lite", "只用本地（离线免费）", cfg.asr_chain_or_default());
    w.s("</select></label>");
    w.s("<label>API Key<input name=asr_key type=password value=\"\" placeholder=\"");
    if cfg.asr_key.is_empty() {
        w.s("未设置（仅云端需要）");
    } else {
        w.s("已设置（留空则不修改）");
    }
    w.s("\"></label>");
    w.s("<label>模型<input name=asr_model value=\"");
    w.esc(cfg.asr_model_or_default());
    w.s("\"></label>");
    w.s("</section>");

    // ---- TTS ----
    w.s("<section><h2>语音合成 TTS</h2>");
    w.s("<label>优先级<select name=tts_chain>");
    w.opt(store::TTS_CHAIN_DEFAULT, "云端优先（音质好）", cfg.tts_chain_or_default());
    w.opt("piper", "只用本地 Piper（免费）", cfg.tts_chain_or_default());
    w.opt("edge", "Edge TTS（免费）", cfg.tts_chain_or_default());
    w.s("</select></label>");
    w.s("<label>音色<input name=tts_voice value=\"");
    w.esc(cfg.tts_voice_or_default());
    w.s("\"></label>");
    w.s("<p class=hint>ASR 和 TTS 的 Key 都填硅基流动的同一个 key</p>");
    w.s("</section>");

    // ---- 热点 ----
    w.s("<section><h2>设备热点</h2>");
    w.s("<label>热点名称<input name=ap_ssid value=\"");
    w.esc(cfg.ap_ssid.as_str());
    w.s("\" placeholder=\"");
    w.s(store::DEFAULT_AP_SSID);
    w.s("\"></label>");
    w.s("<label>热点密码（8~63 位，留空用默认）<input name=ap_pass type=password value=\"");
    w.esc(cfg.ap_pass.as_str());
    w.s("\" placeholder=\"");
    w.s(store::DEFAULT_AP_PASS);
    w.s("\"></label>");
    w.s("</section>");

    w.s("<button type=submit>保存并重启</button>");
    w.s("<p class=hint style='text-align:center'><a href=/reset style='color:#64748b'>恢复出厂设置</a></p>");
    w.s("</form>");

    // 温度轮询。放 5 秒一次，配置页开着的时候也不会有多少流量。
    // 注意：这是**芯片内部温度**，比环境温度高 —— Wi-Fi 满载 + 音频跑起来
    // 50~60℃ 很正常，70℃ 以上才值得注意。
    w.s("<script>");
    w.s("function upTemp(){fetch('/status').then(function(r){return r.json()}).then(function(d){");
    w.s("var e=document.getElementById('temp');if(!e)return;");
    w.s("if(d.temp===null||d.temp===undefined){e.textContent='--℃';e.style.color='#475569';return;}");
    w.s("e.textContent=d.temp+'℃';");
    w.s("e.style.color=d.temp>=70?'#fbbf24':'#5eead4';");
    w.s("}).catch(function(){})}");
    w.s("upTemp();setInterval(upTemp,5000);");
    w.s("</script>");

    w.s("</body></html>");

    // 截断检测：W::s 写不下时会静默截断，页面就会缺尾巴（最常见是保存按钮消失）。
    // 缓冲区满了就打一条日志，别让它悄悄坏掉。
    if w.pos >= w.buf.len() {
        log::warn!(
            "[web] 配置页被截断！缓冲 {} 字节已写满，请调大 PAGE_BUF",
            w.buf.len()
        );
    }

    w.pos
}

// ============================================================================
// 请求处理
// ============================================================================

/// 处理一条连接。返回 true 表示保存过配置、需要重启。
pub async fn serve_once(sock: &mut TcpSocket<'_>, req_buf: &mut [u8], page_buf: &mut [u8]) -> bool {
    let n = match read_request(sock, req_buf).await {
        Ok(n) => n,
        Err(_) => return false,
    };
    let req = &req_buf[..n];
    let path = request_path(req);
    let path_str = core::str::from_utf8(path).unwrap_or("/");

    // ---- 保存 ----
    if path_str.starts_with("/save") {
        // body 在 \r\n\r\n 之后；GET 方式的话参数在 query 里
        let body_start = find_seq(req, b"\r\n\r\n").map(|i| i + 4).unwrap_or(0);
        let form: &[u8] = if path_str.contains('?') {
            // GET /save?a=b：取 ? 之后
            let q = path_str.find('?').unwrap();
            &path[q + 1..]
        } else {
            &req[body_start..]
        };

        let cur = snapshot().await;

        // 端口解析
        let port_s = field::<8>(form, "gw_port");
        let gw_port = port_s
            .as_str()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .unwrap_or(cur.gw_port);

        let new = Config {
            wifi_ssid: field(form, "wifi_ssid"),
            wifi_pass: field(form, "wifi_pass"),
            gw_host: field(form, "gw_host"),
            gw_port,
            llm_backend: field(form, "llm_backend"),
            // key 留空 = 不改
            llm_key: keep_if_empty(field(form, "llm_key"), &cur.llm_key),
            llm_model: field(form, "llm_model"),
            llm_base: field(form, "llm_base"),
            asr_chain: field(form, "asr_chain"),
            asr_key: keep_if_empty(field(form, "asr_key"), &cur.asr_key),
            asr_model: field(form, "asr_model"),
            tts_chain: field(form, "tts_chain"),
            tts_voice: field(form, "tts_voice"),
            ap_ssid: field(form, "ap_ssid"),
            ap_pass: field(form, "ap_pass"),
        };

        let ok = persist(new).await;
        if ok {
            write_simple(
                sock,
                "已保存",
                "设备正在重启，约 10 秒后生效。之后请把手机连回原来的 WiFi。",
                page_buf,
            )
            .await;
        } else {
            write_simple(sock, "保存失败", "flash 写入出错，请重试。", page_buf).await;
        }
        return ok;
    }

    // ---- 恢复出厂 ----
    if path_str.starts_with("/reset") {
        {
            let mut guard = FLASH.lock().await;
            if let Some(f) = guard.as_mut() {
                store::erase(f);
            }
        }
        *CFG.lock().await = Config::new();
        write_simple(
            sock,
            "已恢复出厂",
            "所有配置已清除，设备重启后进入配网模式。",
            page_buf,
        )
        .await;
        return true;
    }

    // ---- 状态 JSON ----
    if path_str.starts_with("/status") {
        let cfg = snapshot().await;
        let mut s = heapless::String::<320>::new();
        let _ = write!(
            s,
            "{{\"wifi_set\":{},\"llm_set\":{},\"asr_key_set\":{},\"gw\":\"{}\",\"port\":{},\"temp\":",
            cfg.wifi_set(),
            !cfg.llm_key.is_empty(),
            !cfg.asr_key.is_empty(),
            cfg.gw_host_or_default(),
            cfg.gw_port
        );
        // 芯片温度，没读到过就是 null
        match crate::soc_temp::last() {
            Some(v) => {
                let _ = write!(s, "{v}");
            }
            None => {
                let _ = s.push_str("null");
            }
        }
        let _ = s.push('}');
        let _ = write_head(sock, "application/json", s.len()).await;
        let _ = sock.write_all(s.as_bytes()).await;
        return false;
    }

    // ---- 配置页 ----
    let cfg = snapshot().await;
    let len = render(&cfg, page_buf);
    let _ = write_page(sock, &page_buf[..len]).await;
    false
}

/// 网页任务。AP 侧和 STA 侧各跑一份。
///
/// **所有缓冲都从外面传进来**，函数体里一个字节的数组都不许有：
/// 这些数组会进 future，而 future 是存在任务栈上的 —— 之前 req(2KB) +
/// page(8KB) 直接放函数里，链接器立刻报 `Main stack is smaller than 8192 bytes`。
/// 而且这个函数会被调两次，函数内 static 只有一份，第二次写 StaticCell 会 panic。
// pool_size = 2：AP 侧和 STA 侧各起一份，两侧都能打开配置页
#[embassy_executor::task(pool_size = 2)]
pub async fn web_task(
    stack: Stack<'static>,
    tag: &'static str,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
    req: &'static mut [u8],
    page: &'static mut [u8],
) {
    let mut sock = TcpSocket::new(stack, rx, tx);

    log::info!("[web/{tag}] 监听 0.0.0.0:{}", HTTP_PORT);

    loop {
        // 空等连接可以很久，120 秒只是让日志偶尔刷一行
        sock.set_timeout(Some(Duration::from_secs(120)));
        match sock.accept(HTTP_PORT).await {
            Ok(_) => {
                // 打这行是为了区分两种故障：
                //   有这条 = TCP 三次握手成功，问题在 HTTP 处理
                //   没这条 = 包根本没到设备（ARP / 链路 / IP 配置问题）
                log::info!("[web/{tag}] 收到 TCP 连接");
            }
            Err(e) => {
                // accept 没有 Timeout 变体，只有 InvalidState / InvalidPort / ConnectionReset
                log::warn!("[web/{tag}] accept 出错: {e:?}");
                Timer::after(Duration::from_millis(300)).await;
                continue;
            }
        }
        // 收数据给 15 秒
        sock.set_timeout(Some(Duration::from_secs(15)));

        let need_restart = serve_once(&mut sock, req, page).await;

        // 让 FIN 真正发出去再往下走
        Timer::after(Duration::from_millis(300)).await;
        sock.close();
        Timer::after(Duration::from_millis(100)).await;

        if need_restart {
            log::info!("[web/{tag}] 配置已保存，重启生效");
            Timer::after(Duration::from_millis(600)).await;
            esp_hal::system::software_reset();
        }
    }
}
