//! ESP32-S3 语音助手。
//!
//! 音频线程独占 I2S / 麦克风 / VAD / 状态机，绝不阻塞；
//! `net.rs` 独占 ASR WebSocket 和所有 HTTPS 请求。
//!
//! 一轮对话：唤醒 -> 应答 -> 收音 -> 思考 -> 播报 -> 结束。

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

/// 服务端尚未确认句子结束时的本地静音帧数（约 0.64 秒）。
const SILENCE_FRAMES: u32 = 20;

/// 同上，但前提是识别端已产出文本且停止更新超过 `SERVER_SETTLE_MS`：
/// 服务端已经判定句子说完了，本地只需确认麦克风确实安静（约 0.19 秒）。
const SILENCE_FRAMES_SERVER: u32 = 6;

/// 识别端保持安静多久，我们才采信它的"沉默"。
const SERVER_SETTLE_MS: u64 = 500;

/// 单句时长硬上限（约 15 秒）。旧的 3.2 秒上限会把长问题拦腰砍断，
/// 设备只回答前半句。
const MAX_SPEECH_FRAMES: u32 = 300;

/// 唤醒词之后无语音持续多久就放弃（约 4.8 秒）。
const NO_SPEECH_FRAMES: u32 = 150;

/// 判定一句话成立所需的最少语音帧数（约 0.5 秒）。
const MIN_SPEECH_FRAMES: u32 = 15;

/// 播放队列连续为空多少帧才算"喇叭说完了"。
/// 这期间持续送零帧让 I2S DMA 保持运转，同时把最后一块音频
/// 从硬件缓冲里冲出去（约 190 毫秒）。
const DRAIN_FRAMES: u32 = 6;

/// 32 毫秒帧 RMS 的语音/噪声门限。刻意设得很低：这只麦克风的正常人声
/// 在 rms 150-400，旧的 250 下限把整句话都吞掉了（实测连续 4 次、
/// 没有一帧过线）。
const SPEECH_RMS: f64 = 150.0;

/// 送给 ASR 的 PCM 上的数字增益。原始信号太小，识别服务经常返回空文本，
/// 用户看到的就是"没听清"。
const ASR_GAIN: f32 = 3.0;

/// 应用层队列最多缓存 4 秒的 16 kHz 单声道 PCM，
/// 有界事件通道借此对 TTS 下载端形成背压。
const MAX_TTS_QUEUE_BYTES: usize = 16_000 * 2 * 4;

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Idle,
    /// 播放本地"在呢"应答；同时 ASR 任务在后台启动。
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
    // 让射频先稳定。start() 后几毫秒就 connect() 的话，
    // 关联成功后立刻被踢（run -> init），实测每次开机多花 35 秒。
    thread::sleep(Duration::from_millis(1200));
    // 等真正的 DHCP 租约，而不是只等到关联：没有租约时任何请求都会
    // 报 `getaddrinfo 202`，之前每次开机前 38 秒设备看起来"不理人"，
    // 其实只是没网。
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
    // ---- 网络线程 ----
    let (net_cmd, net_ev) = net::start();

    // ---- I2C：ES8311（DAC）+ ES7210（ADC）----
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

    // NS4150B 功放使能。
    let mut pa = PinDriver::output(peripherals.pins.gpio7).expect("PA pin");
    pa.set_high().expect("PA high");

    // ---- I2S0 全双工：BCLK=9, DIN=11, DOUT=12, MCLK=8, WS=10 ----
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
    let mut turn_seq: net::TurnId = 0;
    let mut current_turn: Option<net::TurnId> = None;
    let mut ui_voice = false;
    let mut ui_voice_until: Option<Instant> = None;
    // 播放队列连续为空的帧数计数。只有达到 DRAIN_FRAMES 才算
    // 喇叭真正说完（见常量注释）。
    let mut drain_frames = 0u32;
    let mut last_state_log = Instant::now();
    let mut peak_rms: f64 = 0.0;
    // WakeNet 的 wn9 量化模型 clean() 已损坏：一旦报出 DETECTED，
    // 实例不重建就再也不触发。每轮对话结束后重建一次。
    let mut rebuild_wake = false;
    // 环境音量，只在待机时跟踪。用于 LCD 提示和"无语音"超时的合理性，
    // 绝不当作语音门限：这只麦克风的底噪和人声几乎完全重叠。
    let mut noise_floor = 150.0f64;
    let mut wake_dets = 0u32;
    let mut listen_dbg = 0u32;
    // Wi-Fi 已拿到 DHCP 租约，即云端可达。
    let mut online = ip != [0, 0, 0, 0];
    // 当前 Greeting 只是播报通知，播完要回 Idle 而不是开麦（离线时用）。
    let mut announce_only = false;

    let _ = lcd.send("READY");
    println!("[boot] ready");

    loop {
        // ---- WiFi 看门狗 ----
        if last_wifi_check.elapsed() > Duration::from_secs(5) {
            last_wifi_check = Instant::now();
            // 必须关联且有租约：只有关联没有 IP 毫无用处，
            // 在那种状态下重连只会打断正常的 DHCP 交换。
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

        // 网络一轮迟迟不回报时的保险丝。
        if net_pending && net_since.elapsed() > Duration::from_secs(30) {
            println!("[net] turn timeout, resetting");
            if let Some(turn) = current_turn.take() {
                let _ = net_cmd.send(net::Cmd::Cancel(turn));
            }
            net_pending = false;
            turn_done = false;
            tts_queue.clear();
            state = State::Idle;
            let _ = lcd.send("READY");
        }

        // ---- 来自网络线程的事件 ----
        while tts_queue.len() < MAX_TTS_QUEUE_BYTES {
            let Ok(ev) = net_ev.try_recv() else {
                break;
            };
            match ev {
                net::Ev::Text(turn, t) if current_turn == Some(turn) => {
                    println!("[net] text: {t}");
                    let _ = lcd.send(if t.trim().is_empty() {
                        "ASR EMPTY"
                    } else {
                        "ASR OK"
                    });
                }
                net::Ev::Pcm(turn, pcm) if current_turn == Some(turn) => {
                    if state == State::Thinking {
                        state = State::Speaking;
                        let _ = lcd.send("SPEAKING");
                        thinking_until = None;
                    }
                    tts_queue.extend(pcm);
                }
                net::Ev::Done(turn) if current_turn == Some(turn) => {
                    turn_done = true;
                    net_pending = false;
                }
                net::Ev::Text(turn, _) | net::Ev::Pcm(turn, _) | net::Ev::Done(turn) => {
                    println!("[net] discard stale event for turn {turn}");
                }
            }
        }

        // ---- 播放记账 ----
        if tts_queue.is_empty() {
            drain_frames += 1;
        } else {
            drain_frames = 0;
        }
        // "队列空了"不等于"说完了"：流式 TTS 在块与块之间会卡顿几百毫秒，
        // 且最后一段样本入队后 I2S DMA 里还压着几块数据。只有队列连续空
        // DRAIN_FRAMES 帧、且网络线程明确报告流结束，才算真正播完。
        let drained = drain_frames >= DRAIN_FRAMES;
        let audio_busy = !drained;

        match state {
            State::Greeting if drained => {
                if announce_only {
                    // 离线播报（"没有网络"）：说完回待机。
                    announce_only = false;
                    state = State::Idle;
                    let _ = lcd.send("READY");
                } else {
                    // 应答播完：开麦。ASR 任务在唤醒时就已启动，这段提示音
                    // 恰好覆盖它 0.3~1 秒的启动延迟——用户此刻说的话不会丢，
                    // 回复也不会盖在用户话上。
                    speech_frames = 0;
                    speech_started = false;
                    no_speech_frames = 0;
                    silence_frames = 0;
                    utt.clear();
                    listen_dbg = 0;
                    // 开麦前让喇叭先沉住：DMA 尾部已冲掉，但房间里的余响
                    // 还要持续约 200 毫秒。
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
                // 回答已完整播放、TTS 流已结束，关闭本轮——不开隐式追问
                // 窗口：没有回声消除时它只会收到设备自己的声音，
                // 造成自问自答死循环。
                turn_done = false;
                net_pending = false;
                current_turn = None;
                thinking_until = None;
                state = State::Idle;
                let _ = lcd.send("READY");
            }
            State::Thinking if turn_done => {
                // 本轮没有产出音频（回声被丢弃、识别结果为空）。
                state = State::Idle;
                turn_done = false;
                current_turn = None;
                thinking_until = None;
                let _ = lcd.send("READY");
            }
            _ => {}
        }

        if state == State::Thinking {
            if let Some(until) = thinking_until {
                if Instant::now() >= until && !audio_busy {
                    println!("[state] thinking timeout");
                    if let Some(turn) = current_turn.take() {
                        let _ = net_cmd.send(net::Cmd::Cancel(turn));
                    }
                    state = State::Idle;
                    turn_done = false;
                    net_pending = false;
                    let _ = lcd.send("READY");
                }
            }
        }

        // ---- 音频输入 ----
        match i2s_rx.read(&mut raw, BLOCK) {
            Ok(n) if n >= chunk * 4 => {}
            _ => continue,
        }

        // ---- 音频输出 ----
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

        // ---- 麦克风 ----
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
        // 只给识别端加增益；VAD 和语音判定继续用原始电平，
        // 保证阈值的意义不变。
        for s in &mono {
            let v = (*s as f32 * ASR_GAIN).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            frame_pcm.extend_from_slice(&v.to_le_bytes());
        }

        // 待机时的 LCD 提示。
        if state == State::Idle {
            if frame_rms > 300.0 {
                if !ui_voice {
                    let _ = lcd.send("LISTENING");
                    ui_voice = true;
                }
                ui_voice_until = Some(Instant::now() + Duration::from_millis(800));
            } else if ui_voice && ui_voice_until.map(|t| Instant::now() >= t).unwrap_or(true) {
                let _ = lcd.send("READY");
                ui_voice = false;
            }
        } else if ui_voice {
            ui_voice = false;
        }

        match state {
            State::Idle => {
                // 慢速环境噪声跟踪（EMA）：待机时语音太稀疏，不宜让它
                // 带偏跟踪值，响亮的唤醒喊声也会被平均掉。
                // 喇叭播放时必须跳过——没有 AEC，设备自己的 TTS 打进
                // 麦克风 rms 约 20000，会把底噪一路顶到语音判定失效。
                // 离群值剔除：敲击/爆音（rms 远高于底噪）只有单帧，
                // 不能让它们把底噪抬起来。
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
                            // 先启动识别任务：下面播放的"在呢"提示音恰好
                            // 覆盖 run-task 握手需要的窗口，提示音一结束
                            // 麦克风即可用。
                            announce_only = false;
                            turn_seq = turn_seq.wrapping_add(1);
                            current_turn = Some(turn_seq);
                            let _ = net_cmd.send(net::Cmd::Wake(turn_seq));
                            tts_queue.extend(prompts::HERE.iter().copied());
                        } else {
                            // 说出来。离线时装死和设备坏了没区别。
                            announce_only = true;
                            let _ = lcd.send("NO WIFI");
                            tts_queue.extend(prompts::NO_WIFI.iter().copied());
                        }
                    }
                }
            }
            State::Greeting | State::Thinking | State::Speaking => {}
            State::Listening => {
                // 限制 HTTP 兜底缓冲大小（约 6 秒）：再长就得整段重传，
                // 在这条链路上要等很久。
                if utt.len() < 200 * 1024 {
                    utt.extend_from_slice(&frame_pcm);
                }
                if let Some(turn) = current_turn {
                    let _ = net_cmd.send(net::Cmd::Audio(turn, frame_pcm));
                }
                // WebRTC VAD 是频谱分类器，在这块硬件上是唯一可信的
                // 人声判据：麦克风的底噪在 rms 100-200，正常人声在
                // 150-400，任何绝对阈值要么挡掉真人说话（"没听清"的
                // 由来），要么被风扇打开。RMS 只留作数字静音的下限；
                // 削波也算语音，因为削波会破坏 VAD 依赖的频谱特征。
                let clipping = frame_rms > 8000.0;
                let vad_hit = match vad.as_mut() {
                    Some(v) => v.speech(&mono) || clipping,
                    None => frame_rms > SPEECH_RMS,
                };
                let voiced = vad_hit && frame_rms > 120.0;
                // 调试用：观察收音期间的 rms / VAD 轨迹。
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
                        if let Some(turn) = current_turn.take() {
                            let _ = net_cmd.send(net::Cmd::Cancel(turn));
                        }
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
                    // ---- 句子在哪里结束？----
                    // 这个问题由识别端自己回答：用户还在说话时它就输出
                    // 中间文本，说完后停止更新。它的输出静默超过
                    // SERVER_SETTLE_MS 后，本地只需再确认麦克风也安静了，
                    // 比盲等 SILENCE_FRAMES 定时器短得多。socket 一直没
                    // 出文本时回退到定时器。
                    let srv_len = asr_ws::PARTIAL_LEN.load(std::sync::atomic::Ordering::Relaxed);
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
                        let Some(turn) = current_turn else {
                            println!("[net] missing turn id, dropping utterance");
                            state = State::Idle;
                            continue;
                        };
                        let _ = net_cmd.send(net::Cmd::End(turn, std::mem::take(&mut utt)));
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
