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

/// Frames of local silence that end an utterance when the server has not
/// confirmed the sentence yet (~0.64 s).
const SILENCE_FRAMES: u32 = 20;

/// Same, once the recogniser has produced text and then stopped updating for
/// `SERVER_SETTLE_MS`: the server already decided the sentence is over, so the
/// local timer only has to confirm the mic went quiet (~0.19 s).
const SILENCE_FRAMES_SERVER: u32 = 6;

/// How long the recogniser must stay quiet before we trust its silence.
const SERVER_SETTLE_MS: u64 = 500;

/// Absolute cap on one utterance (~15 s). The old 3.2 s cap cut long questions
/// in half and made the device answer only the first clause.
const MAX_SPEECH_FRAMES: u32 = 300;

/// Frames without speech after the wake word before giving up (~4.8 s).
const NO_SPEECH_FRAMES: u32 = 150;

/// Minimum speech frames for a valid utterance (~0.5 s).
const MIN_SPEECH_FRAMES: u32 = 15;

/// Consecutive empty playback frames that count as "the speaker is done".
/// Sending zeros keeps the I2S DMA moving, so this also flushes the last
/// chunk out of the hardware buffer (~190 ms).
const DRAIN_FRAMES: u32 = 6;

/// Speech / noise gate on the 32 ms frame RMS. Kept low on purpose: this mic
/// delivers a normal speaking voice at rms 150-400, and the old 250 floor
/// swallowed whole questions (observed: 4 attempts, zero frames above gate).
const SPEECH_RMS: f64 = 150.0;

/// Digital make-up gain applied to the PCM that goes to ASR. The raw signal is
/// quiet enough that the recogniser often returns an empty transcript, which
/// the user experiences as "没听清".
const ASR_GAIN: f32 = 3.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Idle,
    /// Playing the short "在呢" acknowledgement; the ASR task starts meanwhile.
    Greeting,
    Listening,
    Thinking,
    Speaking,
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
    // Let the RF stage settle. Calling connect() a few ms after start()
    // reliably got us kicked right after association (run -> init) and cost
    // 35 s per boot in the field.
    thread::sleep(Duration::from_millis(1200));
    // Wait for an actual DHCP lease, not just association: any request made
    // before the lease exists fails with `getaddrinfo 202`, and that burned
    // the first ~38 s after boot — the device looked deaf while it was really
    // just offline.
    let mut ip: [u8; 4] = [0, 0, 0, 0];
    for attempt in 1..=4 {
        let _ = wifi.wifi_mut().connect();
        let deadline = Instant::now() + Duration::from_secs(12);
        while Instant::now() < deadline {
            if let Ok(info) = wifi.wifi().sta_netif().get_ip_info() {
                ip = info.ip.octets();
                if ip != [0, 0, 0, 0] {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
        if ip != [0, 0, 0, 0] {
            break;
        }
        println!("[wifi] no lease yet (attempt {attempt})");
    }
    unsafe { esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE) };
    println!(
        "[wifi] connected={} ip={}.{}.{}.{}",
        wifi.is_connected().unwrap_or(false),
        ip[0],
        ip[1],
        ip[2],
        ip[3]
    );
    // The CPE's resolver intermittently answers 202 for the Alibaba hosts, so
    // pin public resolvers before the first TLS handshake.
    set_public_dns();

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
    let mut listen_grace_until: Option<Instant> = None;
    let mut thinking_until: Option<Instant> = None;
    let mut turn_done = false;
    let mut net_pending = false;
    let mut net_since = Instant::now();
    let mut ui_voice = false;
    let mut ui_voice_until: Option<Instant> = None;
    // Consecutive frames the playback queue has been empty. Only when this
    // reaches DRAIN_FRAMES is the speaker really finished (see the constant).
    let mut drain_frames = 0u32;
    let mut last_state_log = Instant::now();
    let mut peak_rms: f64 = 0.0;
    // WakeNet's wn9 quantized model has no working clean(); once it reports
    // DETECTED it never fires again unless the instance is recreated. Rebuild
    // it after every completed turn.
    let mut rebuild_wake = false;
    // Ambient level, tracked only while idle. Used for the LCD hint and to
    // keep the "no speech" timeout sane — never as a speech gate: on this mic
    // the noise floor and a normal voice overlap almost completely.
    let mut noise_floor = 150.0f64;
    let mut wake_dets = 0u32;
    let mut listen_dbg = 0u32;
    // Wi-Fi holds a DHCP lease, i.e. the cloud is reachable.
    let mut online = ip != [0, 0, 0, 0];
    // The current Greeting only announces something and must return to Idle
    // instead of opening the mic (used while offline).
    let mut announce_only = false;

    let _ = lcd.send("READY");
    println!("[boot] ready");

    loop {
        // ---- WiFi watchdog ----
        if last_wifi_check.elapsed() > Duration::from_secs(5) {
            last_wifi_check = Instant::now();
            // Associated AND leased: an association without an IP is useless
            // and reconnecting on it just interrupted a healthy DHCP exchange.
            let has_ip = wifi
                .wifi()
                .sta_netif()
                .get_ip_info()
                .map(|i| i.ip.octets() != [0, 0, 0, 0])
                .unwrap_or(false);
            if has_ip {
                no_ip_checks = 0;
                online = true;
            } else {
                online = false;
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
        if tts_queue.is_empty() {
            drain_frames += 1;
        } else {
            drain_frames = 0;
        }
        // "Queue empty" alone does NOT mean "finished speaking": a streaming
        // TTS hiccups for hundreds of ms between chunks, and the I2S DMA still
        // holds a few blocks after the last sample is queued. The speaker is
        // done only when the queue has stayed empty for DRAIN_FRAMES *and* the
        // network thread has reported the end of the stream.
        let drained = drain_frames >= DRAIN_FRAMES;
        let audio_busy = !drained;

        match state {
            State::Greeting if drained => {
                if announce_only {
                    // Offline announcement ("没有网络"): say it, then go idle.
                    announce_only = false;
                    state = State::Idle;
                    let _ = lcd.send("READY");
                } else {
                    // The acknowledgement is over: open the mic. The ASR task
                    // was started at wake time, so this prompt is exactly the
                    // window that covers its 0.3-1 s start-up latency —
                    // nothing the user says now is lost, and the reply is
                    // never played over it.
                    speech_frames = 0;
                    speech_started = false;
                    no_speech_frames = 0;
                    silence_frames = 0;
                    utt.clear();
                    listen_dbg = 0;
                    // Let the speaker settle before trusting the mic: the DMA
                    // tail is flushed but the room still rings for ~200 ms.
                    listen_grace_until = Some(Instant::now() + Duration::from_millis(200));
                    if let Some(v) = vad.as_mut() {
                        v.reset();
                    }
                    state = State::Listening;
                    let _ = lcd.send("LISTENING");
                    println!("[state] listening (mic open)");
                }
            }
            State::Speaking if drained && turn_done => {
                // The answer is fully played and the TTS stream is over.
                // Close the turn — no implicit follow-up window: with no
                // acoustic echo canceller it only ever caught the device's
                // own voice and produced the self-talking loop.
                turn_done = false;
                net_pending = false;
                thinking_until = None;
                state = State::Idle;
                let _ = lcd.send("READY");
            }
            State::Thinking if turn_done => {
                // Turn produced no audio (echo dropped, empty transcript).
                state = State::Idle;
                turn_done = false;
                thinking_until = None;
                let _ = lcd.send("READY");
            }
            _ => {}
        }

        if state == State::Thinking {
            if let Some(until) = thinking_until {
                if Instant::now() >= until && !audio_busy {
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
                "[state] {:?} q={} rms={:.0} peak={:.0} det={}",
                state,
                tts_queue.len(),
                frame_rms,
                peak_rms,
                wake_dets
            );
            peak_rms = 0.0;
            wake_dets = 0;
        }

        let mut frame_pcm = Vec::with_capacity(chunk * 2);
        // Make-up gain for the recogniser only; VAD and the voiced gate keep
        // working on the raw level so the thresholds stay meaningful.
        for s in &mono {
            let v = (*s as f32 * ASR_GAIN).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            frame_pcm.extend_from_slice(&v.to_le_bytes());
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
                // Slow ambient tracker (EMA): speech is too sparse here to
                // bias it much, and loud wake-word shouts average out.
                // MUST skip while the speaker is playing — the device's own
                // TTS hits the mic at rms ~20000 (no AEC) and would pump the
                // floor until the voice gate blocked real speech entirely.
                // Outlier rejection: knocks/pops (rms far above the floor)
                // are single frames and must not drag the floor up.
                if !audio_busy && frame_rms < noise_floor * 4.0 {
                    noise_floor = noise_floor * 0.97 + frame_rms * 0.03;
                }
                if rebuild_wake && !audio_busy && !net_pending {
                    rebuild_wake = false;
                    println!("[kws] rebuilding wakenet");
                    if !wake.reset() {
                        println!("[kws] wakenet reset failed");
                    }
                }
                if !audio_busy && !net_pending {
                    wake_dets += 1;
                    if wake.detect(&mono) {
                        println!("[kws] wake online={online}");
                        rebuild_wake = true;
                        tts_queue.clear();
                        drain_frames = 0;
                        state = State::Greeting;
                        speech_frames = 0;
                        speech_started = false;
                        no_speech_frames = 0;
                        silence_frames = 0;
                        utt.clear();
                        listen_dbg = 0;
                        let _ = lcd.send("LISTENING");
                        if online {
                            // Open the recognition task first: the short "在呢"
                            // prompt played below is exactly the window its
                            // run-task handshake needs, so the mic is live the
                            // moment the prompt ends.
                            announce_only = false;
                            let _ = net_cmd.send(net::Cmd::Wake);
                            tts_queue.extend(prompts::HERE.iter().copied());
                        } else {
                            // Say it out loud. Staying silent while offline is
                            // indistinguishable from a broken device.
                            announce_only = true;
                            let _ = lcd.send("NO WIFI");
                            tts_queue.extend(prompts::NO_WIFI.iter().copied());
                        }
                    }
                }
            }
            State::Greeting | State::Thinking | State::Speaking => {}
            State::Listening => {
                // Cap the HTTP-fallback buffer (~6 s): anything longer would be
                // re-uploaded whole and take forever on this link.
                if utt.len() < 200 * 1024 {
                    utt.extend_from_slice(&frame_pcm);
                }
                let _ = net_cmd.send(net::Cmd::Audio(frame_pcm));
                // WebRTC VAD is a spectral classifier and the only trustworthy
                // judge on this hardware: the mic's noise floor sits at rms
                // 100-200 and a normal voice at 150-400, so *any* absolute
                // threshold either blocks real speech (the "没听清" reports) or
                // opens on the fan. RMS is kept only as a floor against
                // digital silence; loud clipping also counts as speech because
                // clipping destroys the features VAD looks at.
                let clipping = frame_rms > 8000.0;
                let vad_hit = match vad.as_mut() {
                    Some(v) => v.speech(&mono) || clipping,
                    None => frame_rms > SPEECH_RMS,
                };
                let voiced = vad_hit && frame_rms > 120.0;
                // Diagnostic: trajectory of rms / VAD while listening.
                listen_dbg += 1;
                if listen_dbg % 16 == 0 || (listen_dbg <= 64 && listen_dbg % 4 == 0) {
                    println!(
                        "[listen] f={} rms={:.0} floor={:.0} vad={} clip={}",
                        listen_dbg, frame_rms, noise_floor, voiced, clipping
                    );
                }
                if !speech_started && !voiced {
                    no_speech_frames += 1;
                    if no_speech_frames >= NO_SPEECH_FRAMES {
                        println!("[state] no speech, back to idle");
                        state = State::Idle;
                        utt.clear();
                        drain_frames = 0;
                        tts_queue.clear();
                        tts_queue.extend(prompts::AGAIN.iter().copied());
                        let _ = lcd.send("READY");
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
                    // ---- where does the sentence end? ----
                    // The recogniser answers that question itself: it emits
                    // partial text while the user is still speaking and then
                    // stops. Once its output has been quiet for
                    // SERVER_SETTLE_MS we only need to confirm the mic went
                    // quiet too, which is a much shorter local wait than the
                    // blind SILENCE_FRAMES timer. Falls back to the timer when
                    // the socket never produced text.
                    let srv_len =
                        asr_ws::PARTIAL_LEN.load(std::sync::atomic::Ordering::Relaxed);
                    let srv_quiet = asr_ws::partial_age_ms();
                    let server_done = srv_len > 0 && srv_quiet >= SERVER_SETTLE_MS;
                    let need_sil = if server_done {
                        SILENCE_FRAMES_SERVER
                    } else {
                        SILENCE_FRAMES
                    };
                    let grace_done = listen_grace_until
                        .map(|t| Instant::now() >= t)
                        .unwrap_or(true);
                    if (grace_done
                        && speech_frames >= MIN_SPEECH_FRAMES
                        && silence_frames >= need_sil)
                        || speech_frames >= MAX_SPEECH_FRAMES
                    {
                        println!(
                            "[vad] end speech={speech_frames} sil={silence_frames} srv={srv_len}/{}ms",
                            if srv_len > 0 { srv_quiet.min(99999) } else { 0 }
                        );
                        let _ = net_cmd.send(net::Cmd::End(std::mem::take(&mut utt)));
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
            State::Thinking | State::Speaking | State::Idle => {}
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
