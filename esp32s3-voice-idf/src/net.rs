//! Network worker: owns the ASR WebSocket and every blocking HTTP call.
//!
//! The audio loop talks to it with `Cmd` and receives `Ev`. Nothing in here
//! touches I2S, so a slow turn can never starve the DMA.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::{asr_ws::AsrStream, cloud::Cloud, config, prompts};

/// Only transcribe; skip the LLM and TTS steps.
pub const TEXT_ONLY: bool = false;

pub enum Cmd {
    /// Wake word fired: open a recognition task.
    Wake,
    /// One 32 ms frame of 16 kHz mono PCM.
    Audio(Vec<u8>),
    /// Utterance finished: transcribe, ask the LLM, synthesise the answer.
    /// Carries the raw utterance PCM for the HTTP ASR fallback.
    End(Vec<u8>),
}

pub enum Ev {
    /// Recognised text (for the LCD / logs).
    Text(String),
    /// PCM to append to the playback queue.
    Pcm(Vec<u8>),
    /// Turn finished.
    Done,
}

pub fn start() -> (mpsc::Sender<Cmd>, mpsc::Receiver<Ev>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();

    thread::Builder::new()
        .name("net".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut cloud = Cloud::new();
            // Open the ASR socket at start-up; reconnecting on every wake lost
            // the first second of audio and produced empty transcripts.
            println!("[net] opening asr socket");
            let mut asr: Option<AsrStream> = AsrStream::connect();
            // Pre-open the two HTTPS connections (DNS + TLS per host) so the
            // first real turn does not pay 2 handshakes (1-3 s each here).
            // Must stay on this thread (EspHttpConnection is not Send); the
            // 64 KB stack below covers the mbedtls handshake + cert verify,
            // which overflowed the previous 32 KB stack.
            println!("[net] warming up https connections");
            cloud.warmup();
            // The first warm-up usually fails when the link is not up yet
            // (`getaddrinfo 202`). Repeat it once the ASR socket actually
            // connects, otherwise every turn pays a fresh DNS + TLS handshake.
            let mut warmed = false;
            println!("[net] warmup attempt done");

            // Watchdog bookkeeping: the esp websocket client can end up in a
            // dead state that neither emits Disconnected nor auto-reconnects
            // (observed after a single DNS failure), so we poll and rebuild.
            let mut last_ok = Instant::now();
            // Last spoken answer: used to detect the mic picking up our own
            // speaker (no AEC on this board) and to refuse to answer ourselves.
            let mut last_reply = String::new();

            loop {
                let cmd = match cmd_rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(c) => c,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let alive = asr.as_ref().map(|a| a.is_connected()).unwrap_or(false);
                        if alive {
                            last_ok = Instant::now();
                            if !warmed {
                                warmed = true;
                                println!("[net] link is up, warming up again");
                                cloud.warmup();
                            }
                        } else if last_ok.elapsed() > Duration::from_secs(5) {
                            println!("[net] asr socket dead, rebuilding");
                            last_ok = Instant::now();
                            asr = AsrStream::connect();
                            if let Some(a) = asr.as_mut() {
                                a.start_task();
                            }
                        }
                        continue;
                    }
                    Err(_) => break,
                };
                match cmd {
                    Cmd::Wake => {
                        // Reconnect when the socket died (the client's own
                        // auto-reconnect does not always recover).
                        let need = match asr.as_ref() {
                            Some(a) => !a.is_connected(),
                            None => true,
                        };
                        if need {
                            println!("[net] (re)connecting asr socket");
                            asr = AsrStream::connect();
                        }
                        if let Some(a) = asr.as_mut() {
                            // Blocking here is fine: this thread owns nothing but
                            // the network. Waiting for task-started keeps the
                            // first frames of the utterance from being dropped.
                            a.start_task();
                            if !a.wait_started(4000) {
                                // The socket looked alive but the task never
                                // started: rebuild and try exactly once more,
                                // otherwise the whole utterance is lost.
                                println!("[net] asr task did not start, rebuilding socket");
                                asr = AsrStream::connect();
                                if let Some(b) = asr.as_mut() {
                                    b.start_task();
                                    b.wait_started(4000);
                                }
                            }
                        }
                    }
                    Cmd::Audio(pcm) => {
                        if let Some(a) = asr.as_mut() {
                            if a.is_connected() {
                                a.push(&pcm);
                            }
                        }
                    }
                    Cmd::End(utt) => {
                        let t_asr = Instant::now();
                        let (text, sent) = match asr.as_mut() {
                            Some(a) => {
                                let sent = a.frames_sent();
                                let t = a.finish();
                                // Open the next task right away so a follow-up
                                // question has no start-up delay.
                                a.start_task();
                                (t, sent)
                            }
                            None => (String::new(), 0),
                        };
                        let mut text = text.trim().to_string();
                        println!(
                            "[net] asr: '{text}' (frames sent: {sent}, {} ms)",
                            t_asr.elapsed().as_millis()
                        );
                        // Fallback: either the utterance came back empty, or
                        // not a single frame reached the server (dead socket) —
                        // in both cases redo the whole utterance over plain
                        // HTTP so the user's speech is not thrown away.
                        // Only when not a single frame left the board: the
                        // streaming path now waits for the real result, so an
                        // empty transcript there means genuine silence, and a
                        // 10 s HTTP re-upload would just add dead air.
                        if text.chars().count() < 2 && sent == 0 {
                            println!("[net] http asr fallback ({} B, ws sent {sent})", utt.len());
                            match cloud.asr(&utt, 16_000) {
                                Ok(t) if t.trim().chars().count() >= 2 => {
                                    text = t.trim().to_string();
                                    println!("[net] http asr: '{text}'");
                                }
                                Ok(t) => println!("[net] http asr empty: {t}"),
                                Err(e) => println!("[net] http asr failed: {e}"),
                            }
                        }
                        // Echo guard: without AEC the mic can capture the
                        // device's own answer. Anything that is basically a
                        // fragment of what we just said is dropped instead of
                        // being answered again (that loop never ends).
                        if is_echo(&text, &last_reply) {
                            println!("[net] echo of own reply ignored: '{text}'");
                            let _ = ev_tx.send(Ev::Text(String::new()));
                            let _ = ev_tx.send(Ev::Done);
                            continue;
                        }
                        let _ = ev_tx.send(Ev::Text(text.clone()));
                        if TEXT_ONLY {
                            let _ = ev_tx.send(Ev::Done);
                            continue;
                        }

                        if text.chars().count() < 2 {
                            let _ = ev_tx.send(Ev::Pcm(prompts::AGAIN.to_vec()));
                            let _ = ev_tx.send(Ev::Done);
                            continue;
                        }

                        let system = "你是语音助手，用一句话、口语化地回答，不超过 15 个字。";
                        let reply = match cloud.chat(config::DEEPSEEK_KEY, system, &text) {
                            Ok(r) => r.trim().to_string(),
                            Err(e) => {
                                println!("[net] chat failed: {e}");
                                let _ = ev_tx.send(Ev::Pcm(prompts::NET.to_vec()));
                                let _ = ev_tx.send(Ev::Done);
                                continue;
                            }
                        };
                        println!("[net] llm: {reply}");
                        if reply.is_empty() {
                            let _ = ev_tx.send(Ev::Pcm(prompts::AGAIN.to_vec()));
                            let _ = ev_tx.send(Ev::Done);
                            continue;
                        }
                        last_reply = reply.clone();

                        // 8 kHz halves the payload; duplicate samples back to 16 kHz.
                        let tx = ev_tx.clone();
                        let mut sink = |chunk: &[u8]| {
                            let mut out = Vec::with_capacity(chunk.len() * 2);
                            for s in chunk.chunks_exact(2) {
                                out.extend_from_slice(&[s[0], s[1], s[0], s[1]]);
                            }
                            let _ = tx.send(Ev::Pcm(out));
                        };
                        // Streaming TTS first: reuses the warm Dash connection
                        // and starts playing on the first chunk, instead of
                        // waiting for a full JSON + an OSS round-trip.
                        if let Err(e) = cloud.tts_stream(&reply, 8000, &mut sink) {
                            println!("[net] tts stream failed ({e}); falling back");
                            if let Err(e) = cloud.tts_progressive(&reply, 8000, &mut sink) {
                                println!("[net] tts failed: {e}");
                                let _ = ev_tx.send(Ev::Pcm(prompts::NET.to_vec()));
                            }
                        }
                        let _ = ev_tx.send(Ev::Done);
                    }
                }
            }
        })
        .expect("net thread");

    (cmd_tx, ev_rx)
}

/// True when `text` looks like the device's own previous answer leaking back
/// through the microphone (there is no acoustic echo canceller on this board).
/// Detection: the transcript is a contiguous fragment of the last reply, or
/// most of its characters appear in that reply in roughly the same order.
fn is_echo(text: &str, last_reply: &str) -> bool {
    fn norm(s: &str) -> Vec<char> {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .filter(|c| !"，。？！、；：,.?!;:\"'“”‘’（）()~-—…".contains(*c))
            .collect()
    }
    let t = norm(text);
    let l = norm(last_reply);
    if t.len() < 4 || l.len() < 4 {
        return false;
    }
    // Contiguous fragment match, in either direction.
    let ts: String = t.iter().collect();
    let ls: String = l.iter().collect();
    if ls.contains(&ts) || ts.contains(&ls) {
        return true;
    }
    // Order-preserving overlap ratio.
    let mut hit = 0usize;
    let mut j = 0usize;
    for c in t.iter() {
        while j < l.len() && l[j] != *c {
            j += 1;
        }
        if j < l.len() {
            hit += 1;
            j += 1;
        }
    }
    (hit as f32) / (t.len() as f32) >= 0.8
}
