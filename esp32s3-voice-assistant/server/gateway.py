#!/usr/bin/env python3
"""
ESP32-S3 语音助手 —— 网关服务

把设备上传的 PCM 音频串成一条完整对话：

    PCM → VAD 分段 → ASR → 唤醒词判断 → LLM → TTS(PCM) → 按帧下发

# 后端可插拔

ASR / LLM / TTS 各自独立选后端，TTS 还带**自动降级链**：
云端挂了就换本地，本地也没了才报错。

    ASR:  siliconflow（SenseVoiceSmall，免费）
    LLM:  siliconflow（便宜）| deepseek
    TTS:  siliconflow（¥0.5/万字符，质量最好）→ piper（本地免费）

# 启动

    pip install -r requirements.txt
    python gateway.py --check      # 先检查各后端是否可用，不花钱
    python gateway.py              # 正式跑
    python gateway.py --text       # 文本调试，不需要硬件

配置写在 server/.env 里（参考 .env.example），或直接用环境变量。
"""

import argparse
import asyncio
import io
import json
import os
import struct
import sys
import time
import wave
from dataclasses import dataclass, field
from pathlib import Path

import aiohttp
import websockets
from websockets.exceptions import ConnectionClosed

# ============================================================================
# 配置
# ============================================================================

HERE = Path(__file__).parent


def load_env():
    """读 .env（手写解析，不为了几行配置引 python-dotenv）"""
    f = HERE / ".env"
    if not f.exists():
        return
    for line in f.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        os.environ.setdefault(k.strip(), v.strip().strip('"').strip("'"))


load_env()

# ---- 音频参数（**必须**和设备端 src/board.rs 逐字一致）----
#
# 曾经的故障（2026-10-28）：这里写 60ms，固件里是 32ms。
# 网关每 60ms 发 1920 字节，设备的下行缓冲只有 FRAME_BYTES=1024，
# `read_frame_data()` 只拷得进前 1024 字节，另外 896 字节连帧一起被
# `consume()` 丢弃 —— 等于每帧扔掉 47% 的音频样本。表现是：
#   · 波形每 32ms 硬截断一次 → 破音 / 沙沙声
#   · 时长被拉长 1.875 倍 → 说话听起来又慢又拖
# 两个症状一个根因。
#
# 现在固定 32ms：512 样本 = 1024 字节，正好对上 esp-sr AEC 的块长，
# 也和固件 board.rs 的 FRAME_MS/FRAME_BYTES/I2S_FRAME_BYTES 完全对齐。
SAMPLE_RATE = 16000
FRAME_MS = 32
FRAME_BYTES = SAMPLE_RATE * 2 * FRAME_MS // 1000   # 1024
DOWNLINK_BYTES = FRAME_BYTES                        # 1024
LISTEN_PORT = int(os.environ.get("GATEWAY_PORT", 8765))

# ---- 后端选择 ----
ASR_BACKEND = os.environ.get("ASR_BACKEND", "siliconflow")
LLM_BACKEND = os.environ.get("LLM_BACKEND", "siliconflow")
TTS_CHAIN = [x.strip() for x in os.environ.get("TTS_CHAIN", "siliconflow,piper").split(",") if x.strip()]
# ASR 降级链。local 走 sherpa-onnx + SenseVoice，和云端是**同一个模型**，
# 只是跑在你机器上——免费、离线、延迟更低。本地跑不了才走云端。
# local       = sherpa-onnx（C++ 实现，最快，但 Termux 装不上）
# local-lite  = 纯 Python 自实现（慢 13ms，但手机也能跑）
# siliconflow = 云端（免费，但要走网络）
ASR_CHAIN = [x.strip() for x in os.environ.get("ASR_CHAIN", "local,local-lite,siliconflow").split(",") if x.strip()]

# ---- 本地 ASR（sherpa-onnx）----
SENSEVOICE_DIR = HERE / "models" / "asr" / "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"
ASR_THREADS = int(os.environ.get("ASR_THREADS", "4"))

# ---- 硅基流动：ASR 永久免费，TTS 收费 ----
SF_KEY = os.environ.get("SILICONFLOW_API_KEY", "")
SF_BASE = os.environ.get("SILICONFLOW_BASE", "https://api.siliconflow.cn/v1")
SF_ASR_MODEL = os.environ.get("SF_ASR_MODEL", "FunAudioLLM/SenseVoiceSmall")
SF_TTS_MODEL = os.environ.get("SF_TTS_MODEL", "FunAudioLLM/CosyVoice2-0.5B")
SF_TTS_VOICE = os.environ.get("SF_TTS_VOICE", "FunAudioLLM/CosyVoice2-0.5B:anna")
SF_LLM_MODEL = os.environ.get("SF_LLM_MODEL", "Qwen/Qwen2.5-7B-Instruct")

# ---- DeepSeek ----
DS_KEY = os.environ.get("DEEPSEEK_API_KEY", "")
DS_BASE = os.environ.get("DEEPSEEK_BASE", "https://api.deepseek.com")
DS_MODEL = os.environ.get("DEEPSEEK_MODEL", "deepseek-chat")

# ---- Piper 本地 TTS ----
PIPER_MODEL = HERE / "models" / "zh_CN-huayan-medium.onnx"
PIPER_LENGTH_SCALE = float(os.environ.get("PIPER_LENGTH_SCALE", "1.0"))  # >1 更慢

# ---- Kokoro 本地 TTS（82M 参数，音质比 Piper 好一档，但慢 6 倍）----
KOKORO_MODEL = HERE / "models" / "tts" / "kokoro-v1.0.onnx"
KOKORO_VOICES = HERE / "models" / "tts" / "voices-v1.0.bin"
# 中文音色：zf_xiaobei/zf_xiaoni/zf_xiaoxiao/zf_xiaoyi（女）
#           zm_yunjian/zm_yunxi/zm_yunxia/zm_yunyang（男）
KOKORO_VOICE = os.environ.get("KOKORO_VOICE", "zf_xiaobei")
KOKORO_SPEED = float(os.environ.get("KOKORO_SPEED", "1.0"))

# ---- edge-tts（需代理，国内直连不通，实测过）----
EDGE_VOICE = os.environ.get("EDGE_VOICE", "zh-CN-XiaoxiaoNeural")
EDGE_PROXY = os.environ.get("EDGE_PROXY", "")

# ============================================================================
# 运行时可改配置 —— 设备网页下发的优先，.env 只是兜底
# ============================================================================
#
# 为什么要这一层：设备端现在有网页配置界面，用户在手机上填完 WiFi、
# DeepSeek key、语音 key 就存进 flash 并随 WS hello 一起下发。
# 如果网关还用启动时读的 .env，那就变成"改一次 key 要重启两边"，
# 网页配置的意义就没了。
#
# 规则：
#   - 设备下发的字段为空 → 用 .env 的值（用户没填就沿用旧配置）
#   - 设备下发的字段非空 → 覆盖
#   - 只有 ASR/TTS 的**调用链**变了才重建后端实例（重建 = 重新加载模型，很慢）
RUNTIME = {
    "llm_backend": "",
    "llm_key": "",
    "llm_model": "",
    "llm_base": "",
    "asr_chain": "",
    "asr_key": "",
    "asr_model": "",
    "tts_chain": "",
    "tts_voice": "",
}


def llm_backend() -> str:
    return RUNTIME["llm_backend"] or LLM_BACKEND


def llm_key() -> str:
    if RUNTIME["llm_key"]:
        return RUNTIME["llm_key"]
    return SF_KEY if llm_backend() == "siliconflow" else DS_KEY


def llm_model() -> str:
    if RUNTIME["llm_model"]:
        return RUNTIME["llm_model"]
    return SF_LLM_MODEL if llm_backend() == "siliconflow" else DS_MODEL


def llm_base() -> str:
    if RUNTIME["llm_base"]:
        return RUNTIME["llm_base"]
    return SF_BASE if llm_backend() == "siliconflow" else DS_BASE


def voice_key() -> str:
    """ASR 和 TTS 共用一把硅基流动 key"""
    return RUNTIME["asr_key"] or SF_KEY


def voice_model() -> str:
    return RUNTIME["asr_model"] or SF_ASR_MODEL


def voice_tts() -> str:
    return RUNTIME["tts_voice"] or SF_TTS_VOICE


def asr_chain_list() -> list:
    s = RUNTIME["asr_chain"]
    return [x.strip() for x in s.split(",") if x.strip()] if s else ASR_CHAIN


def tts_chain_list() -> list:
    s = RUNTIME["tts_chain"]
    chain = [x.strip() for x in s.split(",") if x.strip()] if s else list(TTS_CHAIN)
    # 云端 TTS 必须有语音 key。没配却排在前面的话，**每一句**都要先抛一次
    # 「还没配语音 key」的异常再降级到 Piper —— 日志刷屏，还多一次调度。
    # 这里直接把它沉到队尾（稳定排序，其余顺序不变）。
    if not voice_key():
        chain.sort(key=lambda x: x == "siliconflow")
    return chain


def apply_device_config(cfg) -> bool:
    """合入设备下发的配置，返回是否需要重建 ASR/TTS 后端链。"""
    if not isinstance(cfg, dict):
        return False
    changed_chain = False

    llm = cfg.get("llm")
    if isinstance(llm, dict):
        for src, dst in (("backend", "llm_backend"), ("key", "llm_key"),
                         ("model", "llm_model"), ("base", "llm_base")):
            v = str(llm.get(src) or "").strip()
            if v:
                RUNTIME[dst] = v

    asr = cfg.get("asr")
    if isinstance(asr, dict):
        v = str(asr.get("chain") or "").strip()
        if v:
            if v != (RUNTIME["asr_chain"] or ",".join(ASR_CHAIN)):
                changed_chain = True
            RUNTIME["asr_chain"] = v
        for src, dst in (("key", "asr_key"), ("model", "asr_model")):
            v2 = str(asr.get(src) or "").strip()
            if v2:
                RUNTIME[dst] = v2

    tts = cfg.get("tts")
    if isinstance(tts, dict):
        v = str(tts.get("chain") or "").strip()
        if v:
            if v != (RUNTIME["tts_chain"] or ",".join(TTS_CHAIN)):
                changed_chain = True
            RUNTIME["tts_chain"] = v
        v2 = str(tts.get("voice") or "").strip()
        if v2:
            RUNTIME["tts_voice"] = v2

    return changed_chain


def print_runtime_config(prefix="[cfg]"):
    def mask(k):
        return (k[:8] + "…") if k else "未设置"
    print(f"{prefix} LLM  = {llm_backend()} / {llm_model()}  key={mask(llm_key())}")
    print(f"{prefix} ASR  = {' → '.join(asr_chain_list())}  key={mask(voice_key())}")
    print(f"{prefix} TTS  = {' → '.join(tts_chain_list())}  voice={voice_tts()}")


# ---- 唤醒词 ----
WAKE_WORDS = [
    # 设备端 WakeNet 用的是「小特小特」，ASR 转写可能写成下面几种
    "小特小特", "小特", "小特小",
    "你好小智", "小智小智", "嗨小智", "嘿小智", "小智",
]

# 是否要求先喊唤醒词才应答。
#
# 设备端 WakeNet 目前没跑起来（见协议里的说明），所以默认关掉这道闸：
# 任何一段语音都会直接进 LLM。等唤醒词调通后再设 REQUIRE_WAKE=1。
REQUIRE_WAKE = os.environ.get("REQUIRE_WAKE", "0") == "1"
# 同音兜底：ASR 很容易把「小智」听成「小芝 / 小志 / 晓智」，
# 实测本地 SenseVoice 就输出过「你好，小芝」。
# 所以除了精确字符串，再用拼音匹配一遍，但有两条约束防误唤醒：
#   1. 整句不超过 12 字（长句子是在聊天，不是在叫它）
#   2. "xiaozhi" 必须出现在前 8 个拼音字符内（唤醒词总在句首附近）
# 这样「我在学习小知识」不会唤醒（xiaozhi 出现在第 13 个字符），
# 而「你好小芝」「晓智你在吗」「小志小志」都能中。
WAKE_PINYIN = "xiaozhi"
WAKE_PINYIN_MAX_LEN = 12
WAKE_PINYIN_MAX_POS = 8


def match_wake(text: str) -> bool:
    if not text:
        return False
    if any(w in text for w in WAKE_WORDS):
        return True
    if len(text) > WAKE_PINYIN_MAX_LEN:
        return False
    try:
        from pypinyin import lazy_pinyin
    except ImportError:
        return False
    flat = "".join(lazy_pinyin(text)).lower()
    pos = flat.find(WAKE_PINYIN)
    return 0 <= pos <= WAKE_PINYIN_MAX_POS

SYSTEM_PROMPT = (
    "你是一个运行在 ESP32 小音箱里的语音助手。"
    "回答要短：一句话、40 个字以内，因为你的回答会被转成语音慢慢播放，说多了用户只能干等。"
    "不要用 Markdown、不要用列表、不要输出表情符号。"
    "不确定的事情直接说不知道，不要编。"
)

# ---- VAD ----
#
# 【注意】这些毫秒值靠 `sess.speech_ms += FRAME_MS` 累加得出，所以前提是
# FRAME_MS 和固件真实帧长一致。之前网关按 60ms 记账、固件实际发的是 32ms
# 音频，静音计时被加速了 1.875 倍 —— 配 700 实际只有 373ms 就切句。
# FRAME_MS 改成 32 之后累加才是真毫秒，所以这里同步回调：
# 450ms 是中文自然换气停顿（200~300ms）之上、又不至于让人觉得"说完半天没反应"的值。
# 能量阈值必须和固件里 audio.rs 的 `(noise_floor * 2).max(200)` **大致对齐**。
#
# 曾经这里是 500，而固件那边是 400/200。同一段音频，设备判「这是人声，上传」，
# 网关却判「这是静音，不理」—— 两边打架，用户看到的就是设备明明在传数据，
# 却永远得不到回应。现在统一到 250：安静环境实测 rms 37~105，正常说话 350+。
VAD_RMS_THRESHOLD = 250
# 连续多响才算「开口」。真实说话是持续的，噪音是断续的 —— 这是两者最可靠的差别。
VAD_ONSET_MS = 96
VAD_SILENCE_MS = 450
VAD_MIN_SPEECH_MS = 300
# 最长一段语音。设 8 秒而不是 15：环境吵的时候 VAD 可能一直判「还在说」，
# 拖到 15 秒才强制截断，那一段音频已经混了几十句噪音，识别必然失败。
VAD_MAX_UTTERANCE_MS = 8000
AWAKE_TIMEOUT_S = 90


# ============================================================================
# 工具
# ============================================================================

def rms(pcm: bytes) -> int:
    """一帧 PCM 的 RMS。

    # 为什么用 numpy 而不是 for 循环

    纯 Python 逐样本 `struct.unpack_from` 处理 2048 字节要 1000+ 次函数调用，
    约 1ms。设备每秒发 31~43 条，看着只占 3%，但这是**在事件循环里同步执行**的：
    一旦某几帧挤在一起，接收就跟不上，TCP 窗口打满，设备那边的 `write_all`
    直接阻塞 —— 实测现象就是「设备传了 8 秒音频之后突然不动了」。
    numpy 版本快两个数量级，这条路径彻底不再是瓶颈。
    """
    import numpy as np
    if len(pcm) < 2:
        return 0
    a = np.frombuffer(pcm[: len(pcm) // 2 * 2], dtype="<i2").astype(np.float32)
    if a.size == 0:
        return 0
    return int(np.sqrt((a * a).mean()))


def pcm_to_wav(pcm: bytes, rate: int = SAMPLE_RATE) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(pcm)
    return buf.getvalue()


def strip_wav_header(data: bytes) -> bytes:
    if len(data) > 44 and data[:4] == b"RIFF" and data[8:12] == b"WAVE":
        pos = 12
        while pos + 8 <= len(data):
            cid = data[pos:pos + 4]
            size = struct.unpack_from("<I", data, pos + 4)[0]
            if cid == b"data":
                return data[pos + 8:pos + 8 + size]
            pos += 8 + size + (size & 1)
        return data[44:]
    return data


def resample(pcm: bytes, src: int, dst: int) -> bytes:
    """重采样，用**频域截断**而不是线性插值。

    # 为什么不能用 np.interp

    线性插值只做「按位置取加权平均」，**完全没有抗混叠滤波**。
    Piper 输出 22.05kHz，降到 16kHz 时 8kHz 以上的能量不会被滤掉，
    而是折叠回可听频段变成一片沙沙声 —— 听感就是「破音 / 发毛」。

    频域做法直接 rfft 之后把超出新奈奎斯特频率的 bin 丢掉，天然就是
    砖墙低通，干净得多。TTS 每段只有几秒，一次 rfft 的开销可以忽略。
    """
    if src == dst or not pcm:
        return pcm
    import numpy as np
    x = np.frombuffer(pcm, dtype="<i2").astype(np.float32)
    if x.size == 0:
        return b""
    n_out = int(x.size * dst / src)
    if n_out < 2:
        return b""

    spec = np.fft.rfft(x)
    n_bins = n_out // 2 + 1
    if n_bins <= spec.size:
        spec = spec[:n_bins]          # 降采样：砍掉高频，这就是抗混叠
    else:
        spec = np.pad(spec, (0, n_bins - spec.size))
    y = np.fft.irfft(spec, n_out) * (n_out / x.size)
    return y.clip(-32768, 32767).astype("<i2").tobytes()


def save_wav(path, pcm: bytes):
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(pcm)


# ============================================================================
# ASR 后端
# ============================================================================

class AsrBackend:
    name = "base"
    available = True

    async def transcribe(self, http: aiohttp.ClientSession, pcm: bytes) -> str:
        raise NotImplementedError


class SiliconFlowAsr(AsrBackend):
    """SenseVoiceSmall —— 硅基流动价格页长期标 0 元，HTTP 同步接口"""
    name = "siliconflow/SenseVoiceSmall"

    def __init__(self):
        # available 在每次调用时实时判断，这样网页改了 key 立刻生效
        self.available = True

    async def transcribe(self, http, pcm):
        key = voice_key()
        if not key:
            return ""
        form = aiohttp.FormData()
        form.add_field("file", pcm_to_wav(pcm), filename="a.wav", content_type="audio/wav")
        form.add_field("model", voice_model())
        try:
            async with http.post(
                f"{SF_BASE}/audio/transcriptions", data=form,
                headers={"Authorization": f"Bearer {key}"},
                timeout=aiohttp.ClientTimeout(total=20),
            ) as r:
                if r.status != 200:
                    print(f"[asr] HTTP {r.status}: {(await r.text())[:200]}")
                    return ""
                return ((await r.json()).get("text") or "").strip()
        except Exception as e:
            print(f"[asr] 异常: {e}")
            return ""


class LocalSenseVoiceAsr(AsrBackend):
    """
    本地 SenseVoice —— **和云端用的是同一个模型**（FunAudioLLM/SenseVoiceSmall）。

    这不是"效果接近 API 的开源替代"，而是模型本身同一份权重，
    只是从硅基流动的 GPU 挪到你自己的 CPU 上跑。所以识别质量基本一致，
    好处是免费、离线、没有网络往返（省 200-400ms），
    代价是占 500MB 内存、首次加载要 1-2 秒。

    用 sherpa-onnx 的 int8 量化版，纯 CPU，不需要 PyTorch。
    """
    name = "local/SenseVoice(int8)"

    def __init__(self):
        self.rec = None
        self.available = (SENSEVOICE_DIR / "model.int8.onnx").exists() and \
                         (SENSEVOICE_DIR / "tokens.txt").exists()
        # sherpa-onnx 的 wheel 是 manylinux 的，Termux（bionic libc）装不上，
        # 那种情况下让位给下面的纯 Python 版
        if self.available:
            try:
                import sherpa_onnx  # noqa: F401
            except ImportError:
                self.available = False

    def _load(self):
        if self.rec is None:
            import sherpa_onnx
            print("[asr] 加载本地 SenseVoice（首次约 1-2 秒）…")
            self.rec = sherpa_onnx.OfflineRecognizer.from_sense_voice(
                model=str(SENSEVOICE_DIR / "model.int8.onnx"),
                tokens=str(SENSEVOICE_DIR / "tokens.txt"),
                num_threads=ASR_THREADS,
                use_itn=True,        # 逆文本归一化：把"一百二十三"输出成"123"
                language="zh",
                debug=False,
            )
        return self.rec

    def _transcribe_sync(self, pcm: bytes) -> str:
        import numpy as np
        rec = self._load()
        samples = np.frombuffer(pcm, dtype="<i2").astype(np.float32) / 32768.0
        stream = rec.create_stream()
        stream.accept_waveform(SAMPLE_RATE, samples)
        rec.decode_stream(stream)
        return (stream.result.text or "").strip()

    async def transcribe(self, http, pcm):
        if not self.available:
            raise RuntimeError(f"本地 SenseVoice 模型不存在：{SENSEVOICE_DIR}")
        # CPU 推理是同步的，丢线程池
        return await asyncio.to_thread(self._transcribe_sync, pcm)


class LocalSenseVoiceLiteAsr(AsrBackend):
    """
    纯 Python 的 SenseVoice —— 为了手机上能跑。

    sherpa-onnx 的 Python 包在 PyPI 上是 manylinux wheel（要 glibc），
    安卓 Termux 用的是 bionic libc，装不上。但模型本身是开源 ONNX，
    前处理参数（CMVN / LFR）也都写在模型元数据里，所以自己用 onnxruntime
    重新实现了一遍，见 `sensevoice_lite.py`。

    实测和 sherpa-onnx 的结果**逐字一致**，只是慢 13ms（Python 算 fbank 的开销）：
        自实现 73ms / sherpa-onnx 60ms （5 句平均，4 核 CPU）

    手机上只要 `pip install onnxruntime numpy` 就能跑。
    """
    name = "local/SenseVoice-lite(py)"

    def __init__(self):
        self.asr = None
        self.available = (SENSEVOICE_DIR / "model.int8.onnx").exists()
        if self.available:
            try:
                import onnxruntime  # noqa: F401
                import numpy  # noqa: F401
            except ImportError:
                self.available = False

    def _load(self):
        if self.asr is None:
            from sensevoice_lite import SenseVoiceLite
            print("[asr] 加载纯 Python SenseVoice（首次约 1-2 秒）…")
            self.asr = SenseVoiceLite(SENSEVOICE_DIR, num_threads=ASR_THREADS)
        return self.asr

    def _transcribe_sync(self, pcm: bytes) -> str:
        return self._load().transcribe(pcm, language="zh")

    async def transcribe(self, http, pcm):
        if not self.available:
            raise RuntimeError("纯 Python SenseVoice 不可用（缺模型或缺 onnxruntime）")
        return await asyncio.to_thread(self._transcribe_sync, pcm)


class AsrChain:
    """
    ASR 降级链。

    注意：只有**抛异常**才降级，返回空字符串不降级。
    空字符串的意思是"这段音频里没人说话"，是正常结果，不该让云端再算一遍。
    """

    def __init__(self, backends):
        self.backends = [b for b in backends if b.available]
        if not self.backends:
            raise RuntimeError("没有任何可用的 ASR 后端（检查 ASR_CHAIN 和本地模型）")

    async def transcribe(self, http, pcm):
        last_err = None
        for b in self.backends:
            try:
                t0 = time.time()
                text = await b.transcribe(http, pcm)
                dt = (time.time() - t0) * 1000
                if text:
                    print(f"  [asr] {b.name} {dt:.0f}ms")
                return text
            except Exception as e:
                print(f"  [asr] {b.name} 失败（{e}），降级下一个")
                last_err = e
        raise RuntimeError(f"所有 ASR 后端都失败：{last_err}")


# ============================================================================
# TTS 后端
# ============================================================================

class TtsBackend:
    name = "base"
    available = True

    async def synth(self, http: aiohttp.ClientSession, text: str) -> bytes:
        """返回 16kHz / 16bit / 单声道 little-endian PCM"""
        raise NotImplementedError


class SiliconFlowTts(TtsBackend):
    """CosyVoice2 —— ¥0.5/万字符，中文自然度最好"""
    name = "siliconflow/CosyVoice2"

    def __init__(self):
        self.available = True

    async def synth(self, http, text):
        key = voice_key()
        if not key:
            raise RuntimeError("还没配语音 key（网页配置里填，或 .env 里的 SILICONFLOW_API_KEY）")
        async with http.post(
            f"{SF_BASE}/audio/speech",
            json={
                "model": SF_TTS_MODEL,
                "voice": voice_tts(),
                "input": text,
                "response_format": "pcm",
                "sample_rate": SAMPLE_RATE,
                "speed": 1.0,
            },
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
            timeout=aiohttp.ClientTimeout(total=40),
        ) as r:
            if r.status != 200:
                raise RuntimeError(f"HTTP {r.status}: {(await r.text())[:150]}")
            data = strip_wav_header(await r.read())
        if not data:
            raise RuntimeError("返回空音频")
        return data


class PiperTts(TtsBackend):
    """本地 Piper —— 完全免费、离线。质量中等，作为降级兜底。"""
    name = "piper/zh_CN-huayan"

    def __init__(self):
        self.voice = None
        self.available = PIPER_MODEL.exists()

    def _load(self):
        if self.voice is None:
            from piper import PiperVoice
            print("[tts] 加载 Piper 模型（首次约 3 秒）…")
            self.voice = PiperVoice.load(str(PIPER_MODEL))
        return self.voice

    def _synth_sync(self, text: str) -> bytes:
        from piper import SynthesisConfig
        import numpy as np
        voice = self._load()
        chunks = list(voice.synthesize(text, syn_config=SynthesisConfig(length_scale=PIPER_LENGTH_SCALE)))
        if not chunks:
            return b""
        arr = np.concatenate([c.audio_float_array for c in chunks]).astype(np.float32)
        # 归一化到 -9dBFS，而不是让它贴着 0dBFS 出去。
        #
        # 实测 Piper 输出的峰值是 -0.1dBFS，几乎打满。经 ES8311 的 DAC 再叠
        # NS4150B 约 24dB 的固定增益，输出必然撞到电源轨 —— 这就是那个
        # 「一直在破音」的根源，光调 DAC 音量压不住，必须在源头留余量。
        #
        # 用「按峰值归一化」而不是固定乘系数：这样每句话的响度是稳定的，
        # 不会出现长句变小声、短句特别炸的情况。
        peak = float(np.abs(arr).max())
        if peak > 1e-6:
            arr = arr / peak * 0.35
        pcm = (arr * 32767).clip(-32768, 32767).astype("<i2").tobytes()
        # Piper 的 huayan 模型是 22.05kHz，要降到设备端的 16kHz
        return resample(pcm, chunks[0].sample_rate, SAMPLE_RATE)

    async def synth(self, http, text):
        if not self.available:
            raise RuntimeError(f"Piper 模型不存在：{PIPER_MODEL}")
        # CPU 密集的同步调用，丢线程池，别堵住事件循环
        return await asyncio.to_thread(self._synth_sync, text)


class KokoroTts(TtsBackend):
    """
    本地 Kokoro-82M —— 音质明显好于 Piper，但慢 6 倍。

    实测（4 核 CPU，热态）：
        2 字  → 0.70s 音频，643ms
        8 字  → 1.37s 音频，941ms
        19 字 → 3.86s 音频，1824ms
    对比 Piper 同样 19 字只要 325ms，实时率 0.07x vs 0.45x。

    固定开销约 500ms，所以它适合「回答短、能等半秒」的场景。
    要真是实时对话，Piper 或云端更合适。

    中文需要 misaki 的 G2P 先把汉字转成 IPA 音素，再喂给模型。
    """
    name = "kokoro/%s" % "kokoro"

    def __init__(self):
        self.k = None
        self.g = None
        self.available = KOKORO_MODEL.exists() and KOKORO_VOICES.exists()
        self.name = f"kokoro/{KOKORO_VOICE}"

    def _load(self):
        if self.k is None:
            from kokoro_onnx import Kokoro
            from misaki import zh
            print("[tts] 加载 Kokoro + 中文 G2P（首次约 2 秒）…")
            self.k = Kokoro(str(KOKORO_MODEL), str(KOKORO_VOICES))
            self.g = zh.ZHG2P()
        return self.k, self.g

    def _synth_sync(self, text: str) -> bytes:
        import numpy as np
        k, g = self._load()
        ps = g(text)
        samples, sr = k.create(ps, voice=KOKORO_VOICE, is_phonemes=True, speed=KOKORO_SPEED)
        pcm = (samples * 32767).clip(-32768, 32767).astype("<i2").tobytes()
        return resample(pcm, sr, SAMPLE_RATE)

    async def synth(self, http, text):
        if not self.available:
            raise RuntimeError(f"Kokoro 模型不存在：{KOKORO_MODEL}")
        return await asyncio.to_thread(self._synth_sync, text)


class EdgeTts(TtsBackend):
    """
    微软 Edge 在线语音 —— 免费无限量、质量高，但两个硬伤：
      1. 国内直连不通（实测 speech.platform.bing.com 无法连接），必须配 EDGE_PROXY
      2. 只输出 mp3，转 16k PCM 需要 ffmpeg
    所以默认不进降级链，装好 ffmpeg 且配了代理才建议启用。
    """
    name = "edge/zh-CN-XiaoxiaoNeural"

    def __init__(self):
        try:
            import edge_tts  # noqa: F401
            self.available = bool(EDGE_PROXY)
        except ImportError:
            self.available = False

    async def synth(self, http, text):
        raise RuntimeError("edge 输出 mp3，需要 ffmpeg 才能转 PCM —— 建议直接用 piper")


class TtsChain:
    """按顺序尝试，第一个成功的就用。全挂才报错。"""

    def __init__(self, backends):
        self.backends = [b for b in backends if b.available]
        if not self.backends:
            raise RuntimeError("没有任何可用的 TTS 后端（检查 TTS_CHAIN 配置和 Piper 模型）")

    async def synth(self, http, text):
        last_err = None
        for b in self.backends:
            try:
                t0 = time.time()
                pcm = await b.synth(http, text)
                if pcm:
                    dt = (time.time() - t0) * 1000
                    print(f"  [tts] {b.name}：{len(pcm)/2/SAMPLE_RATE:.1f}s 音频，{dt:.0f}ms")
                    return pcm
                last_err = RuntimeError("空音频")
            except Exception as e:
                print(f"  [tts] {b.name} 失败（{e}），降级下一个")
                last_err = e
        raise RuntimeError(f"所有 TTS 后端都失败：{last_err}")


# ============================================================================
# LLM
# ============================================================================

async def chat(http: aiohttp.ClientSession, messages: list) -> str:
    backend = llm_backend()
    key = llm_key()
    model = llm_model()
    base = llm_base()

    if backend == "deepseek":
        if not key:
            return "还没配置 DeepSeek 的 API Key。"
    else:
        if not key:
            return "还没配置硅基流动的 API Key。"

    url = f"{base}/chat/completions"

    try:
        async with http.post(
            url,
            json={
                "model": model,
                "messages": [{"role": "system", "content": SYSTEM_PROMPT}] + messages,
                "max_tokens": 200,
                "temperature": 0.7,
            },
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
            timeout=aiohttp.ClientTimeout(total=30),
        ) as r:
            if r.status != 200:
                print(f"[llm] HTTP {r.status}: {(await r.text())[:200]}")
                return "抱歉，我这边出了点问题。"
            return (await r.json())["choices"][0]["message"]["content"].strip()
    except Exception as e:
        print(f"[llm] 异常: {e}")
        return "抱歉，网络好像不太好。"


async def chat_stream(http: aiohttp.ClientSession, messages: list):
    """流式版 chat：一边生成一边把增量文本 yield 出去。

    # 为什么必须流式

    非流式要等 DeepSeek 把整段话生成完（首 token 只要 0.4s，但整段要 1.5~2.5s），
    然后才轮到 TTS 合成（又 0.5s），最后才有第一个字从喇叭出来。用户体感就是
    「说完话要愣三秒」。流式之后第一句话一生成出来就能立刻合成、立刻播，
    后面的句子在播放间隙里并行生成，人耳基本感觉不到等待。

    协议是 OpenAI 兼容的 SSE：每行 `data: {...}`，最后一行 `data: [DONE]`。
    DeepSeek 和硅基流动都认这套。
    """
    backend = llm_backend()
    key = llm_key()
    model = llm_model()
    base = llm_base()

    if not key:
        yield ("还没配置 DeepSeek 的 API Key。" if backend == "deepseek"
               else "还没配置硅基流动的 API Key。")
        return

    url = f"{base}/chat/completions"
    payload = {
        "model": model,
        "messages": [{"role": "system", "content": SYSTEM_PROMPT}] + messages,
        "max_tokens": 200,
        "temperature": 0.7,
        "stream": True,
    }

    try:
        async with http.post(
            url,
            json=payload,
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
            # sock_read 放宽到 20s：模型偶尔会先憋一会儿再吐字
            timeout=aiohttp.ClientTimeout(total=40, sock_read=20),
        ) as r:
            if r.status != 200:
                print(f"[llm] HTTP {r.status}: {(await r.text())[:200]}")
                yield "抱歉，我这边出了点问题。"
                return
            async for raw in r.content:
                line = raw.strip()
                if not line.startswith(b"data:"):
                    continue
                data = line[5:].strip()
                if data == b"[DONE]":
                    break
                try:
                    j = json.loads(data)
                except Exception:
                    continue
                ch = j.get("choices") or []
                if not ch:
                    continue
                piece = (ch[0].get("delta") or {}).get("content")
                if piece:
                    yield piece
    except Exception as e:
        print(f"[llm] 流式异常: {e}")
        yield "抱歉，网络好像不太好。"


# ---- 断句 ----------------------------------------------------------------
# 句末标点：切句的第一优先级
_SENT_END = "。！？!?…\n"
# 从句中的标点：一直没有句末标点时，退而求其次在这里切
_SENT_SOFT = "，,；;、：:"
# 收尾符号：切在句末标点后，紧跟的引号括号要一起带走
_SENT_TAIL = "”’』」）)】》"


def cut_sentence(buf: str):
    """从累积文本里切出**第一句**，返回 `(句子, 剩余文本)`。

    没切出完整句子时返回 `("", buf)`，调用方继续攒。

    # 阈值为什么是 18 字

    首包延迟 = 等 LLM 吐出第一句的时间 + TTS 合成时间。DeepSeek 大约
    30~50 字/秒，所以「等 28 字」和「等 18 字」差 200~300ms，这部分是用户
    开口后干等的主要来源之一。

    18 字大约正好是一个带逗号的中文短句，切出来听着是自然停顿而不是硬截断；
    一直没有标点才会退到每 16 字硬切一刀 —— 多合成几次的代价远小于让人干等。
    """
    for i, ch in enumerate(buf):
        if ch in _SENT_END:
            j = i + 1
            while j < len(buf) and buf[j] in _SENT_TAIL:
                j += 1
            sent = buf[:j].strip()
            rest = buf[j:]
            # 只有标点没有内容（比如开头的「……」），跳过继续累计
            if sent.strip(_SENT_END + _SENT_TAIL + " "):
                return sent, rest

    if len(buf) >= 18:
        for i in range(len(buf) - 1, -1, -1):
            if buf[i] in _SENT_SOFT:
                return buf[:i + 1].strip(), buf[i + 1:]
        return buf[:16].strip(), buf[16:]
    return "", buf


async def _synth_safe(tts, http, text: str) -> bytes:
    """单句合成，失败不抛 —— 一句挂了不该让整段回复哑掉。"""
    if not text.strip():
        return b""
    try:
        return await tts.synth(http, text)
    except Exception as e:
        print(f"  [tts] 单句失败（{e}）")
        return b""


# ============================================================================
# 会话
# ============================================================================

@dataclass
class Session:
    awake: bool = False
    last_talk: float = 0.0
    history: list = field(default_factory=list)
    buf: bytearray = field(default_factory=bytearray)
    speech_ms: int = 0
    silence_ms: int = 0
    # 连续超阈值的累计时长。用来区分「真的在说话」和「碰了一下桌子」：
    # 环境里的短促噪音每 1~2 秒冒一次，如果单帧就判 in_speech，
    # 静音计时会被永远清零，网关永远等不到「说完」——表现为设备一直在传，
    # 但就是不给任何回应。
    hot_ms: int = 0
    in_speech: bool = False
    busy: bool = False
    # 下行发送器。按连接复用 —— 它记着设备端播放队列的水位估算，
    # 跨句子、跨轮次都要接着算，不能每次 respond 新建。
    sender: object = None
    # 上行统计：排查「设备明明在传却收不到」时看这两个数
    rx_msgs: int = 0
    rx_bytes: int = 0

    def reset_utterance(self):
        self.buf.clear()
        self.speech_ms = 0
        self.silence_ms = 0
        self.hot_ms = 0
        self.in_speech = False


async def send_json(ws, obj):
    try:
        await ws.send(json.dumps(obj, ensure_ascii=False))
    except Exception:
        pass


class PcmSender:
    """管理一路语音下行：发送节拍 + 设备端播放队列的水位估算。

    # 为什么需要「水位」

    设备端 speaker_task 从 SPK_CH（深度 8 帧 = 256ms）取帧喂 I2S，队列空了
    就只能播一帧静音顶上 —— 听感是持续的「咔咔」卡顿，和破音很难分辨。

    如果网关严格 1:1 按 32ms 发，队列水位长期停在 0~1 帧，WiFi 任何几毫秒的
    抖动都能把它抽干。所以要主动把水位垫到 TARGET 帧再按节拍走。

    # 为什么不能「每段开头猛发 4 帧」

    那样每句话都往上抬一次水位，而 1:1 阶段水位不会回落，于是一句比一句高，
    到第三句就把 8 帧队列冲爆 —— 设备丢最旧帧，那才是真的破音。
    所以这里维护一个估算水位，只在**低于目标时补水**，够了立刻切回节拍。

    # 为什么用绝对时间轴

    Windows 的 asyncio 定时器颗粒度约 15.6ms，`sleep(0.032)` 实测落在
    31~47ms 之间随机抖。逐帧「发完再睡」会把这个抖动累加成漂移，播 10 秒能
    偏出小半秒。绝对时间轴每帧往后推固定 32ms，抖动不累积。
    """

    TARGET = 4      # 目标水位（帧）= 128ms，足够吸收 WiFi 抖动
    SAFE_MAX = 7    # 绝不超过队列深度 8，留 1 帧余量

    def __init__(self):
        self.level = 0.0
        self.last_t = None
        # 水位历史峰值。正常应该稳定在 TARGET 附近；逼近 SAFE_MAX 就说明
        # 有突发灌帧（会丢帧破音），排查时看这个数。
        self.peak = 0.0

    async def send(self, ws, pcm: bytes):
        loop = asyncio.get_running_loop()
        step = FRAME_MS / 1000.0

        now = loop.time()
        if self.last_t is not None:
            # 上一段结束到现在，设备一直在播，水位要相应下落。
            # 隔了很久（比如上一轮对话几分钟前）会直接落到 0，正是想要的。
            self.level = max(0.0, self.level - (now - self.last_t) / step)
        next_t = now

        for i in range(0, len(pcm), DOWNLINK_BYTES):
            chunk = pcm[i:i + DOWNLINK_BYTES]
            if len(chunk) < DOWNLINK_BYTES:
                chunk += b"\x00" * (DOWNLINK_BYTES - len(chunk))

            if self.level >= self.TARGET:
                # 水位够了：按 32ms 节拍走。等的这一帧时间里设备也消费了一帧，
                # 所以水位保持不动。
                next_t += step
                delay = next_t - loop.time()
                if delay > 0.001:
                    await asyncio.sleep(delay)
                elif delay < -0.15:
                    # 落后超过 150ms 就别追了 —— 追赶会突发灌帧把队列冲爆，
                    # 那才是真破音。时间轴对齐到当下，就当这句拖长了一点。
                    next_t = loop.time()
            else:
                # 水位不足：这一帧不等待，直接补进去
                self.level += 1.0
                next_t = loop.time()

            try:
                await ws.send(chunk)
            except Exception:
                return
            if self.level > self.peak:
                self.peak = self.level

        self.last_t = loop.time()


async def respond(ws, sess, http, tts, user_text: str):
    """一轮完整应答：LLM 流式出字 → 按句切 → 合成 → 边合成边播。

    # 三条流水线并行

        chat_stream ──text_q──> synth_worker ──audio_q──> play_worker ──ws──> 设备

    - `synth_worker` 和 `play_worker` 是两个独立 task，所以**正在播第一句的时候，
      第二句已经在合成了**。如果串行（合成完→播→合成下一句），每句话之间会有
      一个 150ms 的哑巴缝，听起来一顿一顿的。
    - 句子按顺序进出队列，保证播出来的语序不会乱。
    """
    sess.busy = True
    try:
        sess.history.append({"role": "user", "content": user_text})
        sess.history = sess.history[-12:]

        t0 = time.time()
        parts: list = []
        text_q: asyncio.Queue = asyncio.Queue()
        audio_q: asyncio.Queue = asyncio.Queue()
        # 用 dict 是因为闭包要跨两个 task 读写
        stat = {"playing": False, "first_ms": 0.0, "bytes": 0}

        if sess.sender is None:
            sess.sender = PcmSender()

        async def synth_worker():
            while True:
                sent = await text_q.get()
                if sent is None:
                    await audio_q.put(None)
                    return
                audio = await _synth_safe(tts, http, sent)
                if audio:
                    await audio_q.put((sent, audio))

        async def play_worker():
            while True:
                item = await audio_q.get()
                if item is None:
                    return
                sent, audio = item
                if not stat["playing"]:
                    stat["playing"] = True
                    stat["first_ms"] = (time.time() - t0) * 1000
                    print(f"  [pipe] 首段出声 {stat['first_ms']:.0f}ms（{sent[:18]}）")
                    # 设备收到 start 才会把状态切到 Speaking、跑 AEC
                    await send_json(ws, {"type": "tts", "state": "start"})
                stat["bytes"] += len(audio)
                await sess.sender.send(ws, audio)

        sw = asyncio.create_task(synth_worker())
        pw = asyncio.create_task(play_worker())

        buf = ""
        async for delta in chat_stream(http, sess.history):
            parts.append(delta)
            buf += delta
            while True:
                sent, buf = cut_sentence(buf)
                if not sent:
                    break
                text_q.put_nowait(sent)

        tail = buf.strip()
        if tail:
            text_q.put_nowait(tail)
        text_q.put_nowait(None)

        # 两个 worker 都结束 = 所有 PCM 都已经交给 ws.send 了
        await asyncio.gather(sw, pw)

        reply = "".join(parts).strip()
        print(f"  [llm] {reply}")
        if reply:
            sess.history.append({"role": "assistant", "content": reply})
            await send_json(ws, {"type": "llm", "text": reply})

        if stat["playing"]:
            print(f"  [pipe] 共 {stat['bytes'] // DOWNLINK_BYTES} 帧 / "
                  f"{stat['bytes'] / 2 / SAMPLE_RATE:.1f}s，首段 {stat['first_ms']:.0f}ms")
        else:
            print("  [tts] 没合成出任何音频")

        # 必须放在所有音频都交给 sender 之后：设备靠这条消息等播放队列放空再回 Idle，
        # 早发会把它切回待机、最后一句被截断。
        await send_json(ws, {"type": "tts", "state": "stop"})
    finally:
        sess.busy = False


async def handle_utterance(ws, sess, http, asr, tts):
    if len(sess.buf) < SAMPLE_RATE * 2 * VAD_MIN_SPEECH_MS // 1000:
        sess.reset_utterance()
        return

    pcm = bytes(sess.buf)
    print(f"  [vad] 一段语音 {len(pcm)/2/SAMPLE_RATE:.2f}s，送去识别")
    # 调试留档：ASR 识别得莫名其妙时，直接拿这个文件看上行音频到底长什么样
    # （能量分布、有没有断裂、是不是只有半截）。只留最近一次，不做堆积。
    try:
        save_wav(HERE / "debug_last.wav", pcm)
    except Exception:
        pass
    sess.reset_utterance()

    try:
        text = await asr.transcribe(http, pcm)
    except Exception as e:
        print(f"  [asr] 全部失败：{e}")
        return
    if not text:
        print("  [asr] 空结果（key 没配 / 音频太短 / 噪声）")
        return
    print(f"  [asr] {text}")
    await send_json(ws, {"type": "stt", "text": text})

    if REQUIRE_WAKE and not sess.awake:
        if match_wake(text):
            sess.awake = True
            sess.last_talk = time.time()
            print("  [wake] 唤醒成功")
            await send_json(ws, {"type": "ack"})
            await respond(ws, sess, http, tts, "（用户刚叫了你，用一句话简短回应，比如『我在』）")
        else:
            print("  [wake] 未命中唤醒词，忽略")
            return
    elif not REQUIRE_WAKE:
        # 不要求唤醒词时，每段语音都当作用户在跟设备说话
        sess.awake = True

    sess.last_talk = time.time()
    await respond(ws, sess, http, tts, text)


# ============================================================================
# 网络
# ============================================================================

def build_backends():
    asr_map = {
        "siliconflow": SiliconFlowAsr,
        "local": LocalSenseVoiceAsr,
        "local-lite": LocalSenseVoiceLiteAsr,
    }
    tts_map = {
        "siliconflow": SiliconFlowTts,
        "piper": PiperTts,
        "kokoro": KokoroTts,
        "edge": EdgeTts,
    }
    asr = AsrChain([asr_map[n]() for n in asr_chain_list() if n in asr_map])
    chain = TtsChain([tts_map[n]() for n in tts_chain_list() if n in tts_map])
    return asr, chain


async def device_handler(ws, asr, tts):
    peer = ws.remote_address
    print(f"\n[conn] 设备接入 {peer}")
    sess = Session()

    async with aiohttp.ClientSession() as http:
        async def handle_json(raw: str):
            nonlocal asr, tts
            try:
                j = json.loads(raw)
            except Exception as e:
                print(f"[json] 解析失败: {e} raw={raw[:80]!r}")
                return
            # 设备每 8 秒发一次保活心跳，量很大，别让它把日志刷掉
            if j.get("type") == "hb":
                # 必须**回一个**。设备端有个 30 秒「收不到任何下行数据就判断线」
                # 的保护（用于网关被强杀后快速恢复），如果这边一声不吭，
                # 设备会每 30 秒自己断开重连一次 —— 表现为「用着用着突然不吭声」。
                await send_json(ws, {"type": "pong"})
                return
            # 排查用：确认文本帧真的到达网关（打印前 40 字符即可）
            print(f"[json] {len(raw)}B 头={raw[:30]!r} 尾={raw[-60:]!r}")
            if j.get("type") == "hello":
                # 设备网页里配的东西随 hello 一起下发。这是让"改完 key 立刻能用"
                # 成立的关键：否则用户得去改这台机器的 .env 再重启网关。
                cfg = j.get("config")
                if isinstance(cfg, dict):
                    try:
                        need_rebuild = apply_device_config(cfg)
                        print_runtime_config("[cfg] 设备下发")
                    except Exception as e:
                        import traceback
                        print(f"[cfg] 应用设备配置失败: {e}")
                        traceback.print_exc()
                        need_rebuild = False
                    if need_rebuild:
                        print("[cfg] 调用链有变化，重建 ASR/TTS 后端")
                        try:
                            asr, tts = build_backends()
                        except Exception as e:
                            print(f"[cfg] 重建失败，沿用旧后端: {e}")
                print(f"[hello] {j.get('format')} {j.get('sample_rate')}Hz "
                      f"{j.get('channels')}ch {j.get('frame_ms')}ms")
                await send_json(ws, {
                    "type": "hello", "transport": "websocket",
                    "audio_params": {"format": "pcm", "sample_rate": SAMPLE_RATE},
                })
            elif j.get("type") == "abort":
                sess.reset_utterance()

        try:
            last_msg_t = time.time()
            # 用 recv() + 超时循环，而不是 `async for msg in ws`。
            #
            # 关掉 WS 控制帧 ping 之后，网关没有别的手段能发现「对端已经死了
            # 但 TCP 没发 FIN」的半开连接 —— 现象是 netstat 里挂着一条永远
            # Established 的僵尸连接，设备每次重连都多一条，越积越多。
            # 设备每 8 秒发一次心跳，所以 40 秒什么都收不到就是死了。
            while True:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=10)
                    last_msg_t = time.time()
                except asyncio.TimeoutError:
                    # 设备每 8 秒发一次心跳，10 秒没动静说明有点不对劲。
                    #
                    # 这里**不能直接断**：网关被强杀重启后，设备那边的 TCP
                    # 毫无察觉，仍会往缓冲里写数据，一等就是 socket timeout。
                    # 所以先发一个 {"type":"ping"} —— 设备收到会刷新自己的
                    # last_rx，30 秒收不到任何东西它就主动断开重连。
                    if time.time() - last_msg_t > 40:
                        print("[conn] 40 秒没有任何消息（含心跳），判为僵尸连接，主动关闭")
                        break
                    try:
                        await ws.send('{"type":"ping"}')
                    except Exception:
                        break
                    continue

                if isinstance(msg, bytes):
                    sess.rx_msgs += 1
                    sess.rx_bytes += len(msg)
                    # 每秒（约 31 条）打一次，确认上行链路真的在动
                    if sess.rx_msgs % 31 == 0:
                        print(f"  [rx] {sess.rx_msgs} 条 / {sess.rx_bytes/2/SAMPLE_RATE:.1f}s 音频 "
                              f"（最近一条 {len(msg)}B）")
                    sess.buf.extend(msg)
                    # 设备会把多帧合并成一个 WS 消息发（见 main.rs 的上行批处理），
                    # 所以时长必须按实际字节数折算。原来硬编码 `+= FRAME_MS`
                    # 假设「一个消息 = 一帧」，批量之后就会把时长算少一半，
                    # VAD 的静音判定跟着失准。
                    n_frames = max(1, len(msg) // FRAME_BYTES)
                    ms = n_frames * FRAME_MS
                    e = rms(msg)
                    if e > VAD_RMS_THRESHOLD:
                        sess.hot_ms += ms
                        sess.silence_ms = 0
                        # 要连续 96ms（3 帧）够响才算「开始说话」。
                        # 碰桌子、敲键盘、纸张声都只有一两帧，靠这个滤掉，
                        # 否则静音计时会被它们永远清零。
                        if sess.hot_ms >= VAD_ONSET_MS:
                            sess.in_speech = True
                        if sess.in_speech:
                            sess.speech_ms += ms
                    else:
                        sess.hot_ms = 0
                        if sess.in_speech:
                            sess.silence_ms += ms

                    done = (sess.in_speech and sess.silence_ms >= VAD_SILENCE_MS) \
                        or sess.speech_ms >= VAD_MAX_UTTERANCE_MS
                    # 兜底：buf 超过 6 秒还没触发 done（环境吵、VAD 一直判在说话），
                    # 直接处理这一段。不设上限的话 buf 会无限增长，ASR 也越来越慢。
                    #
                    # 从 10 秒压到 6 秒：原来环境里一点风扇声就能让静音计时永远
                    # 清零，每句话都要硬等 10 秒才开始识别 —— 用户原话「好慢好慢」。
                    # 正常情况走不到这条路径（说完 450ms 就该触发），它只是兜底。
                    if not done and len(sess.buf) > SAMPLE_RATE * 2 * 6:
                        print("  [vad] 缓冲区超过 6 秒，强制切分")
                        done = True
                    if done:
                        if not sess.busy:
                            await handle_utterance(ws, sess, http, asr, tts)
                        else:
                            sess.reset_utterance()
                else:
                    await handle_json(msg)

                if sess.awake and time.time() - sess.last_talk > AWAKE_TIMEOUT_S:
                    print("[wake] 90 秒无交互，回到待机")
                    sess.awake = False
        except ConnectionClosed as e:
            # 关闭码比什么都好使：
            #   1006 = 对端没发 close 帧、TCP 直接断了（RST / 掉线 / 被踢）
            #   1000/1001 = 有一方正常发了 close 帧
            print(f"[conn] 异常断开 code={e.code} reason={e.reason!r}")
        finally:
            code = getattr(ws, "close_code", None)
            print(f"[conn] 设备断开 {peer} close_code={code}")


async def main_server(asr, tts):
    print(f"语音网关 ws://0.0.0.0:{LISTEN_PORT}")
    print(f"  ASR : {' → '.join(b.name for b in asr.backends)}")
    print(f"  LLM : {LLM_BACKEND}")
    print(f"  TTS : {' → '.join(b.name for b in tts.backends)}")

    # 预热：所有本地 ONNX 模型的第一次推理都特别慢（懒加载 + 图优化），
    # 不预热的话用户说的第一句话要多等 1-3 秒。启动时各烧一次，输出丢掉。
    for b in asr.backends:
        if isinstance(b, (LocalSenseVoiceAsr, LocalSenseVoiceLiteAsr)):
            try:
                t0 = time.time()
                await b.transcribe(None, b"\x00" * (SAMPLE_RATE * 2))
                print(f"  [warmup] 本地 SenseVoice 预热完成 {(time.time()-t0)*1000:.0f}ms")
            except Exception as e:
                print(f"  [warmup] 本地 SenseVoice 预热失败：{e}")

    for b in tts.backends:
        if isinstance(b, (PiperTts, KokoroTts)):
            try:
                async with aiohttp.ClientSession() as http:
                    t0 = time.time()
                    await b.synth(http, "预热")
                print(f"  [warmup] {b.name} 预热完成 {(time.time()-t0)*1000:.0f}ms")
            except Exception as e:
                print(f"  [warmup] {b.name} 预热失败：{e}")

    async with websockets.serve(
        lambda ws: device_handler(ws, asr, tts),
        "0.0.0.0", LISTEN_PORT,
        max_size=1024 * 1024,
        # ⚠️ 必须关掉 WS 控制帧的 ping。
        #
        # 实测（2026-10-28）：网关每 5 秒发一次 WS ping，设备**从来不回 pong**，
        # 于是 websockets 库在 ping_timeout 到点后强行断链，网关侧报
        # code=1006 keepalive ping timeout —— 看起来像设备掉线，其实是网关自己踢的。
        # 换成一个什么都不做的极简服务器也一样，确认是设备端的问题。
        #
        # 保活改由设备端负责：它每 8 秒发一个 {"type":"hb"} 文本帧（main.rs），
        # 网关收到直接忽略。所以这边不需要也不应该再发 ping。
        ping_interval=None,
    ):
        await asyncio.Future()


# ============================================================================
# 命令
# ============================================================================

async def cmd_check(asr, tts):
    print("=" * 52)
    print("后端可用性检查（不产生费用）")
    print("=" * 52)
    print(f"ASR : {' → '.join(b.name for b in asr.backends)}")
    print(f"LLM : {LLM_BACKEND}")
    print(f"TTS : {' → '.join(b.name for b in tts.backends)}")

    print("\n[配置]")
    ok = lambda b: "已配置" if b else "未配置"
    print(f"  SILICONFLOW_API_KEY : {ok(SF_KEY)}" + (f" ({SF_KEY[:8]}…)" if SF_KEY else " ← 云端 ASR/TTS 备份，可选"))
    if LLM_BACKEND == "deepseek":
        print(f"  DEEPSEEK_API_KEY    : {ok(DS_KEY)} ← LLM 必需")
    print(f"  Piper 模型          : {'存在' if PIPER_MODEL.exists() else '缺失'}  {PIPER_MODEL}")
    print(f"  本地 SenseVoice     : {'存在' if (SENSEVOICE_DIR/'model.int8.onnx').exists() else '缺失'}  {SENSEVOICE_DIR}")
    print(f"  Kokoro 本地 TTS     : {'存在' if KOKORO_MODEL.exists() else '缺失'}  {KOKORO_MODEL}")

    local_ok = LocalSenseVoiceAsr().available
    if not SF_KEY:
        if local_ok:
            print("\n提示：没配 SILICONFLOW_API_KEY，但本地 SenseVoice 可用，ASR 不受影响。")
            print("      配一个可以做降级备份（本地模型损坏时自动切云端）。")
        else:
            print("\n⚠️  没配 SILICONFLOW_API_KEY，且本地模型也缺失 —— ASR 完全不可用。")
            print("   二选一：")
            print("     a) 下载本地模型：见 README「本地 ASR」一节（推荐，免费离线）")
            print("     b) 去 cloud.siliconflow.cn 注册 → 实名认证 → 创建 API Key")

    if SF_KEY or (LLM_BACKEND == "deepseek" and DS_KEY):
        print("\n[实测 LLM 调用]")
        try:
            async with aiohttp.ClientSession() as http:
                t0 = time.time()
                r = await chat(http, [{"role": "user", "content": "回复一个字：好"}])
                dt = (time.time() - t0) * 1000
            if "配置" in r or "问题" in r or "网络" in r:
                print(f"  失败：{r}")
                print(f"  如果是 HTTP 400，多半是模型名不对。去硅基流动「模型广场」复制准确的 ID，")
                print(f"  写进 .env 的 SF_LLM_MODEL。当前用的是：{SF_LLM_MODEL}")
            else:
                print(f"  模型 {SF_LLM_MODEL} 正常，回：{r[:40]}（{dt:.0f}ms）")
        except Exception as e:
            print(f"  失败：{e}")

    if PIPER_MODEL.exists():
        print("\n[实测 Piper 本地合成]")
        try:
            async with aiohttp.ClientSession() as http:
                t0 = time.time()
                pcm = await PiperTts().synth(http, "测试一下本地语音合成")
                dt = (time.time() - t0) * 1000
            out = HERE / "piper_test.wav"
            save_wav(out, pcm)
            print(f"  生成 {len(pcm)/2/SAMPLE_RATE:.2f}s 音频，耗时 {dt:.0f}ms（含首次加载模型）")
            print(f"  已存 {out}，双击试听")

            # 顺手做一次闭环：把刚才合成的音频丢给本地 ASR 识别回来
            if LocalSenseVoiceAsr().available:
                print("\n[实测本地 SenseVoice 闭环]")
                t0 = time.time()
                hyp = await LocalSenseVoiceAsr().transcribe(http, pcm)
                dt = (time.time() - t0) * 1000
                print(f"  原文：测试一下本地语音合成")
                print(f"  识别：{hyp}（{dt:.0f}ms）")
        except Exception as e:
            print(f"  失败：{e}")

    print("\n检查结束。")


async def text_mode(asr, tts):
    print("文本调试模式。输入问题回车，q 退出。")
    print(f"TTS 链：{' → '.join(b.name for b in tts.backends)}\n")
    sess = Session(awake=True)
    async with aiohttp.ClientSession() as http:
        while True:
            try:
                line = input("你> ").strip()
            except (EOFError, KeyboardInterrupt):
                break
            if not line:
                continue
            if line in ("q", "quit", "exit"):
                break

            sess.history.append({"role": "user", "content": line})
            sess.history = sess.history[-12:]
            reply = await chat(http, sess.history)
            print(f"AI> {reply}")
            sess.history.append({"role": "assistant", "content": reply})

            try:
                audio = await tts.synth(http, reply)
            except Exception as e:
                print(f"    TTS 失败：{e}")
                continue
            save_wav(HERE / "out.wav", audio)
            print(f"    （{len(audio)/2/SAMPLE_RATE:.1f}s，已存 out.wav）")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--text", action="store_true", help="文本调试，不需要硬件")
    ap.add_argument("--check", action="store_true", help="检查后端可用性，不产生费用")
    args = ap.parse_args()

    try:
        asr, tts = build_backends()
        if args.check:
            asyncio.run(cmd_check(asr, tts))
        elif args.text:
            asyncio.run(text_mode(asr, tts))
        else:
            asyncio.run(main_server(asr, tts))
    except KeyboardInterrupt:
        print("\n已退出")
    except RuntimeError as e:
        print(f"\n启动失败：{e}")
        sys.exit(1)
