//! Streaming ASR over the DashScope realtime WebSocket:
//! `wss://{workspace}.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference`
//!
//! One connection is kept open for the whole session. Each utterance starts a
//! task (run-task), streams 16 kHz mono PCM frames, then finishes the task
//! (finish-task) and reads the transcript from `result-generated` events.
//! This avoids the ~10 s HTTP upload of a whole utterance.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esp_idf_svc::ws::client::{
    EspWebSocketClient, EspWebSocketClientConfig, FrameType, WebSocketEventType,
};

use crate::cloud::{DASHSCOPE_KEY, DASHSCOPE_WS_URL};

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
            headers: Some(Box::leak(format!("Authorization: Bearer {DASHSCOPE_KEY}\r\n").into_boxed_str())),
            crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
            // Recognition events are large JSON payloads; the 1 KB default
            // truncates them and the text never parses.
            // 8 KB so long recognition events are never truncated; with the
            // SPIRAM threshold at 2 KB this buffer lives in PSRAM.
            buffer_size: 8192,
            reconnect_timeout_ms: Duration::from_secs(5),
            // This link delays pongs; the defaults (10 s ping / 20 s timeout)
            // made the client tear down healthy connections.
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
                                        if text.contains("\"sentence_end\":true") {
                                            g.transcript.push_str(&t);
                                            g.partial.clear();
                                        } else {
                                            g.partial = t;
                                        }
                                    }
                                }
                            } else {
                                println!(
                                    "[asr-ws] event: {}",
                                    &text[..text.len().min(160)]
                                );
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
        // Wait for the handshake, then open the first task.
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
            let _ = c.send(FrameType::Text(false), text.as_bytes());
        }
    }

    pub fn start_task(&mut self) {
        // Idempotent: Idle -> Speaking -> FollowUp -> Listening can call this
        // again while the first task is still running.
        if let Ok(g) = self.inner.lock() {
            if g.started || g.pending {
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
            if g.connected {
                g.pending = true;
                g.seq += 1;
                id = format!("dsh-{:08x}", g.seq);
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
        let started = self.inner.lock().map(|g| g.started).unwrap_or(false);
        if !started {
            if let Ok(mut g) = self.inner.lock() {
                if g.dropped < 3 {
                    g.dropped += 1;
                    println!("[asr-ws] drop frame (not started)");
                }
            }
            return;
        }
        if let Some(c) = self.client.as_mut() {
            match c.send(FrameType::Binary(false), pcm) {
                Ok(_) => {
                    if let Ok(mut g) = self.inner.lock() {
                        if g.frames <= 3 {
                            println!("[asr-ws] send ok #{} ({} B)", g.frames, pcm.len());
                        }
                        g.frames += 1;
                        if g.frames % 50 == 0 {
                            println!("[asr-ws] sent {} frames", g.frames);
                        }
                    }
                }
                Err(e) => println!("[asr-ws] send failed: {e:?}"),
            }
        }
    }

    /// Finish the current task and return the transcript (best effort).
    pub fn finish(&mut self) -> String {
        // The task may still be starting (run-task was just sent at wake).
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
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if self.inner.lock().map(|g| g.finished).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let text = self
            .inner
            .lock()
            .map(|g| { let mut t = g.transcript.clone(); t.push_str(&g.partial); t })
            .unwrap_or_default();
        text
    }
}

impl AsrStream {
    /// Make sure a task is open (the socket may connect after `connect()`).
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
    /// Block until the server acknowledges the task (or the timeout expires).
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
}
