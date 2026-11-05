//! 极简 Web 设置界面：一个 TCP:80，手写 HTTP/1.1，不引框架。

use core::fmt::Write as FmtWrite;

use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::Duration;
use embedded_io_async::{Read, Write};
use heapless::String;

use crate::{config, state};
use crate::{GW_IP, INNER_MTU, PUBLIC_DNS1, PUBLIC_DNS2};

type Page = String<8192>;

static mut PAGE: Page = String::new();

#[embassy_executor::task]
pub async fn web_task(stack: Stack<'static>) {
    let rx = mk_buf();
    let tx = mk_buf2();
    loop {
        let mut sock = TcpSocket::new(stack, rx, tx);
        sock.set_timeout(Some(Duration::from_secs(15)));
        if sock.accept(80).await.is_ok() {
            let _ = serve(&mut sock).await;
        }
        sock.abort();
    }
}

fn mk_buf() -> &'static mut [u8] {
    static CELL: static_cell::StaticCell<[u8; 2048]> = static_cell::StaticCell::new();
    CELL.init([0; 2048])
}

fn mk_buf2() -> &'static mut [u8] {
    static CELL: static_cell::StaticCell<[u8; 4096]> = static_cell::StaticCell::new();
    CELL.init([0; 4096])
}

async fn serve(sock: &mut TcpSocket<'_>) -> Result<(), ()> {
    let mut head = [0u8; 1024];
    let mut n = 0usize;
    // 读到空行为止（只看请求行和头，body 单独按 Content-Length 读）
    loop {
        if n >= head.len() {
            return Err(());
        }
        let r = sock.read(&mut head[n..]).await.map_err(|_| ())?;
        if r == 0 {
            return Err(());
        }
        n += r;
        if find_head_end(&head[..n]).is_some() {
            break;
        }
    }
    let head_end = find_head_end(&head[..n]).ok_or(())?;

    let req = core::str::from_utf8(&head[..head_end]).map_err(|_| ())?;
    let mut lines = req.lines();
    let first = lines.next().ok_or(())?;
    let mut it = first.split_whitespace();
    let method = it.next().ok_or(())?;
    let path = it.next().ok_or(())?;

    let mut content_len = 0usize;
    for line in lines {
        let low = line.to_ascii_lowercase();
        if let Some(v) = low.strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }

    if method == "POST" {
        // body 可能已经跟着读进来一部分
        let mut body = String::<512>::new();
        let already = n - head_end;
        let mut take = already.min(content_len).min(512);
        for &b in &head[head_end..head_end + take] {
            let _ = body.push(b as char);
        }
        while take < content_len && body.len() < 512 {
            let mut tmp = [0u8; 128];
            let want = (content_len - take).min(tmp.len());
            let r = sock.read(&mut tmp[..want]).await.map_err(|_| ())?;
            if r == 0 {
                break;
            }
            for &b in &tmp[..r] {
                let _ = body.push(b as char);
            }
            take += r;
        }
        apply(&body);
        redirect(sock, path).await?;
        return Ok(());
    }

    if path.starts_with("/status.json") {
        let body = status_json();
        write_resp(sock, "200 OK", "application/json", body.as_bytes()).await?;
        return Ok(());
    }

    if path == "/" || path.starts_with("/?") {
        let body = render_index();
        write_resp(sock, "200 OK", "text/html; charset=utf-8", body.as_bytes()).await?;
        return Ok(());
    }

    write_resp(sock, "404 Not Found", "text/plain", b"not found").await?;
    Ok(())
}

fn find_head_end(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 3 < b.len() {
        if &b[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
        i += 1;
    }
    None
}

async fn write_resp(
    sock: &mut TcpSocket<'_>,
    status: &str,
    ctype: &str,
    body: &[u8],
) -> Result<(), ()> {
    let mut h = String::<256>::new();
    let _ = write!(
        h,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        ctype,
        body.len()
    );
    sock.write_all(h.as_bytes()).await.map_err(|_| ())?;
    sock.write_all(body).await.map_err(|_| ())?;
    sock.flush().await.map_err(|_| ())?;
    Ok(())
}

async fn redirect(sock: &mut TcpSocket<'_>, path: &str) -> Result<(), ()> {
    let _ = path;
    let h = "HTTP/1.1 303 See Other\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    sock.write_all(h.as_bytes()).await.map_err(|_| ())?;
    sock.flush().await.map_err(|_| ())?;
    Ok(())
}

// ---------------------------------------------------------------- 表单处理

fn apply(body: &str) {
    let mut ssid = None::<String<64>>;
    let mut pass = None::<String<64>>;
    let mut ip = None::<[u8; 4]>;
    let mut port = None::<u16>;
    // checkbox 未勾选时浏览器根本不提交这个字段，所以默认必须是 false
    let mut on = false;
    let mut up_ssid = None::<String<64>>;
    let mut up_pass = None::<String<64>>;
    let mut upon = None::<bool>;
    let mut up_target = None::<[u8; 4]>;
    let mut route = None::<u8>;
    let mut ch = None::<u8>;
    // checkbox 未勾选时浏览器不提交，所以默认 false = HT20
    let mut wide = false;

    for kv in body.split('&') {
        let mut p = kv.splitn(2, '=');
        let k = p.next().unwrap_or("");
        let v = p.next().unwrap_or("");
        match k {
            "ssid" => ssid = decode(v),
            "pass" => pass = decode(v),
            "ip" => ip = parse_ip(&urldecode(v)),
            "port" => port = urldecode(v).parse().ok(),
            "on" => on = v == "on" || v == "1",
            "upssid" => up_ssid = decode(v),
            "uppass" => up_pass = decode(v),
            "upon" => upon = Some(v == "on" || v == "1"),
            "uptarget" => {
                let t = urldecode(v);
                if t.is_empty() {
                    // 清空 = 回退到 AP 目标
                    up_target = Some([0, 0, 0, 0]);
                } else {
                    up_target = parse_ip(&t);
                }
            }
            "route" => route = urldecode(v).parse().ok(),
            "ch" => ch = urldecode(v).parse().ok(),
            "wide" => wide = v == "on" || v == "1",
            _ => {}
        }
    }

    if let (Some(s), Some(p)) = (ssid, pass) {
        if !s.is_empty() {
            state::set_net(&s, &p);
        }
    }
    if let Some(ip) = ip {
        state::set_fou_ip(core::net::Ipv4Addr::from(ip));
    }
    if let Some(p) = port {
        if p >= 1024 {
            state::set_fou_port(p);
        }
    }
    if let Some(t) = up_target {
        state::set_up_target(core::net::Ipv4Addr::from(t));
    }
    if let Some(r) = route {
        state::set_route_mode(r);
    }
    state::set_enabled(on);

    // 空口参数：保存后直接调驱动，不用重启（双模时信道改动会被驱动忽略）
    if let Some(c) = ch {
        if state::set_ap_channel(c) {
            state::set_wide_band(wide);
            crate::apply_rf(c, wide);
        }
    }

    // 上行配置：SSID 留空 = 关掉上行
    if let Some(us) = up_ssid {
        let up_p = up_pass.unwrap_or_default();
        let want = upon.unwrap_or(false) && !us.is_empty();
        // 密码非空却太短会被 set_uplink 拒掉，此时保留原配置
        let _ = state::set_uplink(us.as_str(), up_p.as_str(), want);
    }

    // 所有 setter 都跑完了才通知落盘；config_task 会合并短时间内的多次改动
    config::SAVE.signal(());
}

fn decode(s: &str) -> Option<String<64>> {
    Some(urldecode(s))
}

fn urldecode(s: &str) -> String<64> {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = match b[i] {
            b'+' => {
                i += 1;
                ' '
            }
            b'%' if i + 2 < b.len() => {
                let h = hex(b[i + 1]);
                let l = hex(b[i + 2]);
                i += 3;
                match (h, l) {
                    (Some(h), Some(l)) => (h * 16 + l) as char,
                    _ => '?',
                }
            }
            c => {
                i += 1;
                c as char
            }
        };
        let _ = out.push(c);
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n >= 4 {
            return None;
        }
        out[n] = part.parse().ok()?;
        n += 1;
    }
    if n == 4 {
        Some(out)
    } else {
        None
    }
}

// ---------------------------------------------------------------- 页面

fn render_index() -> &'static str {
    let (ssid, pass) = state::net_copy();
    let s = state::snapshot();
    let p: &mut Page = unsafe { &mut *core::ptr::addr_of_mut!(PAGE) };
    p.clear();

    let _ = write!(
        p,
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>PocketGW · ESP32 FOU 网关</title><style>
:root{{color-scheme:dark}}
*{{box-sizing:border-box}}
body{{margin:0;padding:20px;background:#0d1117;color:#c9d1d9;
font:15px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif}}
h1{{font-size:19px;margin:0 0 4px;color:#f0f6fc}}
h1 span{{font-size:12px;color:#6e7681;font-weight:400;margin-left:8px}}
.wrap{{max-width:520px;margin:0 auto}}
.card{{background:#161b22;border:1px solid #30363d;border-radius:12px;padding:16px;margin:14px 0}}
.row{{display:flex;justify-content:space-between;gap:12px;padding:5px 0;font-size:14px}}
.row b{{color:#f0f6fc;font-weight:600}}
.ok{{color:#3fb950}}.bad{{color:#f85149}}.dim{{color:#8b949e}}
label{{display:block;margin:12px 0 0;font-size:13px;color:#8b949e}}
input[type=text],input[type=password],input[type=number],select{{width:100%;margin-top:6px;padding:10px 12px;
background:#0d1117;border:1px solid #30363d;border-radius:8px;color:#f0f6fc;font-size:15px}}
input:focus{{outline:none;border-color:#238636}}
.chk{{display:flex;align-items:center;gap:10px;margin-top:16px;color:#c9d1d9;font-size:14px}}
.chk input{{width:18px;height:18px;accent-color:#238636}}
button{{width:100%;margin-top:18px;padding:12px;border:0;border-radius:8px;background:#238636;
color:#fff;font-size:15px;font-weight:600}}
button:active{{background:#2ea043}}
code{{background:#0d1117;padding:1px 5px;border-radius:4px;font-size:13px;color:#79c0ff}}
.foot{{text-align:center;color:#484f58;font-size:12px;margin:18px 0 0}}
.ap{{background:linear-gradient(180deg,#12261a,#161b22);border-color:#238636}}
.kv{{display:flex;justify-content:space-between;align-items:center;gap:12px;padding:8px 0;
border-bottom:1px solid #21262d}}
.kv:last-child{{border-bottom:0}}
.kv b{{font-size:17px;color:#7ee787;letter-spacing:.5px;word-break:break-all;text-align:right}}
.dim{{color:#8b949e;font-size:13px}}
</style></head><body><div class="wrap">
<h1>PocketGW<span>ESP32-S3 · FOU 网关</span></h1>
"#
    );

    // 连接信息卡片：连热点的人第一眼要看的就是账号密码
    let _ = write!(
        p,
        r#"<div class="card ap">
<div class="dim">Wi-Fi 连接信息</div>
<div class="kv"><span>名称</span><b>{}</b></div>
<div class="kv"><span>密码</span><b>{}</b></div>
<div class="kv"><span>设置页</span><b>http://{}</b></div>
</div>"#,
        ssid.as_str(),
        pass.as_str(),
        GW_IP
    );

    // 上行（STA）状态
    {
        let (up_ssid, _, up_on) = state::uplink_copy();
        let (txt, cls) = match s.up_state {
            2 => ("已连接", "ok"),
            1 => ("连接中", ""),
            3 => ("连接失败", "bad"),
            _ => ("未启用", ""),
        };
        let _ = write!(
            p,
            r#"<div class="card"><div class="row"><span>上行 Wi-Fi</span><b class="{}">{}</b></div>"#,
            cls, txt
        );
        if up_on {
            let _ = write!(
                p,
                r#"<div class="row"><span>上行网络</span><b>{}</b></div>"#,
                up_ssid.as_str()
            );
            let _ = write!(
                p,
                r#"<div class="row"><span>上行地址</span><b>{}</b></div>"#,
                if s.up_ip.is_unspecified() { "等待 DHCP".into() } else { alloc::format!("{}", s.up_ip) }
            );
            if s.up_ch > 0 {
                let _ = write!(
                    p,
                    r#"<div class="row"><span>信道（AP 跟随）</span><b>{}</b></div>"#,
                    s.up_ch
                );
            }
            let _ = write!(
                p,
                r#"<div class="row"><span>选路</span><b>{}</b></div>"#,
                match s.route_mode {
                    1 => "只用本热点",
                    2 => "只用上行 Wi-Fi",
                    _ => "自动（谁通用谁）",
                }
            );
        }
        let _ = write!(p, "</div>");
    }

    // 状态卡片
    let up = s.up;
    let _ = write!(
        p,
        r#"<div class="card"><div class="row"><span>转发</span><b class="{}">{}</b></div>"#,
        if up { "ok" } else { "bad" },
        if up { "运行中" } else { "已关闭" }
    );
    {
        let mut pt = String::<64>::new();
        let cls = if s.path == 0 { "bad" } else { "ok" };
        match s.path {
            1 => {
                let _ = write!(pt, "本热点 · {}", s.client_ip);
            }
            2 => {
                let _ = write!(pt, "上行 Wi-Fi · {}", state::tunnel_target());
            }
            _ => {
                let _ = pt.push_str("未就绪");
            }
        }
        let _ = write!(
            p,
            r#"<div class="row"><span>当前出口</span><b class="{}">{}</b></div>"#,
            cls,
            pt.as_str()
        );
    }
    let _ = write!(
        p,
        r#"<div class="row"><span>AP 侧手机</span><b>{}:{}</b></div>"#,
        s.client_ip,
        state::fou_port()
    );
    let _ = write!(
        p,
        r#"<div class="row"><span>上行侧手机</span><b>{}</b></div>"#,
        if s.up_target.is_unspecified() {
            alloc::string::String::from("未填写")
        } else {
            alloc::format!("{}:{}", s.up_target, state::fou_port())
        }
    );
    let _ = write!(
        p,
        r#"<div class="row"><span>下发 DNS</span><b>{} / {}</b></div>"#,
        PUBLIC_DNS1, PUBLIC_DNS2
    );
    let _ = write!(
        p,
        r#"<div class="row"><span>手机 MAC</span><b>{}</b></div>"#,
        match s.client_mac {
            Some(m) => FmtMac(m),
            None => FmtMac([0; 6]),
        }
    );
    let _ = write!(
        p,
        r#"<div class="row"><span>已转发 / 已回收</span><b>{} / {}</b></div>"#,
        s.fwd_out, s.fwd_in
    );
    let _ = write!(
        p,
        r#"<div class="row"><span>丢弃 / 未知 MAC</span><b>{} / {}</b></div>"#,
        s.dropped, s.no_mac
    );
    if s.loopback > 0 {
        let _ = write!(
            p,
            r#"<div class="row"><span>环路拦截</span><b class="bad">{}</b></div>"#,
            s.loopback
        );
    }
    if let Some((src, dst)) = s.flow {
        let _ = write!(
            p,
            r#"<div class="row"><span>最近一条</span><b>{} → {}</b></div>"#,
            src, dst
        );
    }
    let _ = write!(p, "</div>");

    if s.client_mac.is_none() && !s.up_on {
        let _ = write!(
            p,
            r#"<div class="card"><span class="bad">还没找到手机</span><span class="dim"> · 两条路都不通：让手机连上本热点（ARP 里能看到它），或者启用上行 Wi-Fi 并填上手机在上游 LAN 的 IP</span></div>"#
        );
    }
    if s.no_mac > 0 && s.fwd_out == 0 && s.client_mac.is_none() && !s.up_on {
        let _ = write!(
            p,
            r#"<div class="card"><span class="bad">检测到无效目标</span><span class="dim"> · 目标 IP 不在本网段，或手机没连上这个热点</span></div>"#
        );
    }

    // 表单
    let (up_ssid_val, up_pass_val, up_on_ui) = state::uplink_copy();
    let mut ch_opts = String::<160>::new();
    for c in [1u8, 6, 11] {
        let _ = write!(
            ch_opts,
            r#"<option value="{}" {}>{}</option>"#,
            c,
            if state::ap_channel() == c { "selected" } else { "" },
            c
        );
    }
    let mut route_opts = String::<160>::new();
    for (val, label) in [
        (0u8, "自动（谁通用谁）"),
        (1, "只用本热点里的手机"),
        (2, "只用上行 Wi-Fi"),
    ] {
        let _ = write!(
            route_opts,
            r#"<option value="{}" {}>{}</option>"#,
            val,
            if s.route_mode == val { "selected" } else { "" },
            label
        );
    }
    let _ = write!(
        p,
        r#"<form class="card" method="post" action="/set">
<div style="font-size:13px;color:#8b949e">热点</div>
<label>Wi-Fi 名称<input type="text" name="ssid" value="{}" maxlength="31"></label>
<label>Wi-Fi 密码（至少 8 位）<input type="text" name="pass" value="{}" maxlength="63"></label>
<div style="font-size:13px;color:#8b949e;margin-top:18px">空口调优（保存后立即生效，建议 1/6/11 与 20/40MHz 各试一遍）</div>
<label>2.4G 信道<select name="ch">{}</select></label>
<label class="chk"><input type="checkbox" name="wide" {}> 40MHz（拥挤环境关掉用 20MHz 通常更快）</label>
<div style="font-size:13px;color:#8b949e;margin-top:18px">FOU 隧道</div>
<label>本热点里的手机 IP（AP 模式）<input type="text" name="ip" value="{}"></label>
<label>上行 Wi-Fi 里的手机 IP（上行模式，留空则沿用上面的）<input type="text" name="uptarget" value="{}" placeholder="例如 192.168.1.50"></label>
<label>端口<input type="number" name="port" value="{}" min="1024" max="65535"></label>
<label>出口策略<select name="route">{}</select></label>
<label class="chk"><input type="checkbox" name="on" {}> 启用转发</label>
<div style="font-size:13px;color:#8b949e;margin-top:18px">上行 Wi-Fi（连到路由器，隧道走上游网段）</div>
<label>上游 Wi-Fi 名称<input type="text" name="upssid" value="{}" maxlength="31" placeholder="留空则不启用"></label>
<label>上游 Wi-Fi 密码<input type="text" name="uppass" value="{}" maxlength="63"></label>
<label class="chk"><input type="checkbox" name="upon" {}> 启用上行</label>
<button>保存</button>
</form>
"#,
        ssid.as_str(),
        pass.as_str(),
        ch_opts,
        if state::wide_band() { "checked" } else { "" },
        s.client_ip,
        if s.up_target.is_unspecified() {
            alloc::string::String::new()
        } else {
            alloc::format!("{}", s.up_target)
        },
        state::fou_port(),
        route_opts,
        if up { "checked" } else { "" },
        up_ssid_val.as_str(),
        up_pass_val.as_str(),
        if up_on_ui { "checked" } else { "" }
    );

    let _ = write!(
        p,
        r#"<div class="card" style="font-size:13px;line-height:1.8">
<div class="dim">两种出口，可以同时配好，默认自动选</div>
<b>AP 模式</b>：手机连本热点（<code>{}</code> 起分配），封装帧直发，路径最短<br>
<b>上行模式</b>：ESP32 自己连上游路由器，手机也在那个 LAN 里，填上它的上游 IP<br>
自动策略：ARP 表里能看到「本热点里的手机」就用 AP，否则用上行；手机换网不用改配置<br>
<div class="dim" style="margin-top:10px">用法</div>
1. 手机连本热点，在 PocketGateway 里开网关，「FOU 端口」保持 5555 → AP 模式即可用<br>
2. 想让手机走上游 Wi-Fi：填上游 SSID / 密码，并在「上行 Wi-Fi 里的手机 IP」填它的上游地址<br>
3. 其它设备连本热点，网关指向 <code>{}</code>，MTU 建议 <code>{}</code><br>
4. DNS 由 DHCP 下发 <code>223.5.5.5 / 119.29.29.29</code>，不经过本机，查询直接走隧道<br>
5. 手机要能出网：安卓里给这个 Wi-Fi 打开「自动切换到移动数据」，否则手机会把隧道包又送回这里<br>
<div class="dim" style="margin-top:8px">注：<code>192.168.4.1</code> 是本机自己，只提供 Web 设置页和 DHCP，发往它的包不会进隧道。</div>
</div>"#,
        crate::POOL_START, GW_IP, INNER_MTU
    );

    let _ = write!(p, r#"<div class="foot">PocketGW · ESP32-S3</div></div></body></html>"#);

    p.as_str()
}

struct FmtMac([u8; 6]);

impl core::fmt::Display for FmtMac {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == [0; 6] {
            return f.write_str("未发现");
        }
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

fn status_json() -> &'static str {
    static mut J: String<512> = String::new();
    let s = state::snapshot();
    let j: &mut String<512> = unsafe { &mut *core::ptr::addr_of_mut!(J) };
    j.clear();
    let _ = write!(
        j,
        r#"{{"up":{},"fou_ip":"{}","fou_port":{},"fwd_out":{},"fwd_in":{},"dropped":{},"client_mac":"{}","path":{},"route":{},"up_target":"{}","up_ip":"{}"}}"#,
        s.up,
        s.client_ip,
        state::fou_port(),
        s.fwd_out,
        s.fwd_in,
        s.dropped,
        FmtMac(s.client_mac.unwrap_or([0; 6])),
        s.path,
        s.route_mode,
        s.up_target,
        s.up_ip
    );
    j.as_str()
}
