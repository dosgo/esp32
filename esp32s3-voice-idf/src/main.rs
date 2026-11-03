//! ESP32-S3 voice assistant.
//!
//! The audio thread owns I2S / mic / VAD / the state machine and never blocks;
//! `net.rs` owns the ASR WebSocket and all HTTPS calls.
//!
//! Turn: wake -> greeting -> listen -> think -> speak -> follow-up.

use std::cell::RefCell;
use std::collections::VecDeque;
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

mod asr_ws;
mod cloud;
mod config;
mod display;
mod es7210;
mod es8311;
mod net;
mod prompts;
mod vad;
mod wakenet;

/// Frames of silence that end an utterance (~0.64 s).
const SILENCE_FRAMES: u32 = 20;

/// Frames without speech after the wake word before giving up (~4.8 s).
const NO_SPEECH_FRAMES: u32 = 150;

/// Hard cap on one utterance (~3.2 s).
const MAX_SPEECH_FRAMES: u32 = 100;

/// Minimum speech frames for a valid utterance (~0.5 s).
const MIN_SPEECH_FRAMES: u32 = 15;

/// Speech / noise gate on the 32 ms frame RMS.
const SPEECH_RMS: f64 = 250.0;

/// Frames of quiet after the greeting before the mic is trusted (~0.3 s).
const GREETING_QUIET_FRAMES: u32 = 10;

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Idle,
    Greeting,
    Listening,
    Thinking,
    Speaking,
    FollowUp,
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    println!("[boot] voice assistant");

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

    // ---- WiFi ----
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
    wifi.start().unwrap();
    let _ = wifi.wifi_mut().connect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !wifi.is_connected().unwrap_or(false) {
        thread::sleep(Duration::from_millis(150));
    }
    unsafe { esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE) };
    println!("[wifi] connected={}", wifi.is_connected().unwrap_or(false));

    // ---- network worker ----
    let (net_cmd, net_ev) = net::start();

    // ---- I2C: ES8311 (DAC) + ES7210 (ADC) ----
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
    for attempt in 1..=3 {
        if amp.probe().is_some() && amp.init().is_ok() {
            println!("[es8311] ready (attempt {attempt})");
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    let mut codec = es7210::Es7210::new(RefCellDevice::new(&i2c_bus), es7210::ADDR);
    for attempt in 1..=6 {
        match codec.init() {
            Ok(()) => {
                println!("[es7210] ready (attempt {attempt})");
                break;
            }
            Err(e) => println!("[es7210] init failed: {e}"),
        }
        thread::sleep(Duration::from_millis(300));
    }

    // NS4150B PA enable.
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

    let mut wake = match wakenet::WakeNet::new("xiaotexiaote") {
        Ok(wake) => wake,
        Err(err) => {
            println!("[kws] WakeNet init failed: {err}");
            loop {
                thread::sleep(Duration::from_secs(5));
            }
        }
    };
    let chunk = wake.chunk_size();

    let mut vad = match vad::Vad::new() {
        Ok(v) => Some(v),
        Err(e) => {
            println!("[vad] init failed: {e}");
            None
        }
    };

    let mut raw = vec![0u8; chunk * 4];
    let mut mono = vec![0i16; chunk];
    let mut tx_buf = vec![0u8; chunk * 4];
    let mut tts_queue: VecDeque<u8> = VecDeque::new();
    let mut utt: Vec<u8> = Vec::new();

    let mut state = State::Idle;
    let mut last_wifi_check = Instant::now();
    let mut no_ip_checks = 0u32;
    let mut speech_frames = 0u32;
    let mut speech_started = false;
    let mut no_speech_frames = 0u32;
    let mut silence_frames = 0u32;
    let mut onset_frames = 0u32;
    let mut greeting_quiet = 0u32;
    let mut listen_grace_until: Option<Instant> = None;
    let mut followup_until: Option<Instant> = None;
    let mut followup_mute_until: Option<Instant> = None;
    let mut thinking_until: Option<Instant> = None;
    let mut turn_done = false;
    let mut net_pending = false;
    let mut net_since = Instant::now();
    let mut ui_voice = false;
    let mut ui_voice_until: Option<Instant> = None;
    let mut tts_playing = false;
    let mut speaking_idle = 0u32;
    let mut last_state_log = Instant::now();
    let mut peak_rms: f64 = 0.0;

    let _ = lcd.send("READY");
    println!("[boot] ready");

    loop {
        // ---- WiFi watchdog ----
        if last_wifi_check.elapsed() > Duration::from_secs(5) {
            last_wifi_check = Instant::now();
            if wifi.is_up().unwrap_or(false) {
                no_ip_checks = 0;
            } else {
                no_ip_checks += 1;
                println!("[wifi] down, attempt {no_ip_checks}");
                if no_ip_checks % 4 == 0 {
                    let _ = wifi.stop();
                    thread::sleep(Duration::from_millis(400));
                    let _ = wifi.start();
                }
                let _ = wifi.wifi_mut().connect();
            }
        }

        // Safety valve for a network turn that never reports back.
        if net_pending && net_since.elapsed() > Duration::from_secs(30) {
            println!("[net] turn timeout, resetting");
            net_pending = false;
            turn_done = false;
            state = State::Idle;
            let _ = lcd.send("READY");
        }

        // ---- events from the network thread ----
        while let Ok(ev) = net_ev.try_recv() {
            match ev {
                net::Ev::Text(t) => {
                    println!("[net] text: {t}");
                    let _ = lcd.send(if t.trim().is_empty() { "ASR EMPTY" } else { "ASR OK" });
                }
                net::Ev::Pcm(pcm) => {
                    if state == State::Thinking {
                        state = State::Speaking;
                        let _ = lcd.send("SPEAKING");
                        thinking_until = None;
                    }
                    tts_queue.extend(pcm);
                }
                net::Ev::Done => {
                    turn_done = true;
                    net_pending = false;
                }
            }
        }

        // ---- playback bookkeeping ----
        let tts_active = !tts_queue.is_empty() || tts_playing;
        if !tts_queue.is_empty() && !tts_playing {
            tts_playing = true;
            speaking_idle = 0;
        } else if tts_queue.is_empty() && tts_playing {
            speaking_idle += 1;
            if speaking_idle > 30 {
                tts_playing = false;
            }
        }

        if !tts_active {
            match state {
                State::Greeting => {
                    greeting_quiet += 1;
                    if greeting_quiet >= GREETING_QUIET_FRAMES {
                        greeting_quiet = 0;
                        state = State::Listening;
                        speech_frames = 0;
                        speech_started = false;
                        no_speech_frames = 0;
                        silence_frames = 0;
                        utt.clear();
                        listen_grace_until = Some(Instant::now() + Duration::from_millis(500));
                        if let Some(v) = vad.as_mut() {
                            v.reset();
                        }
                        let _ = lcd.send("LISTENING");
                    }
                }
                State::Speaking => {
                    state = State::FollowUp;
                    followup_until = Some(Instant::now() + Duration::from_secs(3));
                    followup_mute_until = Some(Instant::now() + Duration::from_millis(400));
                    onset_frames = 0;
                    if let Some(v) = vad.as_mut() {
                        v.reset();
                    }
                    let _ = lcd.send("READY");
                }
                State::Thinking if turn_done => {
                    state = State::Idle;
                    turn_done = false;
                    thinking_until = None;
                    let _ = lcd.send("READY");
                }
                _ => {}
            }
        }

        if state == State::Thinking {
            if let Some(until) = thinking_until {
                if Instant::now() >= until && !tts_active {
                    println!("[state] thinking timeout");
                    state = State::Idle;
                    turn_done = false;
                    net_pending = false;
                    let _ = lcd.send("READY");
                }
            }
        }

        // ---- audio in ----
        match i2s_rx.read(&mut raw, BLOCK) {
            Ok(n) if n >= chunk * 4 => {}
            _ => continue,
        }

        // ---- audio out ----
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
        } else {
            tx_buf.fill(0);
        }
        if i2s_tx.write(&tx_buf, BLOCK).is_err() {
            println!("[i2s] tx error");
        }

        // ---- mic ----
        let mut rms = 0u64;
        for i in 0..chunk {
            let l = i16::from_le_bytes([raw[i * 4], raw[i * 4 + 1]]);
            let r = i16::from_le_bytes([raw[i * 4 + 2], raw[i * 4 + 3]]);
            let v = (l as i32 + r as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            rms += (v as i64 * v as i64) as u64;
            mono[i] = v;
        }
        let frame_rms = ((rms / chunk as u64) as f64).sqrt();

        peak_rms = peak_rms.max(frame_rms);
        if last_state_log.elapsed() > Duration::from_secs(5) {
            last_state_log = Instant::now();
            println!(
                "[state] {:?} q={} rms={:.0} peak={:.0}",
                state,
                tts_queue.len(),
                frame_rms,
                peak_rms
            );
            peak_rms = 0.0;
        }

        let mut frame_pcm = Vec::with_capacity(chunk * 2);
        for s in &mono {
            frame_pcm.extend_from_slice(&s.to_le_bytes());
        }

        // LCD hint while idle.
        if state == State::Idle {
            if frame_rms > 300.0 {
                if !ui_voice {
                    let _ = lcd.send("LISTENING");
                    ui_voice = true;
                }
                ui_voice_until = Some(Instant::now() + Duration::from_millis(800));
            } else if ui_voice
                && ui_voice_until
                    .map(|t| Instant::now() >= t)
                    .unwrap_or(true)
            {
                let _ = lcd.send("READY");
                ui_voice = false;
            }
        } else if ui_voice {
            ui_voice = false;
        }

        match state {
            State::Idle => {
                if !tts_active && !net_pending && wake.detect(&mono) {
                    println!("[kws] wake");
                    let _ = net_cmd.send(net::Cmd::Wake);
                    tts_queue.clear();
                    tts_queue.extend(prompts::HERE.iter().copied());
                    state = State::Greeting;
                    greeting_quiet = 0;
                    let _ = lcd.send("LISTENING");
                }
            }
            State::Greeting => {}
            State::FollowUp => {
                let expired = followup_until.map(|t| Instant::now() >= t).unwrap_or(true);
                if expired {
                    state = State::Idle;
                    let _ = lcd.send("READY");
                } else if followup_mute_until
                    .map(|t| Instant::now() < t)
                    .unwrap_or(false)
                {
                    onset_frames = 0;
                } else if match vad.as_mut() {
                    Some(v) => v.speech(&mono) && frame_rms > SPEECH_RMS,
                    None => frame_rms > SPEECH_RMS,
                } {
                    onset_frames += 1;
                    if onset_frames >= 1 {
                        let _ = net_cmd.send(net::Cmd::Wake);
                        state = State::Listening;
                        speech_frames = 0;
                        speech_started = false;
                        no_speech_frames = 0;
                        silence_frames = 0;
                        utt.clear();
                        listen_grace_until = Some(Instant::now() + Duration::from_millis(400));
                        if let Some(v) = vad.as_mut() {
                            v.reset();
                        }
                        let _ = lcd.send("LISTENING");
                    }
                } else {
                    onset_frames = 0;
                }
            }
            State::Listening => {
                utt.extend_from_slice(&frame_pcm);
                let _ = net_cmd.send(net::Cmd::Audio(frame_pcm));
                let voiced = match vad.as_mut() {
                    Some(v) => v.speech(&mono) && frame_rms > SPEECH_RMS,
                    None => frame_rms > SPEECH_RMS,
                };
                if !speech_started && !voiced {
                    no_speech_frames += 1;
                    if no_speech_frames >= NO_SPEECH_FRAMES {
                        println!("[state] no speech");
                        state = State::Idle;
                        utt.clear();
                        tts_queue.extend(prompts::AGAIN.iter().copied());
                    }
                } else {
                    if !speech_started {
                        speech_started = true;
                        println!("[vad] start");
                    }
                    speech_frames += 1;
                    if voiced {
                        silence_frames = 0;
                    } else {
                        silence_frames += 1;
                    }
                    let grace_done = listen_grace_until
                        .map(|t| Instant::now() >= t)
                        .unwrap_or(true);
                    if (grace_done
                        && speech_frames >= MIN_SPEECH_FRAMES
                        && silence_frames >= SILENCE_FRAMES)
                        || speech_frames >= MAX_SPEECH_FRAMES
                    {
                        println!("[vad] end speech={speech_frames} sil={silence_frames}");
                        let _ = net_cmd.send(net::Cmd::End);
                        net_pending = true;
                        net_since = Instant::now();
                        utt.clear();
                        state = State::Thinking;
                        turn_done = false;
                        thinking_until = Some(Instant::now() + Duration::from_secs(25));
                        let _ = lcd.send("THINKING");
                    }
                }
            }
            State::Thinking | State::Speaking => {}
        }
    }
}

/// Public DNS fallback: resolvers handed out by some hotspots cannot see the
/// Alibaba endpoints.
pub(crate) fn set_public_dns() {
    unsafe {
        let netif =
            esp_idf_sys::esp_netif_get_handle_from_ifkey(b"WIFI_STA_DEF\0".as_ptr() as *const _);
        if netif.is_null() {
            return;
        }
        for (idx, server) in [&b"223.5.5.5\0"[..], &b"119.29.29.29\0"[..]]
            .iter()
            .enumerate()
        {
            let mut info: esp_idf_sys::esp_netif_dns_info_t = core::mem::zeroed();
            info.ip.type_ = esp_idf_sys::ESP_IPADDR_TYPE_V4 as _;
            info.ip.u_addr.ip4.addr = esp_idf_sys::esp_ip4addr_aton(server.as_ptr() as *const _);
            let kind = if idx == 0 {
                esp_idf_sys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN
            } else {
                esp_idf_sys::esp_netif_dns_type_t_ESP_NETIF_DNS_BACKUP
            };
            esp_idf_sys::esp_netif_set_dns_info(netif, kind, &mut info);
        }
        println!("[wifi] public dns set");
    }
}

/// Called from the network thread before connecting.
pub(crate) fn ensure_dns() {
    use std::net::ToSocketAddrs;
    let host = "ws-ebekoepr6y8ojg9x.cn-beijing.maas.aliyuncs.com:443";
    let ok = host
        .to_socket_addrs()
        .map(|mut it| it.next().is_some())
        .unwrap_or(false);
    if !ok {
        println!("[dns] resolve failed, switching to public dns");
        set_public_dns();
    }
}
