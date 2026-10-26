#!/usr/bin/env python3
"""
本地 ASR 闭环测试 —— 不需要麦克风

思路：用本地 TTS（Piper）造一句中文语音，再喂给本地 ASR（SenseVoice）识别，
对比原文。两个模型互为"考官"，如果识别结果和原文基本一致，
说明这条纯本地链路是通的，而且能顺便量出识别耗时。

用法：
    python test_local_asr.py
"""
import asyncio
import time

import aiohttp

from gateway import (
    LocalSenseVoiceAsr,
    PiperTts,
    SAMPLE_RATE,
    load_env,
)

load_env()

CASES = [
    "你好小智",
    "北京今天天气怎么样",
    "帮我定一个明天早上七点的闹钟",
    "播放一首周杰伦的歌",
    "现在几点了",
]


def cjk_len(s: str) -> int:
    return len([c for c in s if "\u4e00" <= c <= "\u9fff"])


def cer(ref: str, hyp: str) -> float:
    """字错率（简化版，按字符集合差异估算，够用来看趋势）"""
    import difflib
    sm = difflib.SequenceMatcher(None, ref, hyp)
    same = sum(b.size for b in sm.get_matching_blocks())
    return max(0.0, 1 - same / max(1, len(ref)))


async def main():
    asr = LocalSenseVoiceAsr()
    if not asr.available:
        print("本地 SenseVoice 模型不存在，先下载：")
        print("  见 README 第三节「本地 ASR」")
        return

    tts = PiperTts()
    if not tts.available:
        print("Piper 模型不存在，没法造测试音频")
        return

    # 预热：两个模型第一次都要加载
    print("预热模型 …")
    t0 = time.time()
    async with aiohttp.ClientSession() as http:
        await tts.synth(http, "预热")
        await asr.transcribe(http, b"\x00" * (SAMPLE_RATE * 2))
    print(f"预热完成 {(time.time()-t0):.1f}s\n")

    print(f"{'原文':<22} {'识别结果':<24} {'字错率':>6} {'耗时':>7}")
    print("-" * 66)

    total_cer = 0.0
    total_ms = 0.0
    async with aiohttp.ClientSession() as http:
        for text in CASES:
            pcm = await tts.synth(http, text)
            audio_sec = len(pcm) / 2 / SAMPLE_RATE

            t0 = time.time()
            hyp = await asr.transcribe(http, pcm)
            dt = (time.time() - t0) * 1000

            e = cer(text, hyp)
            total_cer += e
            total_ms += dt
            print(f"{text:<22} {hyp:<24} {e:>6.0%} {dt:>6.0f}ms")

    n = len(CASES)
    print("-" * 66)
    print(f"平均字错率 {total_cer/n:.0%}，平均识别耗时 {total_ms/n:.0f}ms")
    print("\n注：这里的『原文』是喂给 TTS 的文字，识别结果可能只差标点、")
    print("    或者把数字读成了汉字，属于正常。字错率 20% 以内都算通。")


if __name__ == "__main__":
    asyncio.run(main())
