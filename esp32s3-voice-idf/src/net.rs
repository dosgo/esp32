//! Network worker: owns the ASR WebSocket and every blocking HTTP call.
//!
//! The audio loop talks to it with `Cmd` and receives `Ev`. Nothing in here
//! touches I2S, so a slow turn can never starve the DMA.

use std::sync::mpsc;
use std::thread;

use crate::{asr_ws::AsrStream, cloud::Cloud, config, prompts};

/// Only transcribe; skip the LLM and TTS steps.
pub const TEXT_ONLY: bool = false;

pub enum Cmd {
    /// Wake word fired: open a recognition task.
    Wake,
    /// One 32 ms frame of 16 kHz mono PCM.
    Audio(Vec<u8>),
    /// Utterance finished: transcribe, ask the LLM, synthesise the answer.
    End,
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
        .stack_size(32 * 1024)
        .spawn(move || {
            let mut cloud = Cloud::new();
            // Open the ASR socket at start-up; reconnecting on every wake lost
            // the first second of audio and produced empty transcripts.
            println!("[net] opening asr socket");
            let mut asr: Option<AsrStream> = AsrStream::connect();

            while let Ok(cmd) = cmd_rx.recv() {
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
                            a.wait_started(4000);
                        }
                    }
                    Cmd::Audio(pcm) => {
                        if let Some(a) = asr.as_mut() {
                            if a.is_connected() {
                                a.push(&pcm);
                            }
                        }
                    }
                    Cmd::End => {
                        let text = match asr.as_mut() {
                            Some(a) => {
                                let t = a.finish();
                                // Open the next task right away so a follow-up
                                // question has no start-up delay.
                                a.start_task();
                                t
                            }
                            None => String::new(),
                        };
                        let text = text.trim().to_string();
                        println!("[net] asr: '{text}'");
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

                        // 8 kHz halves the payload; duplicate samples back to 16 kHz.
                        let tx = ev_tx.clone();
                        let mut sink = |chunk: &[u8]| {
                            let mut out = Vec::with_capacity(chunk.len() * 2);
                            for s in chunk.chunks_exact(2) {
                                out.extend_from_slice(&[s[0], s[1], s[0], s[1]]);
                            }
                            let _ = tx.send(Ev::Pcm(out));
                        };
                        if let Err(e) = cloud.tts_progressive(&reply, 8000, &mut sink) {
                            println!("[net] tts failed: {e}");
                            let _ = ev_tx.send(Ev::Pcm(prompts::NET.to_vec()));
                        }
                        let _ = ev_tx.send(Ev::Done);
                    }
                }
            }
        })
        .expect("net thread");

    (cmd_tx, ev_rx)
}
