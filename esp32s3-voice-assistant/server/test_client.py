#!/usr/bin/env python3
"""
设备模拟器 —— 没有硬件时验证网关的完整链路

它做的事和固件完全一样：
  1. 连上去，发 hello
  2. 发一段"说话声"
  3. 发 0.8 秒静音触发端点检测
  4. 打印网关回来的所有消息（STT / LLM / TTS / 下行音频）

两种音频来源：
  python test_client.py           正弦波 —— 只验证 VAD 和协议，不验证识别
  python test_client.py --voice   用本地 Piper 合成真实中文语音 —— 验证完整链路
  python test_client.py --voice "北京今天天气怎么样"

先另开一个窗口跑 python gateway.py。
"""
import argparse
import asyncio
import json
import math
import struct

import websockets

URL = "ws://127.0.0.1:8765"
RATE = 16000
FRAME = 1920  # 60ms


def tone(ms: int, freq: int = 300, amp: int = 6000) -> bytes:
    n = RATE * ms // 1000
    out = bytearray()
    for i in range(n):
        out += struct.pack("<h", int(amp * math.sin(2 * math.pi * freq * i / RATE)))
    return bytes(out)


def silence(ms: int) -> bytes:
    return b"\x00" * (RATE * 2 * ms // 1000)


async def make_voice(text: str) -> bytes:
    """用本地 Piper 造一段真实语音。没有 Piper 就退回正弦波。"""
    try:
        import aiohttp
        from gateway import PiperTts
    except Exception as e:
        print(f"  (无法加载 Piper: {e}，改用正弦波)")
        return tone(1200)
    t = PiperTts()
    if not t.available:
        print("  (Piper 模型不存在，改用正弦波)")
        return tone(1200)
    async with aiohttp.ClientSession() as http:
        pcm = await t.synth(http, text)
    print(f"  (Piper 合成 {len(pcm)/2/RATE:.2f}s 语音：{text})")
    return pcm


async def feed(ws, pcm: bytes):
    """按 60ms 一帧发出去，节奏和真机一致"""
    if len(pcm) % FRAME:
        pcm += b"\x00" * (FRAME - len(pcm) % FRAME)
    for i in range(0, len(pcm), FRAME):
        await ws.send(pcm[i:i + FRAME])
        await asyncio.sleep(0.06)


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--voice", nargs="+", default=None,
                    metavar="TEXT",
                    help='用真实语音测试完整链路，可给多句（第一句建议是唤醒词）。'
                         '例：--voice "你好小智" "北京今天天气怎么样"')
    ap.add_argument("--gap", type=float, default=9.0,
                    help="多轮之间等多久（秒），要够 TTS 播完，默认 9")
    args = ap.parse_args()

    got_audio = 0
    got_msgs = []

    async with websockets.connect(URL, max_size=1024 * 1024) as ws:
        await ws.send(json.dumps({
            "type": "hello", "version": 1, "format": "pcm",
            "sample_rate": RATE, "channels": 1, "bits": 16, "frame_ms": 60,
        }))

        async def reader():
            nonlocal got_audio
            async for msg in ws:
                if isinstance(msg, bytes):
                    got_audio += len(msg)
                else:
                    got_msgs.append(msg)
                    print(f"  <- {msg}")

        rtask = asyncio.create_task(reader())

        if args.voice:
            turns = args.voice
            print(f"[模式] 真实语音，{len(turns)} 轮")
        else:
            turns = None
            print(f"[模式] 正弦波（只测 VAD/协议）")

        for idx, text in enumerate(turns or ["<tone>"]):
            if turns:
                print(f"\n── 第 {idx+1} 轮：{text} ──")
                speech = await make_voice(text)
            else:
                speech = tone(1200)

            print("发语音 …")
            await feed(ws, speech)
            print("发 0.8 秒静音（触发端点检测）…")
            await feed(ws, silence(800))

            print("等网关处理 …")
            await asyncio.sleep(args.gap)
        rtask.cancel()

    print("-" * 46)
    print(f"收到 JSON 消息 {len(got_msgs)} 条，下行音频 {got_audio} 字节"
          f"（{got_audio/2/RATE:.2f}s）")
    if got_audio:
        print("✅ 完整链路通了：语音 → 识别 → 唤醒 → 回答 → 合成 → 下发")
    else:
        print("提示：没有下行音频。")
        print("  · 如果第一句不是唤醒词（如「你好小智」），这是正常的——网关会忽略")
        print("  · 如果已经唤醒还是没音频，往上翻网关日志看断在哪一步")


if __name__ == "__main__":
    asyncio.run(main())
