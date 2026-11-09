# -*- coding: utf-8 -*-
"""播放唤醒词 + 提问，同时抓串口日志，验证一轮完整对话。"""
import sys
import threading
import time
import winsound

import serial

PORT = sys.argv[1] if len(sys.argv) > 1 else "COM4"
BASE = r"D:\esp32\esp32s3-voice-idf\test_audio"
READY_WAIT = float(sys.argv[2]) if len(sys.argv) > 2 else 18.0

s = serial.Serial(PORT, 115200, timeout=0.2)
s.dtr = False
s.rts = False
s.reset_input_buffer()


def reader():
    while True:
        try:
            d = s.read(4096)
        except Exception:
            return
        if d:
            sys.stdout.write(d.decode("utf-8", "replace"))
            sys.stdout.flush()


threading.Thread(target=reader, daemon=True).start()
print(f"=== 等待设备就绪 {READY_WAIT:.0f}s ===", flush=True)
time.sleep(READY_WAIT)

print("=== 播放唤醒词 小特小特 ===", flush=True)
winsound.PlaySound(BASE + r"\wake.wav", winsound.SND_FILENAME)
time.sleep(2.5)

print("=== 播放提问 现在几点了 ===", flush=True)
winsound.PlaySound(BASE + r"\query48.wav", winsound.SND_FILENAME)

time.sleep(30)
s.close()
print("=== 结束 ===", flush=True)
