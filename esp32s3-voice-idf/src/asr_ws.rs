//! Streaming ASR over the DashScope realtime WebSocket:
//! `wss://{workspace}.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference`
//!
//! One connection is kept open for the whole session. Each utterance starts a
//! task (run-task), streams 16 kHz mono PCM frames, then finishes the task
//! (finish-task) and reads the transcript from `result-generated` events.
//! This avoids the ~10 s HTTP upload of a whole utterance.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use esp_idf_svc::ws::client::{
    EspWebSocketClient, EspWebSocketClientConfig, FrameType, WebSocketEventType,
};

use crate::cloud::{DASHSCOPE_KEY, DASHSCOPE_WS_URL};

/// Server-side end-of-speech signal, published without locks so the audio
/// thread can consult it on every 32 ms frame.
///
/// The local silence timer can only guess where a sentence ends. The
/// recogniser already knows: it returns partial text while the user is still
/// talking and stops updating once the sentence is complete. The audio thread
/// combines both (see `main.rs`), which is what makes segmentation stop
/// guessing.
pub static PARTIAL_LEN: AtomicU32 = AtomicU32::new(0);
/// `now_ms()` when the last partial result arrived.
pub static PARTIAL_AT: AtomicU32 = AtomicU32::new(0);
/// Binary frames accepted by the socket for the current task.
pub static FRAMES_SENT: AtomicU32 = AtomicU32::new(0);

fn boot_instant() -> Instant {
    *BOOT.get_or_init(Instant::now)
}
static BOOT: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the first use of this module.
pub fn now_ms() -> u32 {
    boot_instant().elapsed().as_millis() as u32
}

/// How long ago the recogniser last produced new text (ms).
pub fn partial_age_ms() -> u64 {
    let at = PARTIAL_AT.load(Ordering::Relaxed);
    if at == 0 {
        return u64::MAX;
    }
    // Xtensa has no 64-bit atomics; a 32-bit millisecond stamp wraps after 49
    // days and wrapping_sub still yields the correct small difference.
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
    /// Audio that arrives while run-task is still being acknowledged (the
    /// first 0.3-1 s after the wake word — exactly the user's opening words).
    /// Held here and flushed as soon as the task starts.
    prebuf: Vec<u8>,
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
                                        let ended = text.contains("\"sentence_end\":true");
                                        if ended {
                                            g.transcript.push_str(&t);
                                            g.partial.clear();
                                        } else {
                                            g.partial = t.clone();
                                        }
                                        // Publish progress so the audio thread
                                        // can tell "still talking" from
                                        // "sentence finished".
                                        let total =
                                            g.transcript.chars().count() + g.partial.chars().count();
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
            if let Err(e) = c.send(FrameType::Text(false), text.as_bytes()) {
                // The esp websocket client can die without ever emitting a
                // Disconnected event (e.g. after a DNS failure), leaving our
                // `connected` flag stale. Mark it dead so the net-thread
                // watchdog rebuilds the socket.
                println!("[asr-ws] send_text failed: {e:?}");
                if let Ok(mut g) = self.inner.lock() {
                    g.connected = false;
                }
            }
        }
    }

    pub fn start_task(&mut self) {
        // Idempotent: Idle -> Speaking -> FollowUp -> Listening can call this
        // again while the first task is still running. But a stale `started`
        // flag from a socket that has since died must NOT suppress a new
        // run-task: that silently dropped every frame of the next utterance.
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
        // Not started yet: hold the audio instead of dropping it. run-task
        // acknowledgement can take a few hundred ms and that window contains
        // the user's first words.
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
        let deadline = Instant::now() + Duration::from_secs(8);
        // Do NOT stop at task-finished: on this service the final text can
        // arrive *after* it, and returning then yields an empty transcript —
        // the device then either says "没听清" or falls back to a 10 s HTTP
        // upload of the whole utterance. Wait for actual text instead, and
        // stop once it has been stable for 300 ms.
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
                // Finished but no text yet: give the result a short grace
                // period, then give up (genuine silence).
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

    /// Frames actually pushed into the current task. Zero means the audio
    /// never left the board (dead socket), so the caller must fall back to
    /// whole-utterance HTTP recognition instead of trusting the empty result.
    pub fn frames_sent(&self) -> u32 {
        self.inner.lock().map(|g| g.frames).unwrap_or(0)
    }
}
