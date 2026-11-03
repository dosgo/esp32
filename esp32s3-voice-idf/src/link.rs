//! Transport layer: either talk to the local gateway over WebSocket, or go
//! straight to the cloud (DashScope ASR/TTS + DeepSeek).
//!
//! Both variants expose the same channel pair:
//!   * `utter` - `Msg::Wake` when the wake word fires, `Msg::Utter(pcm)` when a
//!               complete utterance (16 kHz mono s16le) is ready
//!   * `tts`   - chunks of PCM to play on the speaker
//!
//! The gateway variant additionally handles hello/heartbeat and appends the
//! trailing silence the gateway uses for end-pointing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::ws::client::{
    EspWebSocketClient, EspWebSocketClientConfig, FrameType, WebSocketEventType,
};

use crate::{cloud, config, prompts};

pub enum Transport {
    Cloud,
    Gateway,
}

pub enum Msg {
    Wake,
    Utter(Vec<u8>),
}

pub struct Link {
    pub utter: mpsc::Sender<Msg>,
    pub tts: mpsc::Receiver<Vec<u8>>,
    pub connected: Arc<AtomicBool>,
}


pub fn start(transport: Transport) -> Link {
    let (tts_tx, tts_rx) = mpsc::channel::<Vec<u8>>();
    let (utter_tx, utter_rx) = mpsc::channel::<Msg>();
    let connected = Arc::new(AtomicBool::new(false));

    match transport {
        Transport::Cloud => {
            // NOTE: the cloud pipeline runs inline in the main loop (see
            // `run_cloud_pipeline`) because HTTPS from a secondary thread kept
            // failing with ESP_ERR_HTTP_CONNECT on this build.
            let _ = (utter_rx, tts_tx);
            Link {
                utter: utter_tx,
                tts: tts_rx,
                connected,
            }
        }
        Transport::Gateway => {
            let connected_cb = connected.clone();
            let tts_tx_cb = tts_tx.clone();
            let cfg = EspWebSocketClientConfig {
                reconnect_timeout_ms: Duration::from_secs(3),
                network_timeout_ms: Duration::from_secs(15),
                ..Default::default()
            };
            let ws = EspWebSocketClient::new(
                config::WS_URL,
                &cfg,
                Duration::from_secs(10),
                move |event| {
                    if let Ok(event) = event {
                        match event.event_type {
                            WebSocketEventType::Connected => {
                                connected_cb.store(true, Ordering::Relaxed)
                            }
                            WebSocketEventType::Text(text) => println!("[ws] {text}"),
                            WebSocketEventType::Binary(data) => {
                                let _ = tts_tx_cb.send(data.to_vec());
                            }
                            WebSocketEventType::Disconnected => {
                                connected_cb.store(false, Ordering::Relaxed);
                                println!("[ws] disconnected");
                            }
                            _ => {}
                        }
                    }
                },
            )
            .expect("WebSocket init");

            let connected_tx = connected.clone();
            thread::spawn(move || {
                let mut ws = ws;
                let mut hello_sent = false;
                let mut last_hb = Instant::now();
                while let Ok(msg) = utter_rx.recv() {
                    let up = connected_tx.load(Ordering::Relaxed);
                    if !up {
                        hello_sent = false;
                        continue;
                    }
                    if !hello_sent {
                        let hello = format!(
                            "{{\"type\":\"hello\",\"version\":2,\"format\":\"pcm\",\"sample_rate\":16000,\"channels\":1,\"bits\":16,\"frame_ms\":32,\"config\":{{\"llm\":{{\"backend\":\"deepseek\",\"key\":\"{}\",\"model\":\"deepseek-chat\",\"base\":\"\"}},\"asr\":{{\"chain\":\"local\",\"key\":\"\",\"model\":\"\"}},\"tts\":{{\"chain\":\"piper\",\"voice\":\"\"}}}}}}",
                            config::DEEPSEEK_KEY
                        );
                        let _ = ws.send(FrameType::Text(false), hello.as_bytes());
                        hello_sent = true;
                    }
                    if last_hb.elapsed() >= Duration::from_secs(3) {
                        let _ = ws.send(FrameType::Text(false), br#"{"type":"hb"}"#);
                        last_hb = Instant::now();
                    }
                    match msg {
                        Msg::Wake => {
                            let _ = ws.send(FrameType::Text(false), br#"{"type":"wake"}"#);
                        }
                        Msg::Utter(pcm) => {
                            let _ = ws.send(FrameType::Binary(false), &pcm);
                            // 25 frames of 32 ms silence = the 800 ms the
                            // gateway needs to close the utterance.
                            let silence = [0u8; 1024];
                            for _ in 0..25 {
                                let _ = ws.send(FrameType::Binary(false), &silence);
                            }
                        }
                    }
                }
            });

            Link {
                utter: utter_tx,
                tts: tts_rx,
                connected,
            }
        }
    }
}
