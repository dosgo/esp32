# 语音网关 App（Android）

手机版的上位机。ESP32 连上这个 App 的 WebSocket，就能用手机跑完
「识别 → 大模型 → 合成」整条链路，不需要电脑。

**协议和 Python 版 `server/gateway.py` 完全一致**，所以固件一行都不用改，
只要把 `src/config.rs` 里的 `GATEWAY_HOST` 改成手机 IP。

```
ESP32-S3 ──WiFi──> 手机（本 App）
                     ├─ WebSocket 服务   0.0.0.0:8765
                     ├─ ASR  sherpa-onnx + SenseVoice（本地）
                     ├─ LLM  云端 API
                     └─ TTS  sherpa-onnx + VITS 中文（本地）
```

> **本工程已在本机实测编译通过**：`BUILD SUCCESSFUL`，产出 `app-debug.apk` 16MB，
> 17 个单元测试全过，lint 0 error。用的工具链见下表。

## 目录

```
android/
├── gradlew / gradlew.bat        Gradle wrapper（9.3.1）
├── deploy.bat                   一键：装 APK + 推模型
├── app/
│   ├── libs/sherpa-onnx.aar     官方 AAR（47MB，含 arm64 的 so）
│   └── src/
│       ├── main/java/com/dosgo/voicegw/
│       │   ├── MainActivity.kt      界面：启停、配置、日志
│       │   ├── TestActivity.kt      本地测试页（不用板子）
│       │   ├── GatewayService.kt    前台服务（保活的关键）
│       │   ├── WsServer.kt          WebSocket 服务端 + 会话状态机
│       │   ├── Models.kt            模型单例（全 App 只加载一份）
│       │   ├── ModelDownloader.kt   App 内下载模型（多源 + 续传）
│       │   ├── HttpDownloader.kt    下载实现（纯 JVM，可单测）
│       │   ├── Asr.kt               SenseVoice 识别
│       │   ├── Tts.kt               VITS 中文合成
│       │   ├── Llm.kt               云端大模型（OpenAI 兼容接口）
│       │   ├── AudioPcm.kt          PCM 转换 / 重采样 / RMS（纯 Kotlin，可测）
│       │   └── Protocol.kt          协议常量 / JSON 工具
│       └── test/java/com/dosgo/voicegw/
│           └── ProtocolTest.kt      17 个单元测试，不需要真机
└── models/
    └── vits-icefall-zh-aishell3/   TTS 模型 + 文本规则（拷到手机用）
```

## 一、编译

### 本机环境（已实测可用）

| 组件 | 版本 | 位置 |
|---|---|---|
| Android SDK | Platform **android-37**（Android 17）、build-tools 36.0.0 | `C:\Users\DosGo\AppData\Local\Android\Sdk` |
| JDK | **25.0.3**（Android Studio 自带 JBR） | `C:\Program Files\Android\Android Studio\jbr` |
| Gradle | **9.3.1** | wrapper 自动下载（本机已缓存） |
| AGP | **8.13.0** | Gradle 缓存 |
| Kotlin | **2.4.21** | Gradle 缓存 |

> 版本是照着本机实际装的配的，不是猜的。`gradle.properties` 里
> `android.suppressUnsupportedCompileSdk=37,37.0` 是因为 AGP 8.13 官方只到 API 36，
> 但本机只装了 37 的平台，所以显式声明一下（只是消警告，不影响产物）。

### 命令行编译（推荐，最快）

```bat
cd D:\esp32\esp32s3-voice-assistant\android
set JAVA_HOME=C:\Program Files\Android\Android Studio\jbr
gradlew.bat assembleDebug
```

产物：`app\build\outputs\apk\debug\app-debug.apk`（16MB）

跑单元测试（不用手机）：

```bat
gradlew.bat testDebugUnitTest
```

### Android Studio

1. **Open** → 选 `android` 目录（不是仓库根目录）
2. 等 Gradle Sync 完成（依赖已在缓存里，通常 1 分钟内）
3. 手机开 USB 调试插上，点 ▶

## 二、部署（装 APK + 放模型）

模型有两条路进手机，任选一条：

| 方式 | 需要电脑 | 需要网络 | 说明 |
|---|---|---|---|
| **App 内下载** | ❌ | ✅ | 装好 APK 后**主界面**点「下载模型（约 268MB）」 |
| adb push | ✅ | ❌ | `deploy.bat` 一键，局域网/数据线最快 |

### 方式 A：App 内直接下载（不用电脑）

```bat
deploy.bat apk        :: 只装 APK，模型在手机上自己下
```

装好后打开 App，**主界面最上面**就有一个「下载模型（约 xxxMB）」按钮，
点它就行。按钮下面有进度条和 `【1/9】model.int8.onnx 37.2/218.4MB 1.8MB/s 剩余 2:10`。

想换源或看每个文件的明细，再去「本地测试 → ⑤ 下载模型（可换源 / 看明细）」。

- **默认源 `hf-mirror.com`**（国内镜像），失败自动换 `huggingface.co`
- 可以填**自定义源**：家里 NAS、自建服务器、局域网电脑都行，格式就是 `https://xxx.com`
- **支持断点续传**：下到 180MB 断网，下次从 180MB 接着下，不会从零开始
- 下完自动校验长度、自动重新加载并跑一次验证

> 手机没开代理时 `huggingface.co` 大概率连不上，所以默认走镜像。
> 如果两个源都不通，把地址填成你自己能访问的服务器（把仓库文件按
> `<源>/<仓库>/resolve/main/<文件名>` 的路径放好即可）。

### 方式 B：adb push（有电脑时最快）

直接用脚本，两步合一：

```bat
cd D:\esp32\esp32s3-voice-assistant\android
deploy.bat
```

脚本会做：检查设备 → `adb install -r` → 创建目录 → 推全部模型。
只想推模型（换了模型时）：`deploy.bat models`；只想装 APK：`deploy.bat apk`。

### 手动部署

模型不打包进 APK，要放到 App 的私有目录：

```
/sdcard/Android/data/com.dosgo.voicegw/files/
├── asr/
│   ├── model.int8.onnx      ← 229MB
│   └── tokens.txt
└── tts/
    ├── model.onnx           ← 30MB
    ├── tokens.txt
    ├── lexicon.txt          ← 2MB
    ├── phone.fst            ← 规则文件，不能少
    ├── date.fst
    ├── number.fst
    └── new_heteronym.fst
```

**那 4 个 `.fst` 是必需的。** 官方示例里 `--tts-rule-fsts='phone.fst,date.fst,number.fst'`，
不挂的话「7点」「3个」「13800138000」这类数字/日期/电话会读错甚至整段跳过。
它们很小（合计 230KB），但漏了声音就会表现得很奇怪。

同目录的 `rule.far`（173MB）**不用拷**，那是另一套规则，手机上太占地方。

```bash
adb shell mkdir -p /sdcard/Android/data/com.dosgo.voicegw/files/asr
adb shell mkdir -p /sdcard/Android/data/com.dosgo.voicegw/files/tts

# ASR（和电脑上跑的是同一份模型）
adb push "...\server\models\asr\sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17\model.int8.onnx" /sdcard/Android/data/com.dosgo.voicegw/files/asr/
adb push "...\server\models\asr\sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17\tokens.txt"      /sdcard/Android/data/com.dosgo.voicegw/files/asr/

# TTS
adb push "...\android\models\vits-icefall-zh-aishell3\model.onnx"         /sdcard/Android/data/com.dosgo.voicegw/files/tts/
adb push "...\android\models\vits-icefall-zh-aishell3\tokens.txt"         /sdcard/Android/data/com.dosgo.voicegw/files/tts/
adb push "...\android\models\vits-icefall-zh-aishell3\lexicon.txt"        /sdcard/Android/data/com.dosgo.voicegw/files/tts/
adb push "...\android\models\vits-icefall-zh-aishell3\phone.fst"          /sdcard/Android/data/com.dosgo.voicegw/files/tts/
adb push "...\android\models\vits-icefall-zh-aishell3\date.fst"           /sdcard/Android/data/com.dosgo.voicegw/files/tts/
adb push "...\android\models\vits-icefall-zh-aishell3\number.fst"         /sdcard/Android/data/com.dosgo.voicegw/files/tts/
adb push "...\android\models\vits-icefall-zh-aishell3\new_heteronym.fst"  /sdcard/Android/data/com.dosgo.voicegw/files/tts/
```

> App 必须**先运行过一次**，Android 才会创建 `/sdcard/Android/data/com.dosgo.voicegw/files`。
> 顺序：装 APK → 打开一次（会提示模型缺失）→ 再推模型。

## 三、用

1. 打开 App，填大模型配置：
   - API Key：硅基流动的 `sk-...`
   - 接口地址：`https://api.siliconflow.cn/v1`
   - 模型 ID：`Qwen/Qwen2.5-7B-Instruct`（去模型广场确认当前有效的 ID）
   - 点保存
2. 点 **启动网关**，等日志出现 `已启动 ws://192.168.x.x:8765`
3. 点 **免电池优化**，把这 App 加进白名单
4. 固件里把 `GATEWAY_HOST` 改成上面这个 IP
5. 对着板子说「你好小智」

日志里 `hello` 那一行会顺手校验格式，正常长这样：

```
hello: pcm 16000Hz/16bit/60ms 一帧
```

如果不是，会直接打警告告诉你固件哪一项对不上——这比听到一堆杂音再回来猜要省事。

## 四、本地测试页（不用板子）

主界面点 **「本地测试（不用板子）」**，进去是一个自测页面。板子还没到、或者板子连不上时，
用它把问题分层定位——不用先怀疑「是不是 ESP32 那边的问题」。

页面上有四块：

| 功能 | 验证什么 | 失败说明什么 |
|---|---|---|
| ① 检查模型文件 | 清单 + 体积 + 加载耗时 | 哪个文件 ❌ 就是缺哪个 |
| ② 录音识别 / **全链路** | 手机麦克风 → ASR（全链路再接大模型 + TTS + 播放） | 识别为空 = 麦克风/权限/模型 |
| ③ 合成播放 | TTS → 手机喇叭 | 没声音 = TTS 模型或规则文件 |
| ④ 大模型 | 用主界面保存的配置发一句「你好」 | 出错 = Key/地址/模型 ID |
| ⑤ 下载模型 | 同主界面按钮，但**可填自定义源**、逐文件看明细 | 两个源都不通就填自定义地址 |

**② 里两个按钮的区别**：`只识别` 只看 ASR 结果；`全链路（听到回答）` 一次跑完
「录音 → 识别 → 大模型 → 合成 → 播放」，等于把 ESP32 那条链路在手机上完整走一遍。
跑完会打印分段耗时：

```
① 识别 118ms → 北京今天天气怎么样
② 大模型 726ms → 北京今天多云转晴，气温十五到二十三度。
③ 合成 342ms，3.84s 音频
```

录音时那行「音量：🔊 3821（门限 500）」很有用：说话时数字应该明显超过 500。
如果一直很低，是麦克风被挡、或者手机壳/贴膜堵了孔——这种问题换模型是解决不了的。

### 为什么必须做模型单例

测试页和网关服务都要用 ASR/TTS。SenseVoice 的 int8 模型 **229MB**，
如果两边各加载一份，光模型 460MB+，中低端机直接 OOM 被杀。

所以 `Models.kt` 是个惰性单例：谁先要谁触发加载，后来者拿同一个对象。
先开测试页再启动网关（或反过来），内存里始终只有一份。

## 五、必须知道的三件事

1. **别在任务列表里划掉这个 App**——服务会被杀，通知栏那条「语音网关」要留着
2. **手机开热点给板子用**时，手机 IP 通常是 `192.168.43.1`（部分机型 `192.168.137.1`）
3. **首次启动慢 3-5 秒**：要加载两个模型（ASR 229MB + TTS 30MB），日志里能看到进度

## 六、排错

| 现象 | 原因 |
|---|---|
| 日志「ASR 模型未加载」 | `asr/` 里缺 `model.int8.onnx` 或 `tokens.txt`，看日志里打印的路径 |
| 日志「TTS 模型未加载」 | `tts/` 里缺三个主文件，注意 `lexicon.txt` 容易被漏掉 |
| 报错里有 `rule fst` / 数字读成乱码 | 少了 `phone.fst` / `date.fst` / `number.fst` |
| 板子连不上 | 手机和板子不在同一网段；或热点隔离；先看 App 日志有没有「设备接入」 |
| 日志「⚠️ 格式不匹配」 | 固件采样率/帧长和 App 不一致，改 `src/config.rs` 或 `board.rs` |
| 有识别但没声音 | 看日志有没有「合成 x.x 秒音频」，没有就是 TTS 模型问题 |
| 服务几分钟后被系统杀 | 没做免电池优化，或被 ROM 省电策略限制 |
| **点启动就闪退、没有任何提示** | native 崩了。主界面点「运行日志」，看最后一行停在哪一步（见下） |
| 日志停在「加载 ASR」且 logcat 有 `assetManager is NOT set to null` | 已修：模型在 SD 卡上必须给 sherpa-onnx 传 `null` 的 AssetManager，见下 |
| 安装报 `INSTALL_FAILED_UPDATE_INCOMPATIBLE` | 签名变了，`adb uninstall com.dosgo.voicegw` 后重装 |
| 编译报 `Unresolved reference: OfflineTts` | AAR 没加载，检查 `app/libs/sherpa-onnx.aar` |
| 编译报 JDK / Gradle 版本 | `JAVA_HOME` 要指向 Android Studio 的 `jbr`（JDK 25），Gradle 用 9.x |

### TTS 音色：187 个，其中 151 个女声

之前默认那套 VITS 是 **8kHz** 训练的（电话音质），采样率训练时定死，参数怎么调都上不去。
现在默认换成 **fanchen-C**：187 个中文音色、16kHz、121MB。

| 引擎 | 目录 | 采样率 | 音色 | 体积 | 实测（Redmi 2602BRT18C） |
|---|---|---|---|---|---|
| **fanchen-C（默认）** | `tts/fanchen/` | 16kHz | **187（151 女声）** | 121MB | 3.6s 音频，首块 **2689ms** |
| VITS-LL | `tts/vits/` | 16kHz | 5 | 121MB | 3.2s 音频，首块 1742ms |
| Kokoro | `tts/kokoro/` | **24kHz** | 54 | 340MB | 4.56s 音频，**7785ms（1.71x，太慢）** |
| 旧 VITS | `tts/legacy-8k/` | 8kHz | 174 | 39MB | 音质最差 |

#### 女声是怎么分出来的

187 个音色没人愿意一个个试。这里的做法是**在 PC 上先量一遍**：

```
每个 sid 合成同一句话 → 自相关估基频（25ms 窗，70~300Hz）取中位数
→ ≥165Hz 判为女声
```

结果：**151 个女声、36 个男声**，按音高从低到高排序，写进
`app/src/main/assets/female_voices.txt`（格式 `sid,f0`）。

测试页 ③ 区三种方式挑音色，随便用：

| 控件 | 作用 |
|---|---|
| **下拉框** | 列出 151 个女声，标签 `43. sid=128 · 239Hz`，点开直接选 |
| **◀ 上一个 / 下一个 ▶** | 逐个翻，翻到就自动合成一遍直接听 |
| 下面那行状态 | `女声 120/151 · sid=170 · 基频 248Hz · vits 16kHz 187音色` |

选中的音色写进设置（`ttsSpeaker`），网关合成时读同一个值，**换音色不用重启服务**。

### 单独下载音色模型

③ 区底部有「下载音色模型」按钮，只下 TTS 那 6 个文件（约 121MB），
不动已经加载好的 ASR：

- 缺文件时显示 `下载音色模型（6 个文件，约 119MB）`
- 齐了显示 `音色模型已就位（点一下重新检查）`
- 下载中显示进度条 + `【1/6】model.onnx 37.2/115.6MB`
- 下完只 `Models.invalidateTts()` 重载 TTS，ASR 保持加载（不会白等一次 229MB）

主界面那个「下载模型」是 ASR + TTS 全下；只想换音色用这里的按钮就够。

试听：`android/compare/女声1_sid72_172Hz.wav` ~ `女声6_sid39_284Hz.wav`（从低到高 6 档）。

### 流式合成：首块出声

手机上 16kHz 模型约 1.0x 实时，整句算完再播要等 3 秒。现在用
`OfflineTts.generateWithCallback` **边算边发**，实测：

```
合成 3.0s 音频（vits 16kHz 5音色 音色 0，首块 1742ms）
```

等待从「整句 3s」降到「第一块 1.7s」。发送端按「已发音频时长」节流（最多领先播放 200ms），
既不会把设备端 8 帧队列撑爆，也不会插静音。

### 已知坑：JNI 回调不能用 Kotlin lambda

`sherpa-onnx` 的 native 层是这样找流式回调的：

```c
GetMethodID(cls, "invoke", "([F)Ljava/lang/Integer;")
```

它要求类上真的有一个 `java.lang.Integer invoke(float[])` 方法。而 **Kotlin lambda 编译出来是
`invoke(Object): Object`**，签名对不上，native 直接：

```
JNI DETECTED ERROR IN APPLICATION: JNI NewFloatArray called with pending exception
java.lang.NoSuchMethodError: no non-static method "...$$ExternalSyntheticLambda;.invoke([F)Ljava/lang/Integer;"
→ SIGABRT，整个进程被杀
```

所以回调单独写成 Java 类 `TtsChunkCallback.java`（javac 会同时生成 `Integer invoke(float[])`
和 bridge `Object invoke(Object)`）。**以后接任何 sherpa-onnx 的 Java 回调都照这个来。**

### 已知坑：AssetManager 不能传

`sherpa-onnx` 的 Kotlin 构造函数是 `OfflineRecognizer(assetManager: AssetManager?, config: ...)`。
如果把 `context.assets` 传进去、模型文件又在 SD 卡上（绝对路径），native 层会去 **asset 里**
找这个绝对路径，找不到就 `abort()` 掉整个进程：

```
W sherpa-onnx: You are using an absolute path '.../asr/tokens.txt', but assetManager is NOT set to null.
F sherpa-onnx: Read binary file: Load '.../tokens.txt' failed
```

现象是「点启动直接闪退，logcat 里没有任何 Java 栈」。**正确写法是不传**：

```kotlin
// 错
rec = OfflineRecognizer(ctx.assets, config)
// 对（assetManager 默认就是 null）
rec = OfflineRecognizer(config = config)
```

只有把模型打进 APK 的 `assets/` 时才需要传 AssetManager。

### 崩溃排查：先看 run.log

模型加载走的是 C++（sherpa-onnx / onnxruntime）。native 层崩了，进程直接消失，
系统不给任何提示，Java 的 try/catch 也拦不住。

所以 App 里做了两件事：

1. **每一步先写盘再执行**，崩了以后文件里最后一行就是崩在哪：
   ```
   10-26 18:02:11.301  加载 ASR：/storage/.../files/asr
   10-26 18:02:11.305    model.int8.onnx=228MB tokens=309B
   10-26 18:02:11.306    系统可用 2871MB, appLimit=512MB, 低内存=false
   10-26 18:02:14.882  ASR 加载完成 ok=true isReady=true
   10-26 18:02:14.883  准备启动 WebSocket 服务
   10-26 18:02:14.990  WebSocket 已监听 ws://192.168.1.7:8765
   ```
   最后一行若是「加载 ASR」，就是 ASR 崩；若是「加载 TTS」，就是 TTS 崩。

2. **日志落在外置私有目录**，连电脑用 MTP 就能拷：
   ```
   /sdcard/Android/data/com.dosgo.voicegw/files/run.log
   /sdcard/Android/data/com.dosgo.voicegw/files/run.prev.log   ← 上一次运行
   ```

主界面右上「运行日志」按钮可以直接看、一键复制。

另外，网关**不再在启动时加载 TTS**，改成第一次合成时加载。这样启动只吃 ASR 的
229MB，TTS 真有问题也不会把整个网关带崩。

## 七、和 Python 版的关系

| | Python 版（电脑 / Termux） | 本 App |
|---|---|---|
| ASR | sherpa-onnx 或纯 Python 自实现 | sherpa-onnx（AAR） |
| TTS | Piper / Kokoro / 云端 | sherpa-onnx VITS |
| 模型体积 | 229MB + 63MB | 229MB + 32MB |
| 后台稳定性 | Termux 容易被杀 | 前台服务，稳 |
| 装法 | 装 Python 环境 | 装 APK |

两边协议一致，可以用 `server/test_client.py` 先验证 App 的协议是否通：

```bash
# 把 URL 改成手机 IP
python test_client.py --voice "你好小智" "现在几点了"
```

## 八、验证到什么程度了

| 项目 | 状态 |
|---|---|
| 编译 | ✅ `BUILD SUCCESSFUL`，APK 16MB |
| lint | ✅ 0 error（34 warning 全是「依赖有新版本」这类提示） |
| 单元测试 | ✅ **26/26 通过**（17 个协议/PCM/VAD + 9 个下载逻辑：跳转、续传、覆盖、截断、404、取消） |
| APK 内容 | ✅ arm64-v8a 的 `libonnxruntime.so` / `libsherpa-onnx-jni.so` 都在，图标正常 |
| 测试页 | ✅ 已编进 APK（`TestActivity` + `RECORD_AUDIO` 权限都在清单里） |
| 下载功能 | ⚠️ 逻辑有 9 个单测覆盖（本地服务器模拟 308/302、Range、截断），但**真实网络能不能通要你在手机上试** |
| 真机运行 | ❌ **没测**——本机没有连接的设备，也没有 arm64 模拟器镜像 |

测试页就是为这一步准备的：装上 APK 后**先别急着接板子**，进「本地测试」点一遍，
确认识别、合成、大模型三段都通，再去搞 ESP32。这样出问题时能立刻分清是哪一段。

也就是说：**代码是能编译、能打包、核心逻辑有测试覆盖的；但 ASR/TTS 真正跑起来的
效果需要你在手机上验证**。真机第一次跑如果崩，把 `adb logcat -s Asr Tts WsServer VoiceGw`
的输出发我。
