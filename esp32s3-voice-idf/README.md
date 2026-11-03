# ESP32-S3 语音助手（ESP-IDF + std Rust + 官方 esp-sr）

独立工程，基于 ESP-IDF v5.4 与 std Rust（esp-idf-svc / esp-idf-hal），不修改 esp-sr 组件源码，全部通过 bindgen 绑定官方头文件。

硬件：Waveshare ESP32-S3-Touch-LCD-1.54（ES7210 四通道 ADC + ES8311 DAC + NS4150B 功放 + ST7789 屏）。

## 架构

音频与网络彻底分离，音频线程一个阻塞调用都没有：

```text
主线程（音频，永不阻塞）          网络线程（net.rs）
  I2S 读麦克风                      唤醒时开识别任务（WSS run-task）
  WakeNet 唤醒词                    持续推送 PCM（input_audio_buffer）
  VAD 断句                          末句 finish-task → 取文字
  状态机                            qwen / DeepSeek 生成回答
  I2S 播放队列                      TTS 8 kHz 流式下载 → 事件回传
        └──── Cmd::Wake/Audio/End ──→        ←──── Ev::Text/Pcm/Done ────┘
```

状态机：`Idle → Greeting(播「我在」) → Listening → Thinking → Speaking → FollowUp`

每步都有兜底：唤醒播放期间不采集、播放后再等 0.3 s 静音才听；`net_pending` 30 s 超时自动复位，网络卡住不会导致设备“装死”。

## 云端服务

| 能力 | 服务 | 说明 |
|---|---|---|
| ASR | 阿里云百炼 `qwen-audio-3.1-asr-flash-streaming` | WebSocket 流式，边说边传 |
| LLM | `qwen3.8-flash`（DeepSeek 不通时兜底） | 回复限制 15 字 |
| TTS | 百炼 `qwen3-tts-flash` | 请求 8 kHz，播放时补点回 16 kHz |

业务空间域名与 API Key 在 `src/cloud.rs` 顶部；WiFi 与 DeepSeek Key 在 `src/config.rs`（已 gitignore，从 `config.example.rs` 复制）。

## 关键实现点

- **esp-sr VAD 必须喂 10/20/30 ms 帧**：I2S 块是 32 ms（512 采样），`vad.rs` 里做缓冲后按 30 ms 连续投喂，否则判决漂移、断句不准；
- **TLS 缓冲**：WebSocket 与 HTTPS 两条会话来争内部 RAM，`sdkconfig.defaults` 把大块分配推到 PSRAM（阈值 2 KB），TLS 输入缓冲 8 KB；
- **DNS**：部分热点自带的解析器看不到阿里云域名，开机与重连前会检测并回退到公共 DNS；
- **本地提示音**：`assets/` 下 5 句 piper 预生成 PCM，断网、没听清时直接播放。

## 构建与烧录

```powershell
# 需要 ESP-IDF v5.4（ESP_IDF_PATH）与 xtensa-esp32s3-espidf 工具链
.\activate.ps1          # 设置 EXTRA_COMPONENTS_FILE / SDKCONFIG_DEFAULTS
cargo build --release
.\flash.bat COM4        # 编译 + 烧录 + 写入 srmodels.bin 到 0x410000
```

分区：factory 4 MB @0x10000，模型 4 MB @0x410000。

## 当前局限

- 一轮对话约 6~10 秒（TLS 握手 + 云端推理 + 8 kHz 音频下载，受设备 WiFi 吞吐限制）；
- 屏幕字体只有 ASCII，识别出的中文只在串口日志里，不显示在屏上；
- AEC 目前未启用（实测 ERLE ≈ 0），播放时不要同时说话。

## 目录

```text
src/main.rs        音频线程：I2S / 麦克风 / VAD / 状态机
src/net.rs         网络线程：识别 + 大模型 + 合成的调度
src/asr_ws.rs      百炼流式识别 WebSocket 封装
src/cloud.rs       HTTPS：LLM、TTS、DNS
src/vad.rs         esp-sr VAD 安全封装（30 ms 连续投喂）
src/wakenet.rs     WakeNet 安全封装
src/prompts.rs     固化在固件的本地提示音
src/link.rs        旧的网关（WebSocket）传输实现，保留备查
src/aec.rs         esp-sr AEC 封装（当前未启用）
```
