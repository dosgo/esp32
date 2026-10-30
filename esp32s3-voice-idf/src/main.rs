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
    let (ws_tx, ws_rx) = mpsc::channel::<&'static str>();
    let (tts_tx, tts_rx) = mpsc::channel::<Vec<u8>>();
    let speaking = Arc::new(AtomicBool::new(false));
    let speaking_cb = speaking.clone();
    let ws_cfg = EspWebSocketClientConfig::default();
    let mut ws = EspWebSocketClient::new(
        config::WS_URL,
        &ws_cfg,
        Duration::from_secs(10),
        move |event| {
            if let Ok(event) = event {
                match event.event_type {
                    WebSocketEventType::Connected => {
                        let _ = ws_tx.send("connected");
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
                    WebSocketEventType::Disconnected => println!("[ws] disconnected"),
                    _ => {}
                }
            }
        },
    )
    .expect("WebSocket init");
    match ws_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(_) => println!("[ws] connected"),
        Err(e) => println!("[ws] no connect: {e:?}"),
    }

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
    let mut listen_grace_until: Option<Instant> = None;
    let mut thinking_until: Option<Instant> = None;
    let mut followup_until: Option<Instant> = None;
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

        if ws.is_connected() {
            if !hello_sent {
                if ws.send(FrameType::Text(false), hello.as_bytes()).is_ok() {
                    println!("[ws] hello sent");
                    let _ = lcd.send("READY");
                }
                hello_sent = true;
            }
            if last_hb.elapsed() >= Duration::from_secs(3) {
                let _ = ws.send(FrameType::Text(false), br#"{"type":"hb"}"#);
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

        let mut rms = [0u64; 2];
        let mut frame_l = 0u64;
        let mut frame_r = 0u64;
        for i in 0..chunk {
            let l = i16::from_le_bytes([raw[i * 4], raw[i * 4 + 1]]);
            let r = i16::from_le_bytes([raw[i * 4 + 2], raw[i * 4 + 3]]);
            frame_l += (l as i64 * l as i64) as u64;
            frame_r += (r as i64 * r as i64) as u64;
        }
        let frame_ch = if frame_r > frame_l { 1 } else { 0 };
        for i in 0..chunk {
            let l = i16::from_le_bytes([raw[i * 4], raw[i * 4 + 1]]);
            let r = i16::from_le_bytes([raw[i * 4 + 2], raw[i * 4 + 3]]);
            let v = if frame_ch == 0 { l } else { r };
            let v = ((v as i32) * 2).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            rms[0] += (v as i64 * v as i64) as u64;
            mono[i] = v;
        }
        rms[1] = 0;
        if frame_ch != mic_ch {
            mic_ch = frame_ch;
            println!("[i2s] mic_ch -> {mic_ch}");
        }

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
            if pre_roll.len() > 96 {
                pre_roll.pop_front();
            }
        }

        match state {
            State::Idle => {
                if !tts_active && unsafe { sr_wake_detect(mono.as_ptr()) } > 0 {
                    println!("[idf-kws] DETECTED after {frames} frames");
                    for pcm in pre_roll.drain(..) {
                        let _ = ws.send(FrameType::Binary(false), &pcm);
                    }
                    let _ = lcd.send("LISTENING");
                    state = State::Listening;
                    speech_frames = 0;
                    silence_frames = 0;
                    onset_frames = 0;
                    listen_grace_until =
                        Some(Instant::now() + Duration::from_millis(2000));
                }
            }
            State::FollowUp => {
                if let Some(until) = followup_until {
                    if Instant::now() >= until {
                        state = State::Idle;
                        let _ = lcd.send("READY");
                    } else if frame_rms > 300.0 {
                        onset_frames += 1;
                        if onset_frames >= 2 {
                            for pcm in pre_roll.drain(..) {
                                let _ = ws.send(FrameType::Binary(false), &pcm);
                            }
                            let _ = lcd.send("LISTENING");
                            state = State::Listening;
                            speech_frames = 0;
                            silence_frames = 0;
                            listen_grace_until =
                                Some(Instant::now() + Duration::from_millis(1000));
                        }
                    } else {
                        onset_frames = 0;
                    }
                }
            }
            State::Listening => {
                if let Err(e) = ws.send(FrameType::Binary(false), &frame_pcm) {
                    println!("[ws] pcm send failed: {e:?}");
                }
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
            let _ = ws.send(FrameType::Binary(false), &silence);
            tail_left -= 1;
        }
    }
}
