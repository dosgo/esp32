#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
接收 ESP32-S3 ADB 代理的网络日志。

为什么需要这个：ESP32-S3 的 USB-Serial-JTAG 和原生 USB OTG 共用 GPIO19/20，
固件一进 host 模式，串口通道就没了。所以日志改成 UDP 广播到 4444 端口。

用法：
    python tools/netlog_listen.py                # 监听默认 4444
    python tools/netlog_listen.py --port 4444
    python tools/netlog_listen.py --save log.txt # 同时落盘

注意：
- 电脑和 ESP32 必须在同一网段（UDP 广播不出网段）
- Windows 防火墙第一次会弹窗，选「允许访问」
- 日志只在 WiFi 连上之后才开始吐；上电到联网之间的日志会在联网后一次性补发
"""
import argparse
import socket
import sys
import time

DEFAULT_PORT = 4444

TAG_COLOR = {
    "E": "\033[91m",   # 红
    "W": "\033[93m",   # 黄
    "I": "\033[0m",    # 默认
    "D": "\033[90m",   # 灰
    "T": "\033[90m",
}
RESET = "\033[0m"


def colorize(line: str) -> str:
    if sys.platform == "win32":
        # Windows 老终端不认 ANSI，直接原样输出
        return line
    if line.startswith("[") and len(line) > 2:
        tag = line[1]
        code = TAG_COLOR.get(tag)
        if code:
            return code + line + RESET
    return line


def main() -> int:
    ap = argparse.ArgumentParser(description="ESP32-S3 ADB proxy 网络日志接收器")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT, help=f"UDP 端口（默认 {DEFAULT_PORT}）")
    ap.add_argument("--bind", default="0.0.0.0", help="绑定地址（默认 0.0.0.0）")
    ap.add_argument("--save", default=None, help="同时写入指定文件")
    ap.add_argument("--no-color", action="store_true", help="关闭颜色")
    args = ap.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind((args.bind, args.port))
    except OSError as e:
        print(f"绑定 {args.bind}:{args.port} 失败：{e}", file=sys.stderr)
        return 1

    print(f"监听 UDP :{args.port} —— 给 ESP32 上电，联网后日志会开始滚动")
    print("（若长时间无输出：检查 WiFi 账号密码、电脑与 ESP32 是否同网段、防火墙）")
    print("-" * 60)

    fh = open(args.save, "a", encoding="utf-8") if args.save else None
    src_seen = set()

    try:
        while True:
            data, addr = sock.recvfrom(2048)
            now = time.strftime("%H:%M:%S")
            if addr[0] not in src_seen:
                src_seen.add(addr[0])
                print(f"[{now}] -- 来自 {addr[0]} --")
            try:
                text = data.decode("utf-8", errors="replace")
            except Exception:
                text = repr(data)
            line = text.rstrip("\r\n")
            out = f"[{now}] {line}"
            if args.no_color:
                print(out, flush=True)
            else:
                print(colorize(out), flush=True)
            if fh:
                fh.write(out + "\n")
                fh.flush()
    except KeyboardInterrupt:
        print("\n已停止")
    finally:
        if fh:
            fh.close()
        sock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
