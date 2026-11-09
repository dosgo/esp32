# -*- coding: utf-8 -*-
"""Generate test audio via DashScope qwen3-tts and play it through the
PC speakers to exercise the ESP32-S3 voice assistant end to end."""
import json
import os
import sys
import time
import wave
import array
import urllib.request
import winsound

BASE = "https://ws-ebekoepr6y8ojg9x.cn-beijing.maas.aliyuncs.com"
# 密钥从环境变量读取，不要写死在代码里
KEY = os.environ.get("DASHSCOPE_KEY", "")
if not KEY:
    raise SystemExit("请先设置环境变量 DASHSCOPE_KEY（到百炼控制台获取）")
OUT = r"D:\esp32\esp32s3-voice-idf\test_audio"
GAIN = 10  # amplify so the mic across the desk hears it


def synth(text: str, path: str) -> None:
    body = json.dumps({
        "model": "qwen3-tts-flash",
        "input": {"text": text, "voice": "Cherry", "format": "pcm", "sample_rate": 16000},
    }).encode("utf-8")
    req = urllib.request.Request(
        BASE + "/api/v1/services/aigc/multimodal-generation/generation",
        data=body,
        headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        resp = json.loads(r.read().decode("utf-8"))
    url = resp["output"]["audio"]["url"]
    with urllib.request.urlopen(url, timeout=30) as r:
        data = r.read()
    pcm = data[44:] if data[:4] == b"RIFF" else data
    samples = array.array("h")
    samples.frombytes(pcm)
    for i, s in enumerate(samples):
        v = s * GAIN
        if v > 32767:
            v = 32767
        elif v < -32768:
            v = -32768
        samples[i] = v
    # Upsample to 48 kHz stereo: Windows attenuates 16 kHz mono playback
    # heavily (measured 15 dB), which made the mic hear only the noise floor.
    up = array.array("h")
    for i in range(len(samples)):
        a = samples[i]
        b = samples[i + 1] if i + 1 < len(samples) else a
        up.append(a)
        up.append((a * 2 + b) // 3)
        up.append((a + b * 2) // 3)
    st = array.array("h")
    for v in up:
        st.append(v)
        st.append(v)
    with wave.open(path, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(48000)
        w.writeframes(st.tobytes())
    print("generated", path, len(samples) // 16, "ms")


def play(path: str) -> None:
    winsound.PlaySound(path, winsound.SND_FILENAME)
    time.sleep(0.3)


if __name__ == "__main__":
    wake = OUT + r"\wake.wav"
    query = OUT + r"\query.wav"
    synth("小特小特", wake)
    synth("现在几点了", query)
    print("PLAY wake", flush=True)
    play(wake)
    time.sleep(2.5)
    print("PLAY query", flush=True)
    play(query)
    time.sleep(1.0)
    print("DONE")
