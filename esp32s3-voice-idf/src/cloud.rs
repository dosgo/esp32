//! 直连云端（不经网关）：
//!   * ASR  : DashScope qwen-audio-3.1-asr-flash（base64 wav 放在请求体里）
//!   * LLM  : DeepSeek chat/completions
//!   * TTS  : DashScope qwen3-tts-flash -> 音频 URL -> wav 下载
//!
//! 连接保持活跃并复用：这块板上一次 TLS 握手要好几秒，
//! 每台主机只付一次而不是每个请求付一次，
//! 就是一轮对话 20 秒和 8 秒的差别。

use std::time::Duration;

use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;

pub const DASHSCOPE_BASE: &str = "https://ws-ebekoepr6y8ojg9x.cn-beijing.maas.aliyuncs.com";
/// 真正的秘钥放在 `src/config.rs`（已 gitignore，见 config.example.rs）。
/// 绝不要硬编码在这里：本文件是被 git 跟踪的。
pub const DASHSCOPE_KEY: &str = crate::config::DASHSCOPE_KEY;
pub const DEEPSEEK_URL: &str = "https://api.deepseek.com/chat/completions";
pub const DASHSCOPE_WS_URL: &str =
    "wss://ws-ebekoepr6y8ojg9x.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference";

#[derive(Clone, Copy)]
enum Host {
    Dash,
    Deep,
    /// 音频文件在 OSS 上：另一台主机，绝不与 Dash 混用连接。
    Oss,
}

fn client() -> Result<EspHttpConnection, String> {
    let cfg = Configuration {
        buffer_size: Some(4096),
        buffer_size_tx: Some(4096),
        timeout: Some(Duration::from_secs(10)),
        crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
        keep_alive_enable: true,
        keep_alive_idle: Some(30),
        keep_alive_interval: Some(5),
        keep_alive_count: Some(3),
        ..Default::default()
    };
    EspHttpConnection::new(&cfg).map_err(|e| format!("http client: {e:?}"))
}

pub struct Cloud {
    dash: Option<EspHttpConnection>,
    deep: Option<EspHttpConnection>,
    oss: Option<EspHttpConnection>,
}

impl Cloud {
    pub fn new() -> Self {
        Self {
            dash: None,
            deep: None,
            oss: None,
        }
    }
    fn slot(&mut self, host: Host) -> &mut Option<EspHttpConnection> {
        match host {
            Host::Dash => &mut self.dash,
            Host::Deep => &mut self.deep,
            Host::Oss => &mut self.oss,
        }
    }

    fn with_conn<F>(&mut self, host: Host, f: F) -> Result<String, String>
    where
        F: FnOnce(&mut EspHttpConnection) -> Result<String, String>,
    {
        if self.slot(host).is_none() {
            *self.slot(host) = Some(client()?);
        }
        let res = {
            let conn = self.slot(host).as_mut().unwrap();
            f(conn)
        };
        // 成功时保留 socket，让一轮对话能复用它（LLM 和 TTS 走同一台
        // 主机）。出错就丢弃，下次重连。
        if res.is_err() {
            *self.slot(host) = None;
        }

        res
    }

    fn post(&mut self, host: Host, url: &str, token: &str, body: &[u8]) -> Result<String, String> {
        // keep-alive 的 socket 可能在我们空闲时被服务端关掉；
        // 那就丢弃它并重连一次。
        for attempt in 1..=2 {
            let auth = format!("Bearer {token}");
            let t0 = std::time::Instant::now();
            let res = self.with_conn(host, |conn| {
                conn.initiate_request(
                    Method::Post,
                    url,
                    &[
                        ("Authorization", auth.as_str()),
                        ("Content-Type", "application/json"),
                    ],
                )
                .map_err(|e| format!("request: {e:?}"))?;
                println!("[cloud]   connect {:?}", t0.elapsed());
                conn.write_all(body).map_err(|e| format!("write: {e:?}"))?;
                println!("[cloud]   upload {} bytes {:?}", body.len(), t0.elapsed());
                conn.initiate_response()
                    .map_err(|e| format!("response: {e:?}"))?;
                println!("[cloud]   tls+headers {:?}", t0.elapsed());
                let r = read_response(conn, 6);
                println!("[cloud]   body {:?}", t0.elapsed());
                r
            });
            match res {
                Ok(v) => return Ok(v),
                Err(e) => {
                    println!("[cloud] post attempt {attempt} failed: {e}");
                    // 结构完整的 HTTP 错误（4xx/5xx）不是传输问题；
                    // 重试只会白白发一遍整个请求体。
                    let retriable = !e.starts_with("HTTP ");
                    if !retriable || attempt == 2 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!()
    }

    pub fn asr(&mut self, pcm: &[u8], sample_rate: u32) -> Result<String, String> {
        let mut wav = Vec::with_capacity(pcm.len() + 44);
        wav.extend_from_slice(&wav_header(pcm.len(), sample_rate));
        wav.extend_from_slice(pcm);
        let b64 = base64(&wav);
        let body = format!(
            "{{\"model\":\"qwen-audio-3.1-asr-flash\",\"input\":{{\"messages\":[{{\"role\":\"user\",\"content\":[{{\"type\":\"input_audio\",\"input_audio\":{{\"data\":\"data:audio/wav;base64,{b64}\"}}}}]}}]}},\"parameters\":{{\"format\":\"wav\",\"sample_rate\":\"{sample_rate}\"}}}}"
        );
        let url = format!("{DASHSCOPE_BASE}/api/v1/services/aigc/multimodal-generation/generation");
        let resp = self.post(Host::Dash, &url, DASHSCOPE_KEY, body.as_bytes())?;
        string_field(&resp, "text")
            .ok_or_else(|| format!("asr: no text in {}", truncate(&resp, 200)))
    }

    pub fn chat(&mut self, api_key: &str, system: &str, user: &str) -> Result<String, String> {
        let ds_body = format!(
            "{{\"model\":\"deepseek-chat\",\"messages\":[{{\"role\":\"system\",\"content\":\"{}\"}},{{\"role\":\"user\",\"content\":\"{}\"}}],\"stream\":false}}",
            json_escape(system),
            json_escape(user)
        );
        let resp = self.post(Host::Deep, DEEPSEEK_URL, api_key, ds_body.as_bytes())?;
        string_field(&resp, "content")
            .ok_or_else(|| format!("deepseek: no content in {}", truncate(&resp, 200)))
    }

    pub fn tts(&mut self, text: &str) -> Result<Vec<u8>, String> {
        let body = format!(
            "{{\"model\":\"qwen3-tts-flash\",\"input\":{{\"text\":\"{}\",\"voice\":\"Cherry\",\"format\":\"pcm\",\"sample_rate\":16000}}}}",
            json_escape(text)
        );
        let url = format!("{DASHSCOPE_BASE}/api/v1/services/aigc/multimodal-generation/generation");
        let resp = self.post(Host::Dash, &url, DASHSCOPE_KEY, body.as_bytes())?;
        let audio_url = string_field(&resp, "url")
            .ok_or_else(|| format!("tts: no url in {}", truncate(&resp, 200)))?;

        let mut wav = Vec::new();
        self.with_conn(Host::Oss, |conn| {
            conn.initiate_request(Method::Get, &audio_url, &[])
                .map_err(|e| format!("tts get: {e:?}"))?;
            conn.initiate_response()
                .map_err(|e| format!("tts get resp: {e:?}"))?;
            let mut buf = [0u8; 4096];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => wav.extend_from_slice(&buf[..n]),
                    Err(e) => return Err(format!("tts read: {e:?}")),
                }
            }
            Ok(String::new())
        })?;
        if wav.len() <= 44 {
            return Err("tts: empty audio".into());
        }
        Ok(wav[44..].to_vec())
    }

    /// 只预热 DNS（很快）。TLS 握手留到第一轮真实对话。
    pub fn dns_warmup(&self) {
        dns_prime();
        use std::net::ToSocketAddrs;
        match "ws-ebekoepr6y8ojg9x.cn-beijing.maas.aliyuncs.com:443".to_socket_addrs() {
            Ok(it) => {
                let ips: Vec<String> = it.take(3).map(|a| a.ip().to_string()).collect();
                println!("[cloud] dns ips: {:?}", ips);
            }
            Err(e) => println!("[cloud] dns resolve err: {e}"),
        }
    }

    /// 预热 DNS 和每台主机一次 TLS 握手，让第一轮对话不慢。
    pub fn warmup(&mut self) {
        dns_prime();
        let url = format!("{DASHSCOPE_BASE}/compatible-mode/v1/models");
        for attempt in 1..=4 {
            let r = self.with_conn(Host::Dash, |conn| {
                conn.initiate_request(Method::Get, &url, &[])
                    .map_err(|e| format!("{e:?}"))?;
                conn.initiate_response().map_err(|e| format!("{e:?}"))?;
                let status = conn.status().to_string();
                // keep-alive 连接上绝不能留着没读完的响应体。
                drain(conn, 64 * 1024);
                Ok(status)
            });
            match r {
                Ok(status) => {
                    println!("[cloud] dashscope warmup ok (attempt {attempt}, status {status})");
                    break;
                }
                Err(e) => println!("[cloud] dashscope warmup attempt {attempt} failed: {e}"),
            }
        }
        for attempt in 1..=4 {
            let r = self.with_conn(Host::Deep, |conn| {
                conn.initiate_request(Method::Get, "https://api.deepseek.com/", &[])
                    .map_err(|e| format!("{e:?}"))?;
                conn.initiate_response().map_err(|e| format!("{e:?}"))?;
                let status = conn.status().to_string();
                drain(conn, 64 * 1024);
                Ok(status)
            });
            match r {
                Ok(status) => {
                    println!("[cloud] deepseek warmup ok (attempt {attempt}, status {status})");
                    break;
                }
                Err(e) => println!("[cloud] deepseek warmup attempt {attempt} failed: {e}"),
            }
        }
    }
}

fn read_response(conn: &mut EspHttpConnection, _timeout_s: u64) -> Result<String, String> {
    let status = conn.status();
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2048];
    // 一路读到 EOF——不设硬超时：提前返回会把没读完的响应体
    // 留在 keep-alive 连接上，让下一个请求崩溃。
    loop {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > 128 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    if status != 200 {
        return Err(format!("HTTP {status}: {}", truncate(&text, 200)));
    }
    Ok(text)
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 丢弃所有未读完的响应体。keep-alive 连接上如果还残留响应体字节，
/// 下一次发起请求时 esp_http 客户端状态就会错乱，
/// 在 esp_http_client_read 内部崩溃（LoadProhibited，memcpy(NULL)）——
/// 所以每个请求结束时响应体必须完全排干。
fn drain(conn: &mut EspHttpConnection, max_bytes: usize) {
    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    while total < max_bytes {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
}

/// 预热解析器，避免开机后最初的几次 HTTPS 尝试失败。
/// 打印当前 AP 的 RSSI（诊断用：解释上传为什么慢）。
fn log_rssi() {
    unsafe {
        let mut ap: esp_idf_sys::wifi_ap_record_t = core::mem::zeroed();
        if esp_idf_sys::esp_wifi_sta_get_ap_info(&mut ap) == 0 {
            println!("[wifi] rssi={}dBm ch={}", ap.rssi, ap.primary);
        }
    }
}
pub fn dns_prime() {
    use std::net::ToSocketAddrs;
    let host = "ws-ebekoepr6y8ojg9x.cn-beijing.maas.aliyuncs.com:443";
    for attempt in 1..=10 {
        let ok = host
            .to_socket_addrs()
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if ok {
            println!("[cloud] dns ok (attempt {attempt})");
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // TTS 音频在 OSS 上；也要预热解析，否则第一次下载会卡在 DNS 几秒。
    for attempt in 1..=10 {
        let ok = "dashscope-result-bj.oss-cn-beijing.aliyuncs.com:80"
            .to_socket_addrs()
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if ok {
            println!("[cloud] oss dns ok (attempt {attempt})");
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("[cloud] dns still failing after 10 tries");
}

/// Base64（标准字母表，带填充）。
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 给 16 位单声道 PCM 套上 44 字节的 WAV 头。
pub fn wav_header(pcm_len: usize, sample_rate: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let byte_rate = sample_rate * 2;
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&((36 + pcm_len) as u32).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // 单声道
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // 块对齐
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // 位深
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&(pcm_len as u32).to_le_bytes());
    h
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// 提取 `"key":"..."` 的第一个字符串值（按字节安全处理 UTF-8）。
fn string_field(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = json.find(&pat)? + pat.len();
    let bytes = json.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                let n = bytes[i + 1];
                match n {
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'u' => {
                        if i + 5 < bytes.len() {
                            let hex = std::str::from_utf8(&bytes[i + 2..i + 6]).ok();
                            if let Some(code) = hex.and_then(|h| u32::from_str_radix(h, 16).ok()) {
                                if let Some(ch) = char::from_u32(code) {
                                    let mut buf = [0u8; 4];
                                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                }
                            }
                            i += 6;
                            continue;
                        }
                    }
                    other => out.push(other),
                }
                i += 2;
            }
            b'"' => return String::from_utf8(out).ok(),
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

/// 解码标准 base64（带填充）为字节。
pub fn base64_decode(s: &str) -> Vec<u8> {
    let mut rev = [255u8; 256];
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (i, b) in table.iter().enumerate() {
        rev[*b as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for b in s.bytes() {
        let v = rev[b as usize];
        if v == 255 {
            continue;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

impl Cloud {
    /// 流式 TTS：PCM 块一到达就喂给 `sink`（SSE）。
    pub fn tts_stream<F>(
        &mut self,
        text: &str,
        sample_rate: u32,
        sink: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&[u8]),
    {
        let body = format!(
            "{{\"model\":\"qwen3-tts-flash\",\"input\":{{\"text\":\"{}\",\"voice\":\"Cherry\",\"format\":\"pcm\",\"sample_rate\":{sample_rate}}}}}",
            json_escape(text)
        );
        let url = format!("{DASHSCOPE_BASE}/api/v1/services/aigc/multimodal-generation/generation");
        let auth = format!("Bearer {DASHSCOPE_KEY}");
        let mut line = Vec::new();
        let mut total = 0usize;
        let mut first = true;
        self.with_conn(Host::Dash, |conn| {
            conn.initiate_request(
                Method::Post,
                &url,
                &[
                    ("Authorization", auth.as_str()),
                    ("Content-Type", "application/json"),
                    ("X-DashScope-SSE", "enable"),
                ],
            )
            .map_err(|e| format!("tts req: {e:?}"))?;
            conn.write_all(body.as_bytes())
                .map_err(|e| format!("tts write: {e:?}"))?;
            conn.initiate_response()
                .map_err(|e| format!("tts resp: {e:?}"))?;
            let status = conn.status();
            if status != 200 {
                return Err(format!("tts HTTP {status}"));
            }
            let mut buf = [0u8; 1024];
            let mut done = false;
            loop {
                let n = match conn.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    // 音频已结束时 socket 可能正在关闭；
                    // 此时报错不代表流失败。
                    Err(e) => {
                        if done {
                            break;
                        }
                        return Err(format!("tts read: {e:?}"));
                    }
                };
                if done {
                    // 把剩余字节读到 EOF，保证 keep-alive 连接
                    // 干净地留给下一个请求。
                    continue;
                }
                for &b in &buf[..n] {
                    if b == b'\n' {
                        let l = String::from_utf8_lossy(&line).trim().to_string();
                        line.clear();
                        if let Some(payload) = l.strip_prefix("data:") {
                            let payload = payload.trim();
                            if let Some(b64) = string_field(payload, "data") {
                                if !b64.is_empty() {
                                    let pcm = base64_decode(&b64);
                                    if !pcm.is_empty() {
                                        if first {
                                            first = false;
                                            println!("[cloud] tts first chunk {} bytes", pcm.len());
                                        }
                                        total += pcm.len();
                                        sink(&pcm);
                                    }
                                }
                            }
                            if payload.contains("\"finish_reason\":\"stop\"")
                                || payload.contains("finish_reason\":\"stop")
                            {
                                println!("[cloud] tts stream done, {total} bytes");
                                // 绝不能在这里返回：继续读到 EOF，
                                // 连接完全排干后才能复用。
                                done = true;
                            }
                        }
                    } else if b != b'\r' {
                        line.push(b);
                    }
                }
            }
            println!("[cloud] tts stream eof, {total} bytes");
            if total == 0 {
                return Err("tts stream: no audio".into());
            }
            Ok(String::new())
        })?;
        Ok(())
    }
}

impl Cloud {
    /// 渐进式下载的 TTS：wav 边到边读，每块 PCM 直接交给 `sink`
    ///（跳过 44 字节的头）。
    pub fn tts_progressive<F>(
        &mut self,
        text: &str,
        sample_rate: u32,
        sink: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&[u8]),
    {
        let body = format!(
            "{{\"model\":\"qwen3-tts-flash\",\"input\":{{\"text\":\"{}\",\"voice\":\"Cherry\",\"format\":\"pcm\",\"sample_rate\":{sample_rate}}}}}",
            json_escape(text)
        );
        let url = format!("{DASHSCOPE_BASE}/api/v1/services/aigc/multimodal-generation/generation");
        let resp = self.post(Host::Dash, &url, DASHSCOPE_KEY, body.as_bytes())?;
        let audio_url = string_field(&resp, "url")
            .ok_or_else(|| format!("tts: no url in {}", truncate(&resp, 200)))?;

        let mut buf = [0u8; 2048];
        let mut skip = 44usize;
        let mut total = 0usize;
        let mut first = true;
        self.with_conn(Host::Oss, |conn| {
            conn.initiate_request(Method::Get, &audio_url, &[])
                .map_err(|e| format!("tts get: {e:?}"))?;
            conn.initiate_response()
                .map_err(|e| format!("tts resp: {e:?}"))?;
            let status = conn.status();
            if status != 200 {
                return Err(format!("tts HTTP {status}"));
            }
            loop {
                let n = match conn.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => return Err(format!("tts read: {e:?}")),
                };
                let mut chunk = &buf[..n];
                if skip > 0 {
                    let drop = skip.min(chunk.len());
                    chunk = &chunk[drop..];
                    skip -= drop;
                    if chunk.is_empty() {
                        continue;
                    }
                }
                if first {
                    first = false;
                    println!("[cloud] tts first chunk {} bytes", chunk.len());
                }
                total += chunk.len();
                sink(chunk);
            }
            println!("[cloud] tts done, {total} bytes");
            Ok(String::new())
        })?;
        Ok(())
    }
}

/// 临时吞吐量探测：GET 一个 URL，返回（字节数, 毫秒）。
pub fn speed_test(url: &str) -> Result<(usize, u128), String> {
    let cfg = Configuration {
        buffer_size: Some(8192),
        buffer_size_tx: Some(2048),
        timeout: Some(Duration::from_secs(60)),
        crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
        ..Default::default()
    };
    let mut conn = EspHttpConnection::new(&cfg).map_err(|e| format!("new {e:?}"))?;
    let t0 = std::time::Instant::now();
    conn.initiate_request(Method::Get, url, &[])
        .map_err(|e| format!("req {e:?}"))?;
    conn.initiate_response()
        .map_err(|e| format!("resp {e:?}"))?;
    let tls = t0.elapsed().as_millis();
    let mut total = 0usize;
    let mut buf = vec![0u8; 16384];
    loop {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) => return Err(format!("read {e:?} ({total} bytes)")),
        }
    }
    Ok((total, tls))
}

/// 临时：同样的探测但跳过证书校验，用来测量 TLS 握手里
/// 有多少时间花在对照 CA 列表验证证书链上。
pub fn speed_test_noverify(url: &str) -> Result<(usize, u128), String> {
    let cfg = Configuration {
        buffer_size: Some(8192),
        buffer_size_tx: Some(2048),
        timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    };
    let mut conn = EspHttpConnection::new(&cfg).map_err(|e| format!("new {e:?}"))?;
    let t0 = std::time::Instant::now();
    conn.initiate_request(Method::Get, url, &[])
        .map_err(|e| format!("req {e:?}"))?;
    conn.initiate_response()
        .map_err(|e| format!("resp {e:?}"))?;
    let setup = t0.elapsed().as_millis();
    let mut total = 0usize;
    let mut buf = vec![0u8; 16384];
    loop {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) => return Err(format!("read {e:?}")),
        }
    }
    Ok((total, setup))
}
