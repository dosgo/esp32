//! 通过 DashScope 实时 WebSocket 做流式识别：
//! `wss://{workspace}.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference`
//!
//! 整个会话保持一条连接。每句话开一个任务（run-task），流式发送
//! 16 kHz 单声道 PCM 帧，然后结束任务（finish-task），从
//! `result-generated` 事件里读识别文本。
//! 这样避免了整段语音约 10 秒的 HTTP 上传。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use esp_idf_svc::ws::client::{
    EspWebSocketClient, EspWebSocketClientConfig, FrameType, WebSocketEventType,
};

use crate::cloud::{DASHSCOPE_KEY, DASHSCOPE_WS_URL};

static AUTH_HEADER: OnceLock<String> = OnceLock::new();

/// 服务端的"说完"信号，无锁发布，音频线程可以每 32 毫秒帧查询一次。
///
/// 本地静音定时器只能猜句子在哪结束。识别端自己知道：用户还在说时
/// 它就返回中间文本，句子说完就停止更新。音频线程把两者结合
///（见 `main.rs`），断句从此不再靠猜。
pub static PARTIAL_LEN: AtomicU32 = AtomicU32::new(0);
/// 最后一次收到中间结果时的 `now_ms()`。
pub static PARTIAL_AT: AtomicU32 = AtomicU32::new(0);
/// 当前任务已被 socket 接收的二进制帧数。
pub static FRAMES_SENT: AtomicU32 = AtomicU32::new(0);

fn boot_instant() -> Instant {
    *BOOT.get_or_init(Instant::now)
}
static BOOT: OnceLock<Instant> = OnceLock::new();

/// 自模块首次使用起经过的毫秒数。
pub fn now_ms() -> u32 {
    boot_instant().elapsed().as_millis() as u32
}

/// 识别端上次产出新文本是多久以前（毫秒）。
pub fn partial_age_ms() -> u64 {
    let at = PARTIAL_AT.load(Ordering::Relaxed);
    if at == 0 {
        return u64::MAX;
    }
    // Xtensa 没有 64 位原子量；32 位毫秒时间戳 49 天回绕一次，
    // wrapping_sub 仍能得到正确的小差值。
    now_ms().wrapping_sub(at) as u64
}

fn reset_publish() {
    PARTIAL_LEN.store(0, Ordering::Relaxed);
    PARTIAL_AT.store(0, Ordering::Relaxed);
    FRAMES_SENT.store(0, Ordering::Relaxed);
}

#[derive(Default)]
struct Inner {
    connected: bool,
    started: bool,
    pending: bool,
    finished: bool,
    transcript: String,
    last_error: String,
    seq: u32,
    frames: u32,
    partial: String,
    dropped: u32,
    task_id: String,
    /// run-task 还在被确认期间到达的音频（唤醒后最初 0.3-1 秒——
    /// 恰好是用户开口的前几个字）。先存在这里，任务一启动立刻补发。
    prebuf: Vec<u8>,
}

fn event_task_id(json: &str) -> Option<&str> {
    let marker = "\"task_id\":\"";
    let start = json.find(marker)? + marker.len();
    let tail = &json[start..];
    Some(&tail[..tail.find('"')?])
}

pub struct AsrStream {
    client: Option<EspWebSocketClient<'static>>,
    inner: Arc<Mutex<Inner>>,
    task_id: String,
}

fn extract_text(json: &str) -> Option<String> {
    // payload.output.sentence.text  /  payload.output.text
    let idx = json.find("\"text\":\"")? + "\"text\":\"".len();
    let bytes = json.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = idx;
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

impl AsrStream {
    pub fn connect() -> Option<Self> {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let inner_cb = inner.clone();
        let cfg = EspWebSocketClientConfig {
            headers: Some(
                AUTH_HEADER
                    .get_or_init(|| format!("Authorization: Bearer {DASHSCOPE_KEY}\r\n"))
                    .as_str(),
            ),
            crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
            // 识别事件是很大的 JSON；默认 1 KB 会截断导致文本解析失败。
            // 8 KB 保证长识别事件不被截断；SPIRAM 阈值 2 KB，
            // 这个缓冲会落在 PSRAM 里。
            buffer_size: 8192,
            reconnect_timeout_ms: Duration::from_secs(5),
            // 这条链路的 pong 有延迟，默认值（10 秒 ping / 20 秒超时）
            // 会把健康连接拆掉。
            ping_interval_sec: Duration::from_secs(30),
            pingpong_timeout_sec: Duration::from_secs(90),
            disable_pingpong_discon: true,
            network_timeout_ms: Duration::from_secs(10),
            ..Default::default()
        };
        let client = EspWebSocketClient::new(
            DASHSCOPE_WS_URL,
            &cfg,
            Duration::from_secs(10),
            move |event| {
                if let Ok(event) = event {
                    match event.event_type {
                        WebSocketEventType::Connected => {
                            if let Ok(mut g) = inner_cb.lock() {
                                g.connected = true;
                            }
                            println!("[asr-ws] connected");
                        }
                        WebSocketEventType::Disconnected => {
                            if let Ok(mut g) = inner_cb.lock() {
                                g.connected = false;
                                g.started = false;
                                g.transcript.clear();
                                g.partial.clear();
                                g.finished = false;
                                g.pending = false;
                            }
                            println!("[asr-ws] disconnected");
                        }
                        WebSocketEventType::Text(text) => {
                            let mut g = match inner_cb.lock() {
                                Ok(g) => g,
                                Err(_) => return,
                            };
                            if let Some(event_id) = event_task_id(text) {
                                if !g.task_id.is_empty() && event_id != g.task_id {
                                    println!("[asr-ws] ignore stale event for {event_id}");
                                    return;
                                }
                            }
                            if text.contains("task-started") {
                                g.started = true;
                                g.pending = false;
                                println!("[asr-ws] task-started");
                            } else if text.contains("task-finished") {
                                g.finished = true;
                                g.started = false;
                                println!("[asr-ws] task-finished");
                            } else if text.contains("result-generated") {
                                if let Some(t) = extract_text(text) {
                                    if !t.is_empty() {
                                        let ended = text.contains("\"sentence_end\":true");
                                        if ended {
                                            g.transcript.push_str(&t);
                                            g.partial.clear();
                                        } else {
                                            g.partial = t.clone();
                                        }
                                        // 发布进度，音频线程借此区分
                                        // "还在说话"和"句子说完了"。
                                        let total = g.transcript.chars().count()
                                            + g.partial.chars().count();
                                        PARTIAL_LEN.store(total as u32, Ordering::Relaxed);
                                        PARTIAL_AT.store(now_ms(), Ordering::Relaxed);
                                        if g.transcript.is_empty() {
                                            println!(
                                                "[asr-ws] partial '{}' end={} total={}",
                                                t, ended, total
                                            );
                                        }
                                    }
                                }
                            } else {
                                println!("[asr-ws] event: {}", &text[..text.len().min(160)]);
                            }
                        }
                        _ => {}
                    }
                }
            },
        )
        .map_err(|e| println!("[asr-ws] connect failed: {e:?}"))
        .ok()?;

        let mut s = Self {
            client: Some(client),
            inner,
            task_id: String::new(),
        };
        // 等握手完成，然后开第一个任务。
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if s.inner.lock().map(|g| g.connected).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Some(s)
    }

    fn send_text(&mut self, text: &str) {
        if let Some(c) = self.client.as_mut() {
            if let Err(e) = c.send(FrameType::Text(false), text.as_bytes()) {
                // esp websocket 客户端可能一声不吭地死掉、永远不发
                // Disconnected 事件（比如一次 DNS 失败之后），导致
                // `connected` 标记失效。这里标死，让 net 线程看门狗重建。
                println!("[asr-ws] send_text failed: {e:?}");
                if let Ok(mut g) = self.inner.lock() {
                    g.connected = false;
                }
            }
        }
    }

    pub fn start_task(&mut self) {
        // 幂等：Idle -> Greeting -> Listening 的流转可能在本任务仍在
        // 运行时再次调用。但绝不能让已死 socket 留下的陈旧 `started`
        // 标记拦住新的 run-task：那会悄悄丢掉下一句的每一帧音频。
        if let Ok(g) = self.inner.lock() {
            if g.connected && (g.started || g.pending) {
                println!("[asr-ws] task already active, skip run-task");
                return;
            }
        }
        let mut id = String::new();
        if let Ok(mut g) = self.inner.lock() {
            g.started = false;
            g.pending = false;
            g.finished = false;
            g.transcript.clear();
            g.partial.clear();
            g.frames = 0;
            g.prebuf.clear();
            reset_publish();
            if g.connected {
                g.pending = true;
                g.seq += 1;
                id = format!("dsh-{:08x}", g.seq);
                g.task_id = id.clone();
            }
        }
        if id.is_empty() {
            println!("[asr-ws] run-task skipped (not connected)");
            return;
        }
        self.task_id = id;
        let body = format!(
            "{{\"header\":{{\"action\":\"run-task\",\"task_id\":\"{}\",\"streaming\":\"duplex\"}},\"payload\":{{\"task_group\":\"audio\",\"task\":\"asr\",\"function\":\"recognition\",\"model\":\"qwen-audio-3.1-asr-flash-streaming\",\"parameters\":{{\"format\":\"pcm\",\"sample_rate\":16000}},\"input\":{{}}}}}}",
            self.task_id
        );
        self.send_text(&body);
        println!("[asr-ws] run-task sent");
    }

    pub fn push(&mut self, pcm: &[u8]) {
        // 还没启动：先攒住而不是丢弃。run-task 的确认可能要几百毫秒，
        // 这段窗口里正是用户开口的前几个字。
        let flush: Option<Vec<u8>> = match self.inner.lock() {
            Ok(mut g) => {
                if !g.started {
                    if g.prebuf.len() < 96 * 1024 {
                        g.prebuf.extend_from_slice(pcm);
                    } else if g.dropped < 3 {
                        g.dropped += 1;
                        println!("[asr-ws] prebuf full, dropping frame");
                    }
                    return;
                }
                let held = std::mem::take(&mut g.prebuf);
                if held.is_empty() {
                    None
                } else {
                    println!("[asr-ws] flushing {} B of pre-start audio", held.len());
                    Some(held)
                }
            }
            Err(_) => return,
        };
        if let Some(held) = flush {
            self.send_binary(&held);
        }
        self.send_binary(pcm);
    }

    fn send_binary(&mut self, pcm: &[u8]) {
        if let Some(c) = self.client.as_mut() {
            match c.send(FrameType::Binary(false), pcm) {
                Ok(_) => {
                    if let Ok(mut g) = self.inner.lock() {
                        if g.frames <= 3 {
                            println!("[asr-ws] send ok #{} ({} B)", g.frames, pcm.len());
                        }
                        g.frames += 1;
                        FRAMES_SENT.store(g.frames, Ordering::Relaxed);
                        if g.frames % 50 == 0 {
                            println!("[asr-ws] sent {} frames", g.frames);
                        }
                    }
                }
                Err(e) => {
                    println!("[asr-ws] send failed: {e:?}");
                    if let Ok(mut g) = self.inner.lock() {
                        g.connected = false;
                    }
                }
            }
        }
    }

    /// 结束当前任务并返回识别文本（尽力而为）。
    pub fn finish(&mut self) -> String {
        // 任务可能还在启动中（唤醒时刚发出 run-task）。
        let wait_started = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < wait_started {
            if self.inner.lock().map(|g| g.started).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = self.inner.lock().map(|g| g.started).unwrap_or(false);
        if started {
            let body = format!(
                "{{\"header\":{{\"action\":\"finish-task\",\"task_id\":\"{}\",\"streaming\":\"duplex\"}},\"payload\":{{\"input\":{{}}}}}}",
                self.task_id
            );
            self.send_text(&body);
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        // 绝不能见到 task-finished 就返回：这个服务的最终文本可能
        // 比它晚到，那时返回就是空文本——设备要么说"没听清"，
        // 要么走 10 秒的整段 HTTP 上传兜底。改为等真正的文本出现，
        // 稳定 300 毫秒后停止等待。
        let mut stable_since: Option<Instant> = None;
        let mut last_len: usize = 0;
        let mut finished_at: Option<Instant> = None;
        while Instant::now() < deadline {
            let (finished, len) = self
                .inner
                .lock()
                .map(|g| (g.finished, g.transcript.len() + g.partial.len()))
                .unwrap_or((false, 0));
            if len > 0 {
                if len == last_len {
                    if stable_since
                        .map(|t| t.elapsed() >= Duration::from_millis(300))
                        .unwrap_or(false)
                    {
                        break;
                    }
                } else {
                    stable_since = Some(Instant::now());
                }
            } else if finished {
                // 已结束但还没有文本：给结果一小段宽限期，
                // 到期放弃（确实没人说话）。
                let t = *finished_at.get_or_insert_with(Instant::now);
                if t.elapsed() >= Duration::from_millis(900) {
                    println!("[asr-ws] finished without any text");
                    break;
                }
            }
            last_len = len;
            std::thread::sleep(Duration::from_millis(40));
        }
        let text = self
            .inner
            .lock()
            .map(|g| {
                let mut t = g.transcript.clone();
                t.push_str(&g.partial);
                t
            })
            .unwrap_or_default();
        text
    }

    /// 关闭被放弃的识别任务并清空所有中间状态。
    pub fn cancel(&mut self) {
        let active = self
            .inner
            .lock()
            .map(|g| g.started || g.pending)
            .unwrap_or(false);
        if active && !self.task_id.is_empty() {
            let body = format!(
                "{{\"header\":{{\"action\":\"finish-task\",\"task_id\":\"{}\",\"streaming\":\"duplex\"}},\"payload\":{{\"input\":{{}}}}}}",
                self.task_id
            );
            self.send_text(&body);
        }
        if let Ok(mut g) = self.inner.lock() {
            g.started = false;
            g.pending = false;
            g.finished = false;
            g.transcript.clear();
            g.partial.clear();
            g.prebuf.clear();
            g.frames = 0;
        }
        reset_publish();
    }
}

impl AsrStream {
    /// 确保任务已开启（socket 可能在 `connect()` 之后才连上）。
    pub fn ensure_task(&mut self) {
        let need = self
            .inner
            .lock()
            .map(|g| g.connected && !g.started && !g.pending && !g.finished)
            .unwrap_or(false);
        if need {
            self.start_task();
        }
    }
}

impl AsrStream {
    /// 阻塞等待服务端确认任务启动（或超时）。
    pub fn wait_started(&mut self, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if self.inner.lock().map(|g| g.started).unwrap_or(false) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("[asr-ws] task did not start in {ms} ms");
        false
    }
}

impl AsrStream {
    pub fn is_connected(&self) -> bool {
        self.inner.lock().map(|g| g.connected).unwrap_or(false)
    }

    /// 当前任务实际送出的音频帧数。为零说明音频根本没离开板子
    ///（socket 已死），调用方必须改走整段 HTTP 识别，
    /// 而不是相信那个空结果。
    pub fn frames_sent(&self) -> u32 {
        self.inner.lock().map(|g| g.frames).unwrap_or(0)
    }
}
