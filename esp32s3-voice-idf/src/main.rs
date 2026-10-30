//! ESP-IDF std Rust + official esp-sr WakeNet + I2S microphone.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use embedded_hal_bus::i2c::RefCellDevice;
use esp_idf_hal::delay::BLOCK;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_hal::i2s::config::{DataBitWidth, StdConfig};
use esp_idf_hal::i2s::{I2sBiDir, I2sDriver};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::units::FromValueType;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use esp_idf_svc::ws::client::{
    EspWebSocketClient, EspWebSocketClientConfig, FrameType, WebSocketEventType,
};

mod es7210;
mod es8311;
mod display;
mod config;

extern "C" {
    fn sr_wake_init(keyword: *const core::ffi::c_char) -> i32;
    fn sr_wake_detect(pcm: *const i16) -> i32;
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Listening,
    Thinking,
    Speaking,
    FollowUp,
}

enum Outgoing {
    Text(String),
    Binary(Vec<u8>),
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    println!("[idf-kws] boot (I2S + ES7210)");

    let peripherals = Peripherals::take().unwrap();
    let lcd = display::start(
        peripherals.spi2,
        peripherals.pins.gpio38,
        peripherals.pins.gpio39,
        peripherals.pins.gpio21,
        peripherals.pins.gpio45,
        peripherals.pins.gpio40,
        peripherals.pins.gpio46,
    );
    let _ = lcd.send("BOOT");

    // ---- WiFi: must be connected before allocating WakeNet/audio ----
    let sys_loop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs)).unwrap(),
        sys_loop,
    )
    .unwrap();
    let wifi_cfg = Configuration::Client(ClientConfiguration {
        ssid: config::WIFI_SSID.try_into().unwrap(),
        password: config::WIFI_PASSWORD.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    });
    wifi.set_configuration(&wifi_cfg).unwrap();
    let _ = lcd.send("WIFI");
    wifi.start().unwrap();
    // Non-blocking connect: BlockingWifi::connect() can sit in its 15s
    // CONNECT_TIMEOUT even though the link is already up, which is why the
    // screen stayed on BOOT for so long.
    if let Err(e) = wifi.wifi_mut().connect() {
        println!("[wifi] connect request: {e:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut wifi_ok = false;
    while Instant::now() < deadline {
        if wifi.is_connected().unwrap_or(false) {
            wifi_ok = true;
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }
    if wifi_ok {
        let up_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < up_deadline && !wifi.is_up().unwrap_or(false) {
            thread::sleep(Duration::from_millis(100));
        }
        let _ = lcd.send("WIFI OK");
    }
    println!("[wifi] connected={wifi_ok}");

    // ---- WebSocket to the gateway ----
    // All sends go through a dedicated thread. esp_websocket_client's send is
    // blocking; flushing pre-roll directly in the audio loop delayed Listening
    // until the command was already over.
    let (out_tx, out_rx) = mpsc::sync_channel::<Outgoing>(256);
    let (tts_tx, tts_rx) = mpsc::channel::<Vec<u8>>();
    let speaking = Arc::new(AtomicBool::new(false));
    let speaking_cb = speaking.clone();
    let ws_connected = Arc::new(AtomicBool::new(false));
    let ws_connected_cb = ws_connected.clone();
    let ws_cfg = EspWebSocketClientConfig::default();
    let ws = EspWebSocketClient::new(
        config::WS_URL,
        &ws_cfg,
        Duration::from_secs(10),
        move |event| {
            if let Ok(event) = event {
                match event.event_type {
                    WebSocketEventType::Connected => {
                        ws_connected_cb.store(true, Ordering::Relaxed);
                    }
                    WebSocketEventType::Text(text) => {
                        println!("[ws] {text}");
                        if text.contains("\"tts\"") && text.contains("start") {
                            speaking_cb.store(true, Ordering::Relaxed);
                        } else if text.contains("\"tts\"") && text.contains("stop") {
                            speaking_cb.store(false, Ordering::Relaxed);
                        }
                    }
                    WebSocketEventType::Binary(data) => {
                        speaking_cb.store(true, Ordering::Relaxed);
                        let _ = tts_tx.send(data.to_vec());
                    }
                    WebSocketEventType::Disconnected => {
                        ws_connected_cb.store(false, Ordering::Relaxed);
                        println!("[ws] disconnected");
                    }
                    _ => {}
                }
            }
        },
    )
    .expect("WebSocket init");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !ws_connected.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
    if ws_connected.load(Ordering::Relaxed) {
        println!("[ws] connected");
    } else {
        println!("[ws] no connect: timeout");
    }
    thread::spawn(move || {
        let mut ws = ws;
        while let Ok(msg) = out_rx.recv() {
            let r = match msg {
                Outgoing::Text(s) => ws.send(FrameType::Text(false), s.as_bytes()),
                Outgoing::Binary(b) => ws.send(FrameType::Binary(false), &b),
            };
            if let Err(e) = r {
                println!("[ws] send failed: {e:?}");
            }
        }
    });

    // ---- I2C: ES8311 DAC + ES7210 ADC share SDA=42 / SCL=41 ----
    let i2c_cfg = I2cConfig::new().baudrate(100.kHz().into());
    let i2c = I2cDriver::new(
        peripherals.i2c0,
        peripherals.pins.gpio42,
        peripherals.pins.gpio41,
        &i2c_cfg,
    )
    .expect("I2C init");
    let i2c_bus = RefCell::new(i2c);

    let mut amp = es8311::Es8311::new(RefCellDevice::new(&i2c_bus), 0x18);
    let mut amp_ok = false;
    for attempt in 1..=3 {
        if amp.probe().is_some() && amp.init().is_ok() {
            println!("[es8311] ready (attempt {attempt})");
            amp_ok = true;
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    if !amp_ok {
        println!("[es8311] init failed; speaker will be silent");
    }

    let mut codec = es7210::Es7210::new(RefCellDevice::new(&i2c_bus), es7210::ADDR);
    let mut codec_ok = false;
    for attempt in 1..=3 {
        match codec.init() {
            Ok(()) => {
                println!("[es7210] ready (attempt {attempt})");
                codec_ok = true;
                break;
            }
            Err(e) => println!("[es7210] init failed: {e}"),
        }
        thread::sleep(Duration::from_millis(200));
    }
    if !codec_ok {
        println!("[es7210] giving up; microphone will be silent");
    }

    // NS4150B PA enable: GPIO7 high.
    let mut pa = PinDriver::output(peripherals.pins.gpio7).expect("PA pin");
    pa.set_high().expect("PA high");

    // ---- I2S0 full duplex: BCLK=9, DIN=11, DOUT=12, MCLK=8, WS=10 ----
    let std_cfg = StdConfig::philips(16_000, DataBitWidth::Bits16);
    let mut i2s = I2sDriver::<I2sBiDir>::new_std_bidir(
        peripherals.i2s0,
        &std_cfg,
        peripherals.pins.gpio9,
        peripherals.pins.gpio11,
        peripherals.pins.gpio12,
        Some(peripherals.pins.gpio8),
        peripherals.pins.gpio10,
    )
    .expect("I2S bidir init");
    let (mut i2s_rx, mut i2s_tx) = i2s.split();
    i2s_rx.rx_enable().expect("I2S RX enable");
    i2s_tx.tx_enable().expect("I2S TX enable");

    let chunk = unsafe { sr_wake_init(c"灏忕壒灏忕壒".as_ptr()) };
    if chunk <= 0 {
        println!("[idf-kws] WakeNet init failed: {chunk}");
        loop {
            thread::sleep(Duration::from_secs(5));
        }
    }
    let chunk = chunk as usize;
    println!("[idf-kws] WakeNet chunk={chunk}");

    // I2S std mode is stereo: 2 channels * 2 bytes per sample.
    let mut raw = vec![0u8; chunk * 4];
    let mut mono = vec![0i16; chunk];
    let mut hello = String::from(
        r#"{"type":"hello","version":2,"format":"pcm","sample_rate":16000,"channels":1,"bits":16,"frame_ms":32,"config":{"llm":{"backend":"deepseek","key":""#,
    );
    hello.push_str(config::DEEPSEEK_KEY);
    hello.push_str(
        r#"","model":"deepseek-chat","base":""},"asr":{"chain":"local","key":"","model":""},"tts":{"chain":"piper","voice":""}}}"#,
    );

    let mut frames = 0u32;
    let mut mic_ch = 0usize;
    let mut state = State::Idle;
    let mut tail_left = 0u32;
    let mut speech_frames = 0u32;
    let mut silence_frames = 0u32;
    let mut onset_frames = 0u32;
    let mut ui_voice = false;
    let mut ui_voice_until: Option<Instant> = None;
    let mut listen_grace_until: Option<Instant> = None;
    let mut thinking_until: Option<Instant> = None;
    let mut followup_until: Option<Instant> = None;
    let mut followup_mute_until: Option<Instant> = None;
    let mut hello_sent = false;
    let mut last_hb = Instant::now();
    let mut tts_queue: VecDeque<u8> = VecDeque::new();
    let mut tx_buf = vec![0u8; chunk * 4];
    let mut tts_playing = false;
    let mut speaking_idle = 0u32;
    let mut pre_roll: VecDeque<Vec<u8>> = VecDeque::new();

    loop {
        while let Ok(v) = tts_rx.try_recv() {
            tts_queue.extend(v);
        }

        let mut speaking_now = speaking.load(Ordering::Relaxed);
        if speaking_now && tts_queue.is_empty() {
            speaking_idle += 1;
            if speaking_idle > 100 {
                speaking.store(false, Ordering::Relaxed);
                speaking_now = false;
                speaking_idle = 0;
            }
        } else {
            speaking_idle = 0;
        }
        let tts_active = speaking_now || !tts_queue.is_empty();

        if tts_active {
            if state != State::Speaking {
                state = State::Speaking;
                let _ = lcd.send("SPEAKING");
            }
            thinking_until = None;
            tail_left = 0;
            pre_roll.clear();
        } else if state == State::Speaking {
            state = State::FollowUp;
            followup_until = Some(Instant::now() + Duration::from_secs(5));
            followup_mute_until = Some(Instant::now() + Duration::from_millis(1500));
            onset_frames = 0;
            let _ = lcd.send("READY");
        }

        if state == State::Thinking {
            if let Some(until) = thinking_until {
                if Instant::now() >= until {
                    state = State::Idle;
                    let _ = lcd.send("READY");
                }
            }
        }

        if !tts_queue.is_empty() && !tts_playing {
            tts_playing = true;
        } else if tts_queue.is_empty() && tts_playing {
            tts_playing = false;
        }

        if ws_connected.load(Ordering::Relaxed) {
            if !hello_sent {
                let _ = out_tx.send(Outgoing::Text(hello.to_string()));
                hello_sent = true;
                let _ = lcd.send("READY");
            }
            if last_hb.elapsed() >= Duration::from_secs(3) {
                let _ = out_tx.try_send(Outgoing::Text(r#"{"type":"hb"}"#.to_string()));
                last_hb = Instant::now();
            }
        } else {
            hello_sent = false;
        }

        match i2s_rx.read(&mut raw, BLOCK) {
            Ok(n) if n >= chunk * 4 => {}
            Ok(n) => {
                println!("[i2s] short read {n}/{}", chunk * 4);
                continue;
            }
            Err(e) => {
                println!("[i2s] read error: {e:?}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        }

        if !tts_queue.is_empty() {
            for pair in tx_buf.chunks_exact_mut(4) {
                let s = if tts_queue.len() >= 2 {
                    let lo = tts_queue.pop_front().unwrap();
                    let hi = tts_queue.pop_front().unwrap();
                    i16::from_le_bytes([lo, hi])
                } else {
                    0
                };
                let b = s.to_le_bytes();
                pair[0] = b[0];
                pair[1] = b[1];
                pair[2] = b[0];
                pair[3] = b[1];
            }
            if let Err(e) = i2s_tx.write(&tx_buf, BLOCK) {
                println!("[i2s] tx error: {e:?}");
            }
        }

        // Sum both ADC slots. The two ES7210 channels are nearly equal in
        // noise, and picking a per-frame "louder" channel made the waveform
        // alternate between channels every 32ms, which destroyed ASR.
        let mut rms = [0u64; 2];
        for i in 0..chunk {
            let l = i16::from_le_bytes([raw[i * 4], raw[i * 4 + 1]]);
            let r = i16::from_le_bytes([raw[i * 4 + 2], raw[i * 4 + 3]]);
            let v = (l as i32 + r as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            rms[0] += (v as i64 * v as i64) as u64;
            mono[i] = v;
        }
        rms[1] = 0;
        mic_ch = 0;

        frames = frames.wrapping_add(1);
        let frame_rms = ((rms[0] / chunk as u64) as f64).sqrt();
        if frames % 32 == 0 {
            println!("[mic] rms={frame_rms:.0} ch={mic_ch}");
        }

        let mut frame_pcm = Vec::with_capacity(mono.len() * 2);
        for s in &mono {
            frame_pcm.extend_from_slice(&s.to_le_bytes());
        }

        if state == State::Idle || state == State::FollowUp {
            pre_roll.push_back(frame_pcm.clone());
            if pre_roll.len() > 64 {
                pre_roll.pop_front();
            }
        }

        if state == State::Idle && !tts_active {
            if frame_rms > 300.0 {
                if !ui_voice {
                    let _ = lcd.send("LISTENING");
                    ui_voice = true;
                }
                ui_voice_until = Some(Instant::now() + Duration::from_millis(800));
            } else if ui_voice {
                if ui_voice_until.map(|t| Instant::now() >= t).unwrap_or(true) {
                    let _ = lcd.send("READY");
                    ui_voice = false;
                }
            }
        } else if ui_voice {
            ui_voice = false;
        }

        match state {
            State::Idle => {
                if !tts_active && unsafe { sr_wake_detect(mono.as_ptr()) } > 0 {
                    println!("[idf-kws] DETECTED after {frames} frames");
                    // Queue the last 2s of pre-roll. The sender thread makes
                    // this non-blocking, so LISTENING starts immediately.
                    while pre_roll.len() > 64 {
                        pre_roll.pop_front();
                    }
                    for pcm in pre_roll.drain(..) {
                        let _ = out_tx.try_send(Outgoing::Binary(pcm));
                    }
                    let _ = lcd.send("LISTENING");
                    state = State::Listening;
                    speech_frames = 0;
                    silence_frames = 0;
                    onset_frames = 0;
                    listen_grace_until = Some(Instant::now() + Duration::from_millis(2000));
                }
            }
            State::FollowUp => {
                if let Some(until) = followup_until {
                    if Instant::now() >= until {
                        state = State::Idle;
                        let _ = lcd.send("READY");
                    } else if followup_mute_until
                        .map(|t| Instant::now() < t)
                        .unwrap_or(false)
                    {
                        onset_frames = 0;
                    } else if frame_rms > 300.0 {
                        onset_frames += 1;
                        if onset_frames >= 2 {
                            for pcm in pre_roll.drain(..) {
                                let _ = out_tx.try_send(Outgoing::Binary(pcm));
                            }
                            let _ = lcd.send("LISTENING");
                            state = State::Listening;
                            speech_frames = 0;
                            silence_frames = 0;
                            listen_grace_until = Some(Instant::now() + Duration::from_millis(1000));
                        }
                    } else {
                        onset_frames = 0;
                    }
                }
            }
            State::Listening => {
                let _ = out_tx.try_send(Outgoing::Binary(frame_pcm.clone()));
                speech_frames += 1;
                if frame_rms > 300.0 {
                    silence_frames = 0;
                } else {
                    silence_frames += 1;
                }
                let grace_done = listen_grace_until
                    .map(|until| Instant::now() >= until)
                    .unwrap_or(true);
                if (grace_done && speech_frames >= 10 && silence_frames >= 22)
                    || speech_frames >= 150
                {
                    println!("[stream] end frames={speech_frames} silence={silence_frames}");
                    state = State::Thinking;
                    thinking_until = Some(Instant::now() + Duration::from_secs(10));
                    listen_grace_until = None;
                    tail_left = 25;
                    let _ = lcd.send("THINKING");
                }
            }
            State::Thinking | State::Speaking => {}
        }

        if tail_left > 0 {
            let silence = vec![0u8; frame_pcm.len()];
            let _ = out_tx.try_send(Outgoing::Binary(silence));
            tail_left -= 1;
        }
    }
}
