# ESP32-S3 语音助手（纯 Rust）

一块 1.54 寸屏的 ESP32-S3 AI 语音板，固件全用 Rust 写（no_std + esp-hal），
不用 ESP-IDF、不写一行 C/C++。语音识别和大模型放在上位机跑，板子只负责
录音、上传、播放、显示。

```
你说话 ──> 麦克风 ──I2S──> ESP32-S3 ──Wi-Fi/WebSocket──> 上位机网关
                                                          ├─ ASR   语音转文字
                                                          ├─ LLM   生成回答
                                                          └─ TTS   文字转语音
你听到 <── 喇叭 <──I2S── ESP32-S3 <──Wi-Fi/WebSocket──────┘
```

上位机可以是**电脑**，也可以是**安卓手机**（见第五节）。

## 目录

```
esp32s3-voice-assistant/
├── src/
│   ├── main.rs       主流程：初始化、任务编排、WebSocket 会话循环
│   ├── board.rs      ★ 所有引脚定义都在这里，换板子只改这个文件
│   ├── config.rs     ★ Wi-Fi 账号密码、网关 IP
│   ├── es8311.rs     ES8311 codec 驱动（I2C 寄存器序列）
│   ├── audio.rs      I2S 全双工 + 能量 VAD + 播放队列
│   ├── ws.rs         手写 WebSocket 客户端（握手 + 帧编解码）
│   ├── json.rs       极简 JSON 取值器（替代 serde，省几十 KB）
│   ├── sensors.rs    I2C 扫描 + QMI8658 IMU + 电容触摸
│   ├── sdcard.rs     TF 卡（SD 卡的 SPI 兼容模式）
│   └── display.rs    ST7789 240x240 状态表情
└── server/
    ├── gateway.py         网关主程序（ASR/TTS 都是可插拔 + 自动降级）
    ├── .env.example       配置模板
    ├── requirements.txt         基础依赖
    ├── requirements-local.txt   本地模型依赖（可选）
    ├── test_client.py     设备模拟器，无硬件也能验协议
    ├── test_local_asr.py  本地 ASR 闭环测试（TTS 造音频 → ASR 识别 → 比字错率）
    └── models/           本地模型（SenseVoice 229MB + Piper 63MB）
```

---

## 一、设备端配置

**`src/config.rs`** — 必须改，否则连不上：

```rust
pub const WIFI_SSID: &str = "你家的WiFi名";        // 必须 2.4GHz
pub const WIFI_PASSWORD: &str = "WiFi密码";
pub const GATEWAY_HOST: &str = "192.168.88.160";   // 上位机的局域网 IP
```

**`src/board.rs`** — 引脚。**已按卖家提供的 GPIO 表逐项落定**（2026-10-27），
不再是早期那套猜测值。完整引脚表、和旧版的差异、strapping 脚注意事项
都写在文件开头。烧进去先看串口 `[i2c] 扫描` 那几行：能扫到 `0x18`(ES8311)
就说明 I2C 对了，扫到的其余地址就是 IMU 和触摸芯片。

## 二、编译烧录

```bash
cd D:\esp32\esp32s3-voice-assistant
cargo run --release          # 编译 + 烧录 + 打开串口监视
```

固件 622KB（占 4MB app 分区的 15%）。

## 三、云端 API 还是本地模型？这一节决定你花多少钱

先说结论：**ASR 用本地**（和云端是同一个模型，更快更省），
**TTS 看你要音质还是要免费**，**LLM 只能用云端**。

### 各环节的「本地同款」对照

| 环节 | 你现在用的云端 | 最接近的本地项目 | 是同一个模型吗 | 实测结论 |
|---|---|---|---|---|
| **ASR** | 硅基流动 `SenseVoiceSmall` | **sherpa-onnx + SenseVoice int8** | ✅ **同权重、同模型** | 字错率 6%，**122ms**（云端约 400ms） |
| **TTS** | 硅基流动 `CosyVoice2-0.5B` | CosyVoice2 官方开源版 | ✅ 同权重 | ❌ 0.5B 参数，**没有 GPU 跑不动**（CPU 实时率 >2x，说话要等） |
| **TTS**（轻量） | — | **Piper VITS**（已装） | ❌ 不同模型 | 4.6s 音频合成 325ms，质量中等偏机械 |
| **TTS**（更好） | — | **Kokoro-82M**（已装） | ❌ 不同模型 | 3.9s 音频合成 1702ms，音质明显更好，但有 ~500ms 固定开销 |
| **LLM** | DeepSeek / Qwen 云端 | Ollama + Qwen2.5-7B | ❌ 差一到两个量级 | 能跑，但复杂问题明显不如云端 |

### 本地 TTS 的四个档位（都实测过）

| 方案 | 参数 | 硬件要求 | 19 字耗时 | 实时率 | 许可证 |
|---|---|---|---|---|---|
| **Piper** | ~20M | 纯 CPU | **325ms** | 0.07x | MIT |
| **Kokoro** | 82M | 纯 CPU | 1702ms | 0.45x | Apache 2.0 |
| CosyVoice2 | 0.5B | **GPU ≥4GB** | 150ms（流式） | — | Apache 2.0 |
| Qwen3-TTS | 0.6-1.7B | **GPU 2-6GB** | 97ms | — | Apache 2.0 |
| Fish Speech 1.5 | ~1B | GPU 4-6GB | 100ms | — | ⚠️ **非商用** |
| IndexTTS2 | 未公开 | GPU ≥8GB | — | — | ⚠️ 自定义协议 |

**结论分三种情况**：

- **没有独显**（你的情况）：Piper 和 Kokoro 二选一。Piper 快 6 倍但声音平，
  Kokoro 音质好一档但每句要多等半秒。`server/compare/` 里有五个音色的试听文件，自己听。
- **有一张 4GB+ 的 N 卡**：直接上 CosyVoice2 或 Qwen3-TTS，这才是真正"媲美 API"的
  ——注意它们本身就是硅基流动云端跑的那两个模型。
- **有 8GB+ 显存且要做情感配音**：IndexTTS2，但协议要单独确认。

**关于要不要为这个买显卡**：按每天 50 轮对话算，云端 TTS 一个月约 4 元。
一张能跑 CosyVoice2 的二手 3060 12G 大概 1500 元，回本要 30 年。
除非你还想跑本地 LLM 或 ComfyUI，否则**为省这 4 块钱买卡是纯亏**。

### 音质自己听（不用猜）

```bash
cd server/compare
# 1_piper_女.wav        当前默认
# 2_kokoro_女-小北.wav    Kokoro 女声
# 3_kokoro_女-晓晓.wav
# 4_kokoro_男-云希.wav
# 5_kokoro_男-云健.wav
```

同一句话：`北京今天天气怎么样？明天要出门的话记得带伞。`

听完想换 Kokoro，把 `.env` 改成：

```
TTS_CHAIN=kokoro
KOKORO_VOICE=zf_xiaobei     # 可选 zf_xiaobei/zf_xiaoxiao/zm_yunxi/zm_yunjian
```

想云端优先、Kokoro 兜底：`TTS_CHAIN=siliconflow,kokoro`

**关键点**：ASR 这一环不是「找了个差不多的开源替代」，而是
**同一个模型换了个地方跑**——硅基流动云端部署的是 FunAudioLLM/SenseVoiceSmall，
sherpa-onnx 跑的是同一份权重的 ONNX 量化版。所以识别质量基本一致，
而本地还多了三个好处：免费、离线、省掉网络往返。

TTS 这一环，CosyVoice2 虽然是同一个模型，但 0.5B 参数的推理成本摆在那，
没有 NVIDIA 显卡就只能忍受长延迟。所以本地退而求其次用 Piper。

### 本地 ASR 安装

```bash
# 1. 装依赖（纯 CPU，不需要 PyTorch，不需要 GPU）
pip install -r requirements-local.txt

# 2. 下模型（229MB 的 int8 版，中文/英文/日文/韩文/粤语）
#    国内直连 GitHub 慢，建议挂代理
cd server/models/asr
curl -L -o sv.tar.bz2 \
  https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2
tar -xjf sv.tar.bz2 && rm sv.tar.bz2

# 3. 验证
cd ../..
python test_local_asr.py
```

跑通后长这样：

```
原文                     识别结果                        字错率      耗时
------------------------------------------------------------------
你好小智                   你好，小芝。                      25%     82ms
北京今天天气怎么样              北京今天天气怎么样？                   0%    119ms
帮我定一个明天早上七点的闹钟         帮我定一个明天早上7点的闹钟。              7%    174ms
播放一首周杰伦的歌              播放一首周杰伦的歌。                   0%    133ms
现在几点了                  现在几点了？                       0%    102ms
------------------------------------------------------------------
平均字错率 6%，平均识别耗时 122ms
```

> 第一行那个 25% 不是识别错了，是 **Piper 把「小智」读成了「小芝」的音**。
> 真人说话不会这样。但它暴露了一个真问题：ASR 确实会把「小智」听成同音字，
> 所以唤醒词判断做了拼音兜底（见第九节）。

### 三种配置，按需选

在 `server/.env` 里改：

| 想要 | 配置 | ASR 成本 | TTS 成本 | 音质 |
|---|---|---|---|---|
| **全免费 + 离线** | `ASR_CHAIN=local`<br>`TTS_CHAIN=piper` | ¥0 | ¥0 | 中等 |
| **混合**（推荐） | `ASR_CHAIN=local`<br>`TTS_CHAIN=siliconflow,piper` | ¥0 | ¥4/月 | 好 |
| **手机上跑** | `ASR_CHAIN=local-lite`<br>`TTS_CHAIN=kokoro` | ¥0 | ¥0 | 好 |
| 全云端 | `ASR_CHAIN=siliconflow`<br>`TTS_CHAIN=siliconflow` | ¥0（免费额度） | ¥4/月 | 好 |

`local` 是 sherpa-onnx（C++，最快，电脑用），`local-lite` 是纯 Python 自实现（慢 13ms，手机用）。
默认链路 `local,local-lite,siliconflow` 会自动挑当前环境能用的那个——**不用手动改**。

两种模式都带**自动降级**：本地模型坏了自动切云端，云端挂了自动切本地，
不会突然变哑巴。只有**抛异常**才降级——识别结果为空不降级，
因为那意思是「这段没人说话」，是正常情况。

### 注册云服务（要用云端才需要）

1. https://cloud.siliconflow.cn 注册（手机号）
2. **完成实名认证**（2026-05 起强制）
3. 控制台 → API 密钥 → 新建 → 填进 `server/.env`

SenseVoice 识别免费，不充值也能用。

## 四、跑网关（电脑）

```bash
cd D:\esp32\esp32s3-voice-assistant\server
pip install -r requirements.txt
pip install -r requirements-local.txt    # 可选：要本地模型才装
copy .env.example .env                   # 编辑 .env，至少改 ASR_CHAIN / TTS_CHAIN

python gateway.py --check    # ① 检查：不花钱，会实测 Piper + 本地 ASR 闭环 + LLM
python gateway.py --text     # ② 文本调试：打字测 LLM+TTS，音频存 out.wav
python gateway.py            # ③ 正式服务
```

`--check` 输出示例：

```
ASR : local/SenseVoice(int8) → siliconflow/SenseVoiceSmall
LLM : siliconflow
TTS : piper/zh_CN-huayan

[配置]
  SILICONFLOW_API_KEY : 未配置 ← 云端 ASR/TTS 备份，可选
  Piper 模型          : 存在
  本地 SenseVoice     : 存在

[实测 Piper 本地合成]
  生成 2.15s 音频，耗时 3315ms（含首次加载模型）

[实测本地 SenseVoice 闭环]
  原文：测试一下本地语音合成
  识别：测试一下本地语音合成。（1895ms）
```

> **模型名要注意**：硅基流动的 LLM 模型 ID 会变。`--check` 报 HTTP 400 就是它，
> 去「模型广场」复制准确 ID 填进 `SF_LLM_MODEL`。

### 不用硬件验证整条链路

```bash
# 多轮对话，第一句是唤醒词（用本地 Piper 合成真实中文语音发过去）
python test_client.py --voice "你好小智" "北京今天天气怎么样"
```

跑通输出：

```
── 第 1 轮：你好小智 ──
  <- {"type":"stt","text":"你好，小芝。"}    ← 注意是同音字，但唤醒成功
  <- {"type":"ack"}
── 第 2 轮：北京今天天气怎么样 ──
  <- {"type":"stt","text":"北京今天天气怎么样？"}
✅ 完整链路通了：语音 → 识别 → 唤醒 → 回答 → 合成 → 下发
```

网关侧对应日志（ASR 是热态耗时）：

```
[asr] local/SenseVoice(int8) 59ms
[asr] 你好，小芝。
[wake] 唤醒成功
[tts] piper/zh_CN-huayan：2.0s 音频，92ms
[asr] local/SenseVoice(int8) 113ms
[asr] 北京今天天气怎么样？
```

Windows 防火墙第一次会弹窗，**选允许**：

```
netsh advfirewall firewall add rule name="voicegw" dir=in action=allow protocol=TCP localport=8765
```

## 五、跑网关（安卓手机）

电脑不常开的话，手机当上位机最方便——随身、省电、还能开热点让板子连。

**为什么不用原生 App**：这套网关是纯 Python。写个 Kotlin App 等于把它翻译一遍，
还得维护两份代码。用 Termux 直接跑同一份 `gateway.py`，改一个文件两边都生效。

```bash
# 1. 装 Termux（从 F-Droid 下，Google Play 版已停更）
#    https://f-droid.org/packages/com.termux/

pkg update && pkg install python -y
pip install aiohttp websockets numpy pypinyin      # 只装基础依赖

# 2. 把 server 目录传到手机，然后
cd ~/server && cp .env.example .env
nano .env        # ASR_CHAIN=siliconflow（手机装不了本地 ASR）
                 # TTS_CHAIN 按手机档次决定，见下面「手机上能跑哪些本地模型」

python gateway.py
```

### 手机上必须做的三件事

1. **`termux-wake-lock`** —— 不加这个，锁屏几分钟服务就被挂起，板子表现为"连不上"
2. **关电池优化** —— 设置 → 应用 → Termux → 电池 → 无限制
   （小米叫「省电策略-无限制」，华为叫「应用启动管理-手动管理」）
3. **手机开热点给板子** —— 手机 IP 通常是 `192.168.43.1`（部分机型 `192.168.137.1`），
   改到 `src/config.rs` 的 `GATEWAY_HOST`，WiFi 名改成热点名

### 手机上到底能跑哪些本地模型

查了 Termux 官方仓库（当前 3014 个包），结论是**分能力看，不是一刀切**：

| 部分 | 手机可行性 | 原因 |
|---|---|---|
| **SenseVoice ASR** | ✅ **能跑** | 用 `sensevoice_lite.py`（纯 Python 自实现），只依赖 onnxruntime + numpy |
| **Kokoro TTS** | ✅ **能跑** | 纯 Python + onnxruntime，中文前端 misaki[zh] 走 jieba+pypinyin，不依赖 espeak |
| Piper TTS | ⚠️ 装不上 | `piper-tts` 的 phonemize 是 manylinux 预编译包，Termux 用 bionic libc，不兼容 |
| 云端 ASR/TTS | ✅ | 免费额度内零成本 |

手机上安装：

```bash
pkg install python python-onnxruntime python-numpy
pip install kokoro-onnx "misaki[zh]"
```

### SenseVoice 在手机上怎么跑起来的

`sherpa-onnx` 的 Python 包是 manylinux wheel，Termux 装不上——但**模型本身是开源的 ONNX 文件**，
而且前处理参数全写在模型元数据里，所以这里自己重新实现了一遍推理，见 `sensevoice_lite.py`：

```
16kHz PCM
  → kaldi fbank（80 维，hamming，25ms/10ms，snip_edges）
  → LFR（7 帧堆叠，步长 6）→ 560 维
  → CMVN（读模型元数据里的 neg_mean / inv_stddev）
  → onnxruntime 推理 → CTC logits [T, 25055]
  → 贪心解码 + 去重去 blank → tokens.txt 反查
```

全部用 numpy 实现，**不需要 sherpa-onnx、不需要 PyTorch、不需要任何 C 扩展**。

实测和 sherpa-onnx 的结果**逐字一致**（5 个官方测试音频 4 个完全相同，
日文那个只差一个逗号），只是慢 13ms：

```
            自实现      sherpa-onnx
平均耗时     73ms         60ms      （5 句中文，4 核 CPU）
```

这 13ms 就是 Python 算 fbank 的开销，手机上 CPU 更强时占比更小。

### 手机上跑 Kokoro 快不快

参考 VoxSherpa（Google Play 上的离线 TTS App，同样基于 sherpa-onnx）公布的实测：

| 手机档位 | Kokoro-82M | Piper/VITS |
|---|---|---|
| 旗舰（骁龙 8 Gen 3） | 20–40 秒 / 每分钟音频（0.33–0.67x） | 5 秒 / 分钟（0.08x） |
| 中端 8 核 | 60–90 秒 / 分钟（1.0–1.5x，偏慢） | 10 秒 / 分钟（0.17x） |
| 低端 6 核 | 2–3 分钟 / 分钟（不可用） | 20 秒 / 分钟（0.33x） |

**旗舰机和 PC 的四核差不多快**（我在电脑上实测 Kokoro 是 0.45x，正好落在这个区间）。
中低端机就老实用云端 TTS。

所以手机上的推荐组合：**云端 ASR（免费额度）+ 本地 Kokoro TTS**，
每月成本压到 0，且 TTS 完全离线。

### 顺带：不写代码也能用离线 TTS

`VoxSherpa TTS`（Google Play，GPL v3）可以直接设成安卓系统的 TTS 引擎，
Chrome、微信、任何 App 里的朗读都会走它，支持 Kokoro/Piper/VITS、中文、完全离线。
缺点是它不能把音频喂给我们的 ESP32 网关——要用在这条链路上还是得走 Termux。

## 五之二、做成 App（可选，比 Termux 稳）

Termux 方案够用，但有个硬伤：**后台随时可能被系统杀掉**。想要「装上去就能一直跑」，
`android/` 目录里是一套完整的 Android 工程，做成了正式 App：

| | Termux 方案 | App 方案 |
|---|---|---|
| 装法 | 装 Termux + pip | 装 APK |
| 后台稳定性 | 容易被杀 | 前台服务 + 通知，稳 |
| 截图/界面 | 命令行 | 有界面（启停、日志、配置） |
| 模型 | 放任意目录 | 放 `/sdcard/Android/data/com.dosgo.voicegw/files/` |

**协议完全一致**，固件不用改，只是把 `GATEWAY_HOST` 换成手机 IP。

```
android/
├── app/libs/sherpa-onnx.aar     官方 AAR（47MB，含 arm64 so）
├── app/src/main/java/com/dosgo/voicegw/
│   ├── MainActivity.kt          主界面（启停 / 配置 / 日志）
│   ├── TestActivity.kt          本地测试页（不用板子）
│   ├── GatewayService.kt        前台服务（保活）
│   ├── WsServer.kt              WebSocket + 会话状态机
│   ├── Models.kt                模型单例（全 App 只加载一份 229MB）
│   ├── Asr.kt                   SenseVoice
│   ├── Tts.kt                   VITS 中文
│   └── Llm.kt                   云端大模型
└── models/vits-icefall-zh-aishell3/   TTS 模型
```

### 装好后先别接板子，用测试页自测

App 里内置了一个**本地测试页**（主界面 →「本地测试（不用板子）」），
用手机麦克风和喇叭把整条链路跑一遍，不依赖 ESP32：

| 功能 | 用途 |
|---|---|
| ① 检查模型文件 | 清单 + 体积 + 加载耗时，缺哪个文件直接标出来 |
| ② 只识别 / **全链路** | 说一句 → ASR；全链路会一路跑到「听到回答」 |
| ③ 合成播放 | 输入文字 → TTS → 手机喇叭 |
| ④ 大模型 | 验证 Key / 地址 / 模型 ID 通不通 |

全链路跑完会打印分段耗时，一眼看出慢在哪：

```
① 识别 118ms → 北京今天天气怎么样
② 大模型 726ms → 北京今天多云转晴，气温十五到二十三度。
③ 合成 342ms，3.84s 音频
```

先确认这三段都通，再去调 ESP32。这样板子出问题时，能立刻分清是**设备端**还是**上位机**。

详细步骤（编译、放模型、adb push 命令、排错）见 **`android/README.md`**。

**编译状态：已实测通过。** `gradlew.bat assembleDebug` → `BUILD SUCCESSFUL`，
产出 16MB 的 `app-debug.apk`，17 个单元测试全过，lint 0 error。

```bat
cd D:\esp32\esp32s3-voice-assistant\android
set JAVA_HOME=C:\Program Files\Android\Android Studio\jbr
gradlew.bat assembleDebug
deploy.bat             :: 插上手机，装 APK + 推模型一步到位
```

**模型也可以不用电脑**：只装 APK（`deploy.bat apk`），然后进 App 的
「本地测试 → ⑤ 下载模型」点一下，手机自己下 268MB（默认走 `hf-mirror.com`
国内镜像，支持断点续传，也可填自己的地址）。

工具链用的是本机实际装的：SDK Platform 37 + build-tools 36 + JDK 25（Studio 自带 JBR）
+ Gradle 9.3.1 + AGP 8.13.0 + Kotlin 2.4.21。

> 唯一没验证的是**真机运行**——本机没有连接的设备，也没有 arm64 的模拟器镜像。
> 代码能编译、能打包、核心逻辑有测试覆盖，但 ASR/TTS 实际出声要你在手机上试。

## 六、怎么用

1. 上电 → 屏幕 `READY`，闭眼表情
2. 说 **「你好小智」** → 屏幕变 `LISTENING`，睁眼
3. 听到「我在」→ 接着说话
4. 90 秒不交互自动回待机

## 七、协议

WebSocket，文本帧走 JSON，二进制帧走 PCM。和供应商完全解耦——
换 ASR、换 LLM、换 TTS 都不影响固件。

**握手**
```json
{"type":"hello","version":1,"format":"pcm","sample_rate":16000,"channels":1,"bits":16,"frame_ms":60}
```

**上行**：每帧 1920 字节 16kHz/16bit/单声道 PCM（60ms）
**下行**：同样的 PCM，每帧 ≤1920 字节，按 60ms 节奏发

**下行 JSON**
```json
{"type":"stt","text":"识别出来的话"}
{"type":"llm","text":"大模型的回答"}
{"type":"tts","state":"start"}
{"type":"tts","state":"stop"}
{"type":"ack"}
{"type":"error","msg":"..."}
```

## 八、排错

| 现象 | 原因 | 怎么查 |
|---|---|---|
| `[i2c] 一个设备都没有` | SDA/SCL 引脚不对 | 改 `board.rs` 的 `PIN_I2C_*` |
| 屏幕全黑 | 背光脚没拉高，或 SPI 引脚不对 | 先确认 `PIN_LCD_BL` |
| 屏幕白屏/花屏 | 不是 ST7789，或 offset 不对 | 240x240 一般要 `offset(0,80)` |
| 没声音 | DOUT/DIN 接反 | PA 现在常开，重点查这两根数据线 |
| 有声音全是杂音 | I2S 格式不匹配 | ES8311 的 SDP 寄存器，本驱动按标准 I2S 16bit |
| 说话没反应 | 麦克风没数据 | 串口有没有 `[vad] 检测到人声` |
| `[ws] 握手被拒` | 网关没开 / IP 写错 / 防火墙 | `python test_client.py` 试 |
| `--check` 报 HTTP 400 | LLM 模型 ID 不对 | 去模型广场复制，填 `SF_LLM_MODEL` |
| 本地 ASR 首次很慢 | 模型加载 | 正常，1-2 秒，之后都是 100ms 级 |
| 手机连不上 | Termux 被挂起 | `termux-wake-lock` + 关电池优化 |
| 唤醒不灵 | 同音字没匹配上 | 看日志 `[asr]` 出来的是什么，往 `WAKE_WORDS` 里加变体 |

## 九、唤醒词

用「**能量门控 + 服务端识别**」，不是本地唤醒词芯片方案。

- 设备端：连续 3 帧（180ms）能量超过自适应噪声阈值就开始上传，带 2 帧 pre-roll 不丢字头
- 网关：ASR 出文字后判断唤醒词

**同音容错**：实测 ASR 会把「小智」识别成「小芝 / 小志 / 晓智」，
纯字符串匹配会失效。所以除了精确匹配，还做了拼音兜底：

```python
pos = "".join(lazy_pinyin(text)).find("xiaozhi")
return 0 <= pos <= 8          # 必须出现在句首附近
```

配合「整句不超过 12 字」的限制，做到：
- ✅ 「你好小芝」「晓智你在吗」「小志小志」都能唤醒
- ❌ 「今天天气不错」「我在学习小知识」不会误唤醒

**为什么不用 ESP-SR 的 WakeNet**：闭源 C 库，必须挂 ESP-IDF 的分区表和组件系统，
一旦引入整个「纯 Rust」架构就废了，还要吃几 MB flash 放声学模型。
现在的方案代价是待机时持续上传静音帧（32KB/s，局域网无所谓），换来零 C 依赖。

要真离线唤醒，路线是 `tract`（纯 Rust 推理）+ microWakeWord 的 tflite 模型，那是另一个工程。

## 十、已知限制

- **没有 AEC**：播放时会听到自己的声音被麦克风收进去。现在靠「Speaking 状态不上传」
  规避，所以不能打断式对话（必须等它说完）
- **单轮唤醒**：90 秒不交互要重新喊唤醒词
- **VAD 是纯能量阈值**：嘈杂环境容易误触发，没有机器学习端点检测
- **CosyVoice2 本地不可用**：没有 GPU，0.5B 参数跑不到实时
- **阿里云 ASR/TTS 未接入**：它的接口是 WebSocket 流式，和现在的「整段上传」模型不同
