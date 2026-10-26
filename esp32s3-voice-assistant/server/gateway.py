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

# ---- 音频参数（和设备端固件一致，别改）----
SAMPLE_RATE = 16000
FRAME_MS = 60
FRAME_BYTES = SAMPLE_RATE * 2 * FRAME_MS // 1000   # 1920
DOWNLINK_BYTES = 1920
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

# ---- 唤醒词 ----
WAKE_WORDS = ["你好小智", "小智小智", "嗨小智", "嘿小智", "小智"]
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
    "回答要短：一般不超过两句、60 个字以内，因为你的回答会被转成语音播放。"
    "不要用 Markdown、不要用列表、不要输出表情符号。"
    "不确定的事情直接说不知道，不要编。"
)

# ---- VAD ----
VAD_RMS_THRESHOLD = 500
VAD_SILENCE_MS = 700
VAD_MIN_SPEECH_MS = 300
VAD_MAX_UTTERANCE_MS = 15000
AWAKE_TIMEOUT_S = 90


# ============================================================================
# 工具
# ============================================================================

def rms(pcm: bytes) -> int:
    n = len(pcm) // 2
    if n == 0:
        return 0
    total = 0
    for i in range(0, n * 2, 2):
        s = struct.unpack_from("<h", pcm, i)[0]
        total += s * s
    return int((total / n) ** 0.5)


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
    """线性插值重采样。语音够用，且不依赖 audioop（3.13 里已被移除）。"""
    if src == dst or not pcm:
        return pcm
    import numpy as np
    x = np.frombuffer(pcm, dtype="<i2")
    if x.size == 0:
        return b""
    n = int(x.size * dst / src)
    idx = np.linspace(0, x.size - 1, n)
    y = np.interp(idx, np.arange(x.size), x.astype(np.float32))
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
        self.available = bool(SF_KEY)

    async def transcribe(self, http, pcm):
        if not SF_KEY:
            return ""
        form = aiohttp.FormData()
        form.add_field("file", pcm_to_wav(pcm), filename="a.wav", content_type="audio/wav")
        form.add_field("model", SF_ASR_MODEL)
        try:
            async with http.post(
                f"{SF_BASE}/audio/transcriptions", data=form,
                headers={"Authorization": f"Bearer {SF_KEY}"},
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
        self.available = bool(SF_KEY)

    async def synth(self, http, text):
        if not SF_KEY:
            raise RuntimeError("没配 SILICONFLOW_API_KEY")
        async with http.post(
            f"{SF_BASE}/audio/speech",
            json={
                "model": SF_TTS_MODEL,
                "voice": SF_TTS_VOICE,
                "input": text,
                "response_format": "pcm",
                "sample_rate": SAMPLE_RATE,
                "speed": 1.0,
            },
            headers={"Authorization": f"Bearer {SF_KEY}", "Content-Type": "application/json"},
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
        arr = np.concatenate([c.audio_float_array for c in chunks])
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
    if LLM_BACKEND == "deepseek":
        if not DS_KEY:
            return "还没配置 DEEPSEEK_API_KEY。"
        url, key, model = f"{DS_BASE}/chat/completions", DS_KEY, DS_MODEL
    else:
        if not SF_KEY:
            return "还没配置 SILICONFLOW_API_KEY。"
        url, key, model = f"{SF_BASE}/chat/completions", SF_KEY, SF_LLM_MODEL

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
    in_speech: bool = False
    busy: bool = False

    def reset_utterance(self):
        self.buf.clear()
        self.speech_ms = 0
        self.silence_ms = 0
        self.in_speech = False


async def send_json(ws, obj):
    try:
        await ws.send(json.dumps(obj, ensure_ascii=False))
    except Exception:
        pass


async def send_pcm(ws, pcm: bytes):
    for i in range(0, len(pcm), DOWNLINK_BYTES):
        chunk = pcm[i:i + DOWNLINK_BYTES]
        if len(chunk) < DOWNLINK_BYTES:
            chunk += b"\x00" * (DOWNLINK_BYTES - len(chunk))
        try:
            await ws.send(chunk)
        except Exception:
            return
        await asyncio.sleep(FRAME_MS / 1000 * 0.95)


async def respond(ws, sess, http, tts, user_text: str):
    sess.busy = True
    try:
        sess.history.append({"role": "user", "content": user_text})
        sess.history = sess.history[-12:]

        reply = await chat(http, sess.history)
        print(f"  [llm] {reply}")
        await send_json(ws, {"type": "llm", "text": reply})

        try:
            audio = await tts.synth(http, reply)
        except Exception as e:
            print(f"  [tts] 全部失败：{e}")
            await send_json(ws, {"type": "tts", "state": "stop"})
            return

        sess.history.append({"role": "assistant", "content": reply})
        await send_json(ws, {"type": "tts", "state": "start"})
        await send_pcm(ws, audio)
        await send_json(ws, {"type": "tts", "state": "stop"})
    finally:
        sess.busy = False


async def handle_utterance(ws, sess, http, asr, tts):
    if len(sess.buf) < SAMPLE_RATE * 2 * VAD_MIN_SPEECH_MS // 1000:
        sess.reset_utterance()
        return

    pcm = bytes(sess.buf)
    print(f"  [vad] 一段语音 {len(pcm)/2/SAMPLE_RATE:.2f}s，送去识别")
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

    if not sess.awake:
        if match_wake(text):
            sess.awake = True
            sess.last_talk = time.time()
            print("  [wake] 唤醒成功")
            await send_json(ws, {"type": "ack"})
            await respond(ws, sess, http, tts, "（用户刚叫了你，用一句话简短回应，比如『我在』）")
        else:
            print("  [wake] 未命中唤醒词，忽略")
        return

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
    asr = AsrChain([asr_map[n]() for n in ASR_CHAIN if n in asr_map])
    chain = TtsChain([tts_map[n]() for n in TTS_CHAIN if n in tts_map])
    return asr, chain


async def device_handler(ws, asr, tts):
    peer = ws.remote_address
    print(f"\n[conn] 设备接入 {peer}")
    sess = Session()

    async with aiohttp.ClientSession() as http:
        async def handle_json(raw: str):
            try:
                j = json.loads(raw)
            except Exception:
                return
            if j.get("type") == "hello":
                print(f"[hello] {j.get('format')} {j.get('sample_rate')}Hz "
                      f"{j.get('channels')}ch {j.get('frame_ms')}ms")
                await send_json(ws, {
                    "type": "hello", "transport": "websocket",
                    "audio_params": {"format": "pcm", "sample_rate": SAMPLE_RATE},
                })
            elif j.get("type") == "abort":
                sess.reset_utterance()

        try:
            async for msg in ws:
                if isinstance(msg, bytes):
                    sess.buf.extend(msg)
                    e = rms(msg)
                    if e > VAD_RMS_THRESHOLD:
                        sess.speech_ms += FRAME_MS
                        sess.silence_ms = 0
                        sess.in_speech = True
                    elif sess.in_speech:
                        sess.silence_ms += FRAME_MS

                    done = (sess.in_speech and sess.silence_ms >= VAD_SILENCE_MS) \
                        or sess.speech_ms >= VAD_MAX_UTTERANCE_MS
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
        except ConnectionClosed:
            pass
        finally:
            print(f"[conn] 设备断开 {peer}")


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
        max_size=1024 * 1024, ping_interval=20, ping_timeout=20,
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
