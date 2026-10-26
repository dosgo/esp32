#!/usr/bin/env python3
"""
SenseVoice 纯 Python 推理 —— 不依赖 sherpa-onnx

# 为什么需要这个

sherpa-onnx 的 Python 包在 PyPI 上是 manylinux wheel（要 glibc），
而安卓 Termux 用的是 bionic libc，**装不上**。但模型本身是开源的 ONNX 文件，
所有前处理参数也都写在模型元数据里，所以完全可以自己用 onnxruntime 跑。

# 推理链路（对齐 sherpa-onnx 的 offline-recognizer-sense-voice-impl.h）

    16kHz PCM
      → kaldi fbank（80 维，hamming 窗，25ms/10ms，snip_edges）
      → LFR（7 帧堆叠，步长 6）→ 560 维
      → CMVN（用模型元数据里的 neg_mean / inv_stddev）
      → ONNX 推理 → CTC logits [T, 25055]
      → 贪心解码 + 去重去 blank → tokens.txt 反查

# 手机上需要的依赖

    pip install onnxruntime numpy

就这两个。kaldi-native-fbank 只在开发期用来校验特征一致性，手机上不需要。
"""

from pathlib import Path

import numpy as np

SAMPLE_RATE = 16000
FEAT_DIM = 80           # fbank 维数
LFR_M = 7               # 每个输出帧堆叠的输入帧数
LFR_N = 6               # 相邻输出帧的步长
BLANK_ID = 0


# ============================================================================
# 特征提取（纯 numpy，对齐 kaldi）
# ============================================================================

def _mel_scale(f):
    return 1127.0 * np.log(1.0 + np.asarray(f) / 700.0)


def _inverse_mel_scale(m):
    return 700.0 * (np.exp(np.asarray(m) / 1127.0) - 1.0)


def _mel_filterbank(n_mels=FEAT_DIM, n_fft=512, sample_rate=SAMPLE_RATE,
                    low_freq=20.0, high_freq=0.0):
    """
    构造 kaldi 风格的三角 mel 滤波器组。

    kaldi 的细节（和 librosa 不同，容易搞错）：
      * high_freq == 0 时取 nyquist
      * 每个 bin 的中心频率用 fft bin 的离散频率 i*fs/n_fft
      * 三角形左边界是闭区间、右边界是开区间（第一个 bin 特殊处理）
    """
    if high_freq <= 0:
        high_freq = sample_rate / 2.0

    num_bins = n_fft // 2 + 1
    mel_low = _mel_scale(low_freq)
    mel_high = _mel_scale(high_freq)
    # kaldi 在 mel 轴均分 n_mels+2 个点（含两端）
    mel_points = mel_low + (mel_high - mel_low) * np.arange(n_mels + 2) / (n_mels + 1)
    hz_points = _inverse_mel_scale(mel_points)

    # fft bin 的实际频率
    fft_freqs = np.arange(num_bins) * sample_rate / n_fft

    fb = np.zeros((n_mels, num_bins), dtype=np.float32)
    for m in range(n_mels):
        left, center, right = hz_points[m], hz_points[m + 1], hz_points[m + 2]
        for i in range(num_bins):
            f = fft_freqs[i]
            if f > left and f < right:
                if f <= center:
                    fb[m, i] = (f - left) / (center - left)
                else:
                    fb[m, i] = (right - f) / (right - center)
    return fb


def compute_fbank(samples: np.ndarray) -> np.ndarray:
    """
    16kHz float32 mono → (num_frames, 80) 的 log mel 特征

    对齐 kaldi 的默认 fbank 配置：
        window_type   = hamming
        frame_length  = 25ms (400 点)
        frame_shift   = 10ms (160 点)
        dither        = 0
        preemph       = 0.97
        remove_dc     = True
        snip_edges    = True
        high_freq     = 0 (=nyquist)
    """
    samples = np.asarray(samples, dtype=np.float32).ravel()
    frame_len, frame_shift, n_fft = 400, 160, 512

    # 1) 预加重
    x = samples.copy()
    if len(x) > 1:
        x[1:] = x[1:] - 0.97 * x[:-1]

    # 2) 分帧（snip_edges=True：不足一帧的尾巴丢掉）
    if len(x) < frame_len:
        return np.zeros((0, FEAT_DIM), dtype=np.float32)
    num_frames = 1 + (len(x) - frame_len) // frame_shift
    idx = np.arange(frame_len)[None, :] + frame_shift * np.arange(num_frames)[:, None]
    frames = x[idx]

    # 3) 去直流
    frames = frames - frames.mean(axis=1, keepdims=True)

    # 4) hamming 窗（kaldi 用 2*pi*n/(N-1)）
    n = np.arange(frame_len)
    window = (0.54 - 0.46 * np.cos(2 * np.pi * n / (frame_len - 1))).astype(np.float32)
    frames = frames * window

    # 5) FFT → 功率谱
    spec = np.fft.rfft(frames, n=n_fft, axis=1)
    power = (np.abs(spec) ** 2).astype(np.float32)

    # 6) mel 滤波 → log
    fb = _mel_filterbank(n_mels=FEAT_DIM, n_fft=n_fft)
    mel = power @ fb.T
    mel = np.log(np.maximum(mel, 1.0e-10))
    return mel.astype(np.float32)


def apply_lfr(feat: np.ndarray, m: int = LFR_M, n: int = LFR_N) -> np.ndarray:
    """
    低帧率：每个输出帧 = 连续 m 个输入帧拼起来，步长 n。
    首帧左补、末帧右补，保证每个窗口完整（对齐 sherpa-onnx 的 lfr.h）。
    """
    T = feat.shape[0]
    if T == 0:
        return np.zeros((0, feat.shape[1] * m), dtype=np.float32)

    left = (m - 1) // 2
    padded = np.concatenate(
        [np.repeat(feat[:1], left, axis=0), feat, np.repeat(feat[-1:], m - 1 - left, axis=0)],
        axis=0,
    )
    num_out = int(np.ceil(T / n))
    out = np.empty((num_out, feat.shape[1] * m), dtype=np.float32)
    for i in range(num_out):
        s = i * n
        out[i] = padded[s:s + m].reshape(-1)
    return out


def apply_cmvn(feat: np.ndarray, neg_mean: np.ndarray, inv_stddev: np.ndarray) -> np.ndarray:
    """(x + neg_mean) * inv_stddev，逐维"""
    return ((feat + neg_mean[None, :]) * inv_stddev[None, :]).astype(np.float32)


# ============================================================================
# 主类
# ============================================================================

def _parse_floats(s: str) -> np.ndarray:
    # 模型元数据里是逗号分隔、无括号的一长串，比如 "-8.31,-8.60,..."
    s = s.strip().strip("[]")
    return np.array([float(x) for x in s.split(",") if x.strip()], dtype=np.float32)


class SenseVoiceLite:
    """
    纯 Python 的 SenseVoice 推理。

    只需要 onnxruntime + numpy，没有 sherpa-onnx、没有 PyTorch、没有 C 扩展，
    所以在 Termux（安卓）上也能跑。
    """

    def __init__(self, model_dir, num_threads: int = 4, verbose: bool = False):
        import onnxruntime as ort

        model_dir = Path(model_dir)
        model_path = model_dir / "model.int8.onnx"
        if not model_path.exists():
            model_path = model_dir / "model.onnx"
        tokens_path = model_dir / "tokens.txt"

        opts = ort.SessionOptions()
        opts.intra_op_num_threads = num_threads
        opts.inter_op_num_threads = 1
        # 手机上线程太多会热降频，这里也保守一点
        opts.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.sess = ort.InferenceSession(str(model_path), opts, providers=["CPUExecutionProvider"])

        md = self.sess.get_modelmeta().custom_metadata_map
        self.neg_mean = _parse_floats(md["neg_mean"])
        self.inv_stddev = _parse_floats(md["inv_stddev"])
        self.lang_zh = int(md.get("lang_zh", 3))
        self.with_itn = int(md.get("with_itn", 14))
        self.without_itn = int(md.get("without_itn", 15))

        # tokens.txt 每行是 "token id"
        self.tokens = []
        with open(tokens_path, encoding="utf-8") as f:
            for line in f:
                parts = line.rstrip("\n").split(" ")
                if len(parts) >= 2:
                    self.tokens.append(parts[0])
                elif parts:
                    self.tokens.append(parts[0])

        if verbose:
            print(f"[sv-lite] 加载完成：{len(self.tokens)} tokens, "
                  f"cmvn dim={len(self.neg_mean)}")

    # ---------- 前处理 ----------

    def _features(self, pcm: bytes) -> np.ndarray:
        samples = np.frombuffer(pcm, dtype="<i2").astype(np.float32) / 32768.0
        f = compute_fbank(samples)
        f = apply_lfr(f, LFR_M, LFR_N)
        f = apply_cmvn(f, self.neg_mean, self.inv_stddev)
        return f

    # ---------- 解码 ----------

    def _ctc_greedy(self, logits: np.ndarray) -> str:
        ids = logits.argmax(axis=-1)
        out = []
        prev = -1
        for i in ids:
            i = int(i)
            if i != prev and i != BLANK_ID:
                out.append(i)
            prev = i

        text = []
        for i in out:
            if i < len(self.tokens):
                t = self.tokens[i]
                # 过滤 <|zh|> <|NEUTRAL|> 这类元信息 token
                if t.startswith("<|") and t.endswith("|>"):
                    continue
                text.append(t)
        s = "".join(text).replace("▁", " ")
        return s.strip()

    # ---------- 对外接口 ----------

    def transcribe(self, pcm: bytes, language: str = "zh") -> str:
        """pcm: 16kHz / 16bit / 单声道 little-endian"""
        feat = self._features(pcm)
        if feat.shape[0] == 0:
            return ""

        lang_id = {"auto": 0, "zh": 3, "en": 4, "yue": 7, "ja": 11, "ko": 12}.get(language, 3)
        x = feat[None, :, :]
        x_len = np.array([feat.shape[0]], dtype=np.int32)
        lang = np.array([lang_id], dtype=np.int32)
        tn = np.array([self.with_itn], dtype=np.int32)

        logits = self.sess.run(None, {"x": x, "x_length": x_len,
                                      "language": lang, "text_norm": tn})[0]
        return self._ctc_greedy(logits[0])


# ============================================================================
# 自测：和 sherpa-onnx 的结果对比
# ============================================================================

if __name__ == "__main__":
    import sys
    import time

    here = Path(__file__).parent
    model_dir = here / "models" / "asr" / "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"

    if not (model_dir / "model.int8.onnx").exists():
        print(f"模型不存在：{model_dir}")
        sys.exit(1)

    asr = SenseVoiceLite(model_dir, verbose=True)

    # 用 test_wavs 里的样例音频
    wavs = sorted((model_dir / "test_wavs").glob("*.wav"))
    if not wavs:
        print("没有找到测试音频")
        sys.exit(1)

    import wave

    print(f"\n{'音频':<28} {'识别结果':<40} {'耗时':>8}")
    print("-" * 80)
    for w in wavs[:6]:
        with wave.open(str(w), "rb") as f:
            assert f.getframerate() == SAMPLE_RATE, f.getframerate()
            pcm = f.readframes(f.getnframes())
        t0 = time.time()
        text = asr.transcribe(pcm)
        dt = (time.time() - t0) * 1000
        print(f"{w.name:<28} {text:<40} {dt:>6.0f}ms")

    print("\n如果上面能出中文，说明纯 Python 链路已经跑通。")
    print("接下来跑 compare_with_sherpa.py 做逐字对比。")
