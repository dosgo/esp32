//! 网络线程：独占 ASR WebSocket 和所有阻塞式 HTTP 请求。
//!
//! 音频循环通过 `Cmd` 与它通信，通过 `Ev` 接收结果。这里绝不碰 I2S，
//! 所以网络再慢也不会饿死 DMA。

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::{asr_ws::AsrStream, cloud::Cloud, config, prompts};

/// 只做识别；跳过 LLM 和 TTS。
pub const TEXT_ONLY: bool = false;

pub type TurnId = u32;

pub enum Cmd {
    /// 唤醒词触发：开一个识别任务。
    Wake(TurnId),
    /// 一帧 32 毫秒的 16 kHz 单声道 PCM。
    Audio(TurnId, Vec<u8>),
    /// 一句话说完：识别、问 LLM、合成回答。
    /// 附带原始语音 PCM，供 HTTP ASR 兜底使用。
    End(TurnId, Vec<u8>),
    /// 放弃一轮监听，不跑 LLM/TTS。
    Cancel(TurnId),
}

pub enum Ev {
    /// 识别出的文本（给 LCD / 日志用）。
    Text(TurnId, String),
    /// 追加到播放队列的 PCM。
    Pcm(TurnId, Vec<u8>),
    /// 本轮结束。
    Done(TurnId),
}

pub fn start() -> (mpsc::Sender<Cmd>, mpsc::Receiver<Ev>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    // TTS 下载太快时不能把整个回答都堆进宝贵的内存里。
    // 通道满了网络线程就等音频循环把队列排空。
    let (ev_tx, ev_rx) = mpsc::sync_channel::<Ev>(8);

    thread::Builder::new()
        .name("net".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut cloud = Cloud::new();
            // 启动时就打开 ASR socket：每次唤醒才重连的话会丢掉
            // 开头一秒的音频，识别结果为空。
            println!("[net] opening asr socket");
            let mut asr: Option<AsrStream> = AsrStream::connect();
            // 预建两条 HTTPS 连接（每台主机一次 DNS + TLS），让第一轮
            // 真实对话不用付两次握手（本机上每次 1-3 秒）。必须留在本
            // 线程（EspHttpConnection 不是 Send）；下面 64 KB 的栈覆盖
            // mbedtls 握手 + 证书校验，之前 32 KB 的栈溢出了。
            println!("[net] warming up https connections");
            cloud.warmup();
            // 链路还没通时第一次预热通常失败（`getaddrinfo 202`）。
            // 等 ASR socket 真正连上后再补一次，否则每轮对话都要
            // 重新付 DNS + TLS 握手。
            let mut warmed = false;
            println!("[net] warmup attempt done");

            // 看门狗记录：esp websocket 客户端可能进入一种死状态——
            // 既不发 Disconnected 也不自动重连（一次 DNS 失败后就
            // 观察到过），所以轮询并重建。
            let mut last_ok = Instant::now();
            // 上一条语音回答：用于检测麦克风收到自己喇叭的声音
            //（这块板没有 AEC）并拒绝回答自己。
            let mut last_reply = String::new();
            let mut active_turn: Option<TurnId> = None;

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
                        }
                        continue;
                    }
                    Err(_) => break,
                };
                match cmd {
                    Cmd::Wake(turn) => {
                        if active_turn != Some(turn) {
                            if let (Some(old), Some(a)) = (active_turn, asr.as_mut()) {
                                println!("[net] cancel stale turn {old} before {turn}");
                                a.cancel();
                            }
                            active_turn = Some(turn);
                        }
                        // socket 已死就重连（客户端自己的自动重连
                        // 并不总是能恢复）。
                        let need = match asr.as_ref() {
                            Some(a) => !a.is_connected(),
                            None => true,
                        };
                        if need {
                            println!("[net] (re)connecting asr socket");
                            asr = AsrStream::connect();
                        }
                        if let Some(a) = asr.as_mut() {
                            // 这里阻塞没问题：本线程只负责网络。等待
                            // task-started 是为了不丢开头几帧语音。
                            a.start_task();
                            if !a.wait_started(4000) {
                                // socket 看着活着但任务没启动：重建并
                                // 恰好再试一次，否则整句话就丢了。
                                println!("[net] asr task did not start, rebuilding socket");
                                asr = AsrStream::connect();
                                if let Some(b) = asr.as_mut() {
                                    b.start_task();
                                    b.wait_started(4000);
                                }
                            }
                        }
                    }
                    Cmd::Audio(turn, pcm) => {
                        if active_turn != Some(turn) {
                            continue;
                        }
                        if let Some(a) = asr.as_mut() {
                            if a.is_connected() {
                                a.push(&pcm);
                            }
                        }
                    }
                    Cmd::Cancel(turn) => {
                        if active_turn == Some(turn) {
                            if let Some(a) = asr.as_mut() {
                                a.cancel();
                            }
                            active_turn = None;
                            println!("[net] turn {turn} cancelled");
                        }
                    }
                    Cmd::End(turn, utt) => {
                        if active_turn != Some(turn) {
                            println!("[net] ignore stale end for turn {turn}");
                            continue;
                        }
                        let t_asr = Instant::now();
                        let (text, sent) = match asr.as_mut() {
                            Some(a) => {
                                let sent = a.frames_sent();
                                let t = a.finish();
                                (t, sent)
                            }
                            None => (String::new(), 0),
                        };
                        active_turn = None;
                        let mut text = text.trim().to_string();
                        println!(
                            "[net] asr: '{text}' (frames sent: {sent}, {} ms)",
                            t_asr.elapsed().as_millis()
                        );
                        // 兜底：要么识别结果为空，要么一帧都没发出去
                        //（socket 已死）——两种情况都用普通 HTTP 把整段
                        // 语音重传一遍，不让用户说的话白说。
                        // 仅在一帧都没发出时才走：流式路径现在会等真正的
                        // 结果，那里的空文本意味着确实没人说话，
                        // 再花 10 秒 HTTP 重传只会徒增冷场。
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
                        // 回声拦截：没有 AEC 时麦克风会收到设备自己的
                        // 回答。凡基本是刚才那句话片段的，直接丢弃
                        // 而不是再回答一遍（那个循环永无止境）。
                        if is_echo(&text, &last_reply) {
                            println!("[net] echo of own reply ignored: '{text}'");
                            let _ = ev_tx.send(Ev::Text(turn, String::new()));
                            let _ = ev_tx.send(Ev::Done(turn));
                            continue;
                        }
                        let _ = ev_tx.send(Ev::Text(turn, text.clone()));
                        if TEXT_ONLY {
                            let _ = ev_tx.send(Ev::Done(turn));
                            continue;
                        }

                        if text.chars().count() < 2 {
                            let _ = ev_tx.send(Ev::Pcm(turn, prompts::AGAIN.to_vec()));
                            let _ = ev_tx.send(Ev::Done(turn));
                            continue;
                        }

                        let system = "你是语音助手，用一句话、口语化地回答，不超过 15 个字。";
                        let reply = match cloud.chat(config::DEEPSEEK_KEY, system, &text) {
                            Ok(r) => r.trim().to_string(),
                            Err(e) => {
                                println!("[net] chat failed: {e}");
                                let _ = ev_tx.send(Ev::Pcm(turn, prompts::NET.to_vec()));
                                let _ = ev_tx.send(Ev::Done(turn));
                                continue;
                            }
                        };
                        println!("[net] llm: {reply}");
                        if reply.is_empty() {
                            let _ = ev_tx.send(Ev::Pcm(turn, prompts::AGAIN.to_vec()));
                            let _ = ev_tx.send(Ev::Done(turn));
                            continue;
                        }
                        last_reply = reply.clone();

                        // 8 kHz 减半载荷；接收端把样本复制回 16 kHz。
                        let tx = ev_tx.clone();
                        let mut sink = |chunk: &[u8]| {
                            let mut out = Vec::with_capacity(chunk.len() * 2);
                            for s in chunk.chunks_exact(2) {
                                out.extend_from_slice(&[s[0], s[1], s[0], s[1]]);
                            }
                            let _ = tx.send(Ev::Pcm(turn, out));
                        };
                        // 先走流式 TTS：复用预热的 Dash 连接，第一块音频
                        // 到了就开始播，而不是等完整 JSON + OSS 往返。
                        if let Err(e) = cloud.tts_stream(&reply, 8000, &mut sink) {
                            println!("[net] tts stream failed ({e}); falling back");
                            if let Err(e) = cloud.tts_progressive(&reply, 8000, &mut sink) {
                                println!("[net] tts failed: {e}");
                                let _ = ev_tx.send(Ev::Pcm(turn, prompts::NET.to_vec()));
                            }
                        }
                        let _ = ev_tx.send(Ev::Done(turn));
                    }
                }
            }
        })
        .expect("net thread");

    (cmd_tx, ev_rx)
}

/// 判断 `text` 是否是设备上一条回答经麦克风漏回来的回声
///（这块板没有声学回声消除）。
/// 判定方式：识别文本是上一条回答的连续片段，或其大部分字符
/// 按大致相同的顺序出现在那条回答里。
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
    // 连续片段匹配，双向都算。
    let ts: String = t.iter().collect();
    let ls: String = l.iter().collect();
    if ls.contains(&ts) || ts.contains(&ls) {
        return true;
    }
    // 保序重合率。
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
