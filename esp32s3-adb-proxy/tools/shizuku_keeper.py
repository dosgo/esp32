#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Shizuku 保活器 —— 走 ESP32-S3 ADB 代理（USB），让手机上的 App 拿到 adb(shell) 级能力。

背景
----
Shizuku 用 adb 起一个 shell 权限的 Java 进程，App 通过 Binder 调它，
从而无需 root 就能使用 adb 级 API（pm / appops / settings / cmd 等）。

非 root 的 Shizuku 有两个死穴，本脚本两个都能破：

  1) 每次手机重启后都要重新激活
     → 本脚本常驻轮询，掉了就自动重新执行启动命令。

  2) 【关键】关掉 WiFi 后 Shizuku 就没了 —— 这是个死循环：
        想用 Shizuku 关 WiFi → 关了 WiFi → 无线调试被系统自动禁用
        → adb 通道断掉 → Shizuku 服务失效 → 连开回来的能力都没了
     官方原话："Wireless debugging is bound to the network it was paired on.
                Switching Wi-Fi networks - or dropping to mobile data -
                breaks the connection."
     也就是：无线调试方案**永远**关不了 WiFi，它自己会被一起关掉。

     → 本脚本走的是 **USB**（手机 ←USB→ ESP32 ←WiFi→ 电脑），
       手机的 WiFi 开关**完全不影响**这条通道。
       所以关掉 WiFi 后脚本照样能连上、照样能把 Shizuku 拉起来、
       照样能再执行 set-wifi-enabled enabled 把 WiFi 开回来。
       这是 USB 桥独有的能力，纯无线调试做不到。

用法
----
    # 基本
    python shizuku_keeper.py --host 192.168.1.50 --status        # 只看状态
    python shizuku_keeper.py --host 192.168.1.50                 # 启动一次
    python shizuku_keeper.py --host 192.168.1.50 --watch         # 常驻保活

    # 远程开关 WiFi（本方案的杀手锏）
    python shizuku_keeper.py --host 192.168.1.50 --wifi off      # 关 WiFi 后自动复活 Shizuku
    python shizuku_keeper.py --host 192.168.1.50 --wifi on
    python shizuku_keeper.py --host 192.168.1.50 --cycle         # 关→复活→开，验证闭环

    # 手动指定启动命令（Shizuku App → 查看命令，复制过来）
    python shizuku_keeper.py --host 192.168.1.50 --start-cmd "sh /sdcard/Android/data/moe.shizuku.privileged.api/start.sh"

要求
----
- adb 在 PATH 里（Google SDK Platform Tools）
- 手机已开「USB 调试」，且首次连接时在手机上点了「总是允许」
- 手机已安装并打开过一次 Shizuku
"""

import argparse
import subprocess
import sys
import time
from datetime import datetime

PKG = "moe.shizuku.privileged.api"
DATA_DIR = "/sdcard/Android/data/%s" % PKG
OLD_START_SH = "%s/start.sh" % DATA_DIR

CONNECT_TIMEOUT = 15
CMD_TIMEOUT = 20
LONG_TIMEOUT = 40

CONFIG_FILE = ".shizuku_keeper_cmd"


def log(msg):
    print("[%s] %s" % (datetime.now().strftime("%H:%M:%S"), msg), flush=True)


class Adb:
    def __init__(self, serial):
        self.serial = serial

    def _run(self, cmd, timeout):
        try:
            p = subprocess.run(cmd, capture_output=True, timeout=timeout)
        except FileNotFoundError:
            log("找不到 adb，请先安装 SDK Platform Tools 并加进 PATH")
            sys.exit(2)
        except subprocess.TimeoutExpired:
            return ""
        out = p.stdout.decode("utf-8", "replace") + p.stderr.decode("utf-8", "replace")
        return out.replace("\r\n", "\n").replace("\r", "\n").strip()

    def run(self, args, timeout=CMD_TIMEOUT):
        return self._run(["adb", "-s", self.serial] + args, timeout)

    def raw(self, args, timeout=CMD_TIMEOUT):
        return self._run(["adb"] + args, timeout)

    def shell(self, sh_cmd, timeout=CMD_TIMEOUT):
        """执行一条 shell 命令（字符串形式，方便拼 && 多条）"""
        return self.run(["shell", sh_cmd], timeout=timeout)

    # ---------- 状态 ----------

    def state(self):
        for line in self.raw(["devices"]).splitlines()[1:]:
            parts = line.split()
            if len(parts) >= 2 and parts[0] == self.serial:
                return parts[1]
        return None

    def connect(self):
        self.raw(["connect", self.serial], timeout=CONNECT_TIMEOUT)
        return self.state()

    def process_list(self):
        for args in (["shell", "ps", "-A"], ["shell", "ps"]):
            out = self.run(args)
            if out and "PID" in out:
                return out
        return ""

    def shizuku_running(self):
        return "shizuku" in self.process_list().lower()

    def boot_completed(self):
        return self.run(["shell", "getprop", "sys.boot_completed"]) == "1"

    def wifi_state(self):
        """返回 on / off / unknown"""
        out = self.shell("dumpsys wifi | grep -i 'Wi-Fi is'")
        if not out:
            out = self.shell("cmd wifi status | head -5")
        low = out.lower()
        if "enabled" in low:
            return "on"
        if "disabled" in low:
            return "off"
        return "unknown"


# ---------------- 启动命令探测 ----------------
#
# Shizuku 13.6.0 的 release note 写了 "Update the start command"，
# App 里显示的命令变成了一长串 /data/app/~~xxx==/.../libshizuku.so。
# 但实测 13.6.0 上 start.sh 仍然存在（内容是把 starter 拷到 /data/local/tmp 再执行）。
# 不同版本/渠道表现不一致，所以这里做多级探测，而不是写死一条。

def _ls(adb, path):
    out = adb.shell("ls %s 2>/dev/null" % path)
    return [x for x in out.split("\n") if x.strip()]


def detect_start_cmd(adb):
    """返回 (描述, shell命令字符串) 或 (None, None)"""
    # 1) 老方式：数据目录里的 start.sh（≤13.6.0 大多还在）
    entries = _ls(adb, DATA_DIR)
    if "start.sh" in entries:
        return ("start.sh（数据目录）", "sh %s" % OLD_START_SH)

    # 2) 数据目录里的 starter（13.6.0 的 native 启动体）
    if "starter" in entries:
        return (
            "starter（拷到 /data/local/tmp 执行）",
            "cp %s/starter /data/local/tmp/shizuku_starter && "
            "chmod 700 /data/local/tmp/shizuku_starter && "
            "/data/local/tmp/shizuku_starter" % DATA_DIR,
        )

    # 3) 新方式：从 pm path 推出 libshizuku.so 的位置
    pm = adb.shell("pm path %s" % PKG)
    if pm.startswith("package:"):
        base = pm[len("package:"):].strip()
        lib_dir = base.rsplit("/", 1)[0] + "/lib"
        for abi in _ls(adb, lib_dir):
            so = "%s/%s/libshizuku.so" % (lib_dir, abi)
            if _ls(adb, so):
                return (
                    "libshizuku.so（%s，拷到 /data/local/tmp 执行）" % abi,
                    "cp %s /data/local/tmp/shizuku && "
                    "chmod 755 /data/local/tmp/shizuku && "
                    "/data/local/tmp/shizuku" % so,
                )

    # 4) 兜底：还是试老路径（App 可能只是没生成目录）
    return ("start.sh（兜底路径，可能失败）", "sh %s" % OLD_START_SH)


def load_saved_cmd():
    try:
        with open(CONFIG_FILE, "r", encoding="utf-8") as f:
            v = f.read().strip()
            return v or None
    except OSError:
        return None


def save_cmd(cmd):
    try:
        with open(CONFIG_FILE, "w", encoding="utf-8") as f:
            f.write(cmd)
    except OSError:
        pass


def activate(adb, forced_cmd=None):
    """启动 Shizuku。forced_cmd 为用户手动指定的 shell 命令（不含 adb shell 前缀）"""
    if forced_cmd:
        desc, cmd = "用户指定", forced_cmd
    else:
        desc, cmd = detect_start_cmd(adb)
        if desc is None:
            log("探测不到启动命令。请打开 Shizuku App → 查看命令，用 --start-cmd 传进来")
            return False

    log("启动方式：%s" % desc)
    log("执行：%s" % (cmd if len(cmd) < 160 else cmd[:160] + " ..."))
    out = adb.shell(cmd, timeout=LONG_TIMEOUT)
    if out:
        log("  输出：%s" % out.replace("\n", " | ")[:300])

    time.sleep(3)
    ok = adb.shizuku_running()
    log("Shizuku %s" % ("已运行" if ok else "仍未运行"))
    if ok and forced_cmd:
        save_cmd(forced_cmd)
    return ok


# ---------------- WiFi 控制 ----------------

def set_wifi(adb, enable):
    """开关 WiFi。走 cmd（shell 身份有效），svc 需要 root 所以只作兜底"""
    target = "enabled" if enable else "disabled"
    log("设置 WiFi = %s ..." % target)
    out = adb.shell("cmd -w wifi set-wifi-enabled %s" % target, timeout=25)
    if out:
        log("  -> %s" % out.replace("\n", " | ")[:200])
    time.sleep(2)
    return adb.wifi_state()


def ensure_connected(adb, tries=2):
    for i in range(tries):
        st = adb.state()
        if st == "device":
            return True
        if st == "unauthorized":
            log("设备 unauthorized：请在手机上确认「允许 USB 调试」并勾选总是允许")
            return False
        log("未连接（state=%s），adb connect %s ..." % (st, adb.serial))
        adb.connect()
        time.sleep(2)
    return adb.state() == "device"


def cmd_status(adb, args):
    log("adb state = %s" % adb.state())
    if not ensure_connected(adb):
        return 1
    log("boot_completed = %s" % adb.boot_completed())
    log("wifi           = %s" % adb.wifi_state())
    log("shizuku        = %s" % ("running" if adb.shizuku_running() else "NOT running"))
    desc, c = detect_start_cmd(adb)
    log("探测到启动方式 = %s" % desc)
    if args.verbose and c:
        log("  命令：%s" % c)
    return 0


def cmd_wifi(adb, args, enable):
    """关/开 WiFi，之后自动把被连带干掉的 Shizuku 拉回来"""
    if not ensure_connected(adb):
        return 1

    before = adb.shizuku_running()
    log("操作前：shizuku=%s wifi=%s" % (before, adb.wifi_state()))

    st = set_wifi(adb, enable)
    log("WiFi 现在 = %s" % st)

    # adbd 可能因网络变更重启，连接会掉一次，重连即可
    time.sleep(2)
    if not ensure_connected(adb, tries=4):
        log("WiFi 切换后连不上 adb —— 检查手机 USB 调试是否还在")
        return 1

    after = adb.shizuku_running()
    log("操作后：shizuku=%s" % after)

    if before and not after:
        log("Shizuku 被 WiFi 切换连带干掉了 —— 通过 USB 桥重新拉起（这是无线调试方案做不到的）")
        activate(adb, args.start_cmd)
    elif not after:
        log("Shizuku 未运行，尝试启动")
        activate(adb, args.start_cmd)
    else:
        log("Shizuku 存活，无需处理")

    return 0


def cmd_cycle(adb, args):
    """关 WiFi → 复活 Shizuku → 开 WiFi，一次性验证整个闭环"""
    log("=== 闭环验证：关 WiFi → 复活 Shizuku → 开 WiFi ===")
    if not ensure_connected(adb):
        return 1
    log("起始：wifi=%s shizuku=%s" % (adb.wifi_state(), adb.shizuku_running()))

    log("\n--- 步骤 1/3：关 WiFi ---")
    set_wifi(adb, False)
    time.sleep(2)
    if not ensure_connected(adb, tries=4):
        log("失败：关 WiFi 后 adb 断了且连不回来")
        return 1
    log("关 WiFi 后：wifi=%s shizuku=%s  <- 注意 adb 通道仍然存活（走 USB）"
        % (adb.wifi_state(), adb.shizuku_running()))

    log("\n--- 步骤 2/3：复活 Shizuku（此时手机没有 WiFi）---")
    if not adb.shizuku_running():
        activate(adb, args.start_cmd)
    else:
        log("Shizuku 仍存活，无需重启")

    log("\n--- 步骤 3/3：把 WiFi 开回来 ---")
    set_wifi(adb, True)
    time.sleep(2)
    ensure_connected(adb, tries=4)
    log("最终：wifi=%s shizuku=%s" % (adb.wifi_state(), adb.shizuku_running()))
    log("=== 闭环验证结束 ===")
    return 0


def main():
    ap = argparse.ArgumentParser(description="Shizuku over ESP32 ADB proxy keeper")
    ap.add_argument("--host", required=True, help="ESP32 的 IP（如 192.168.1.50）")
    ap.add_argument("--port", type=int, default=5555, help="代理端口，默认 5555")
    ap.add_argument("--watch", action="store_true", help="常驻保活循环")
    ap.add_argument("--status", action="store_true", help="只查看状态，不启动")
    ap.add_argument("--wifi", choices=["on", "off"], help="开/关手机 WiFi，并自动复活 Shizuku")
    ap.add_argument("--cycle", action="store_true", help="关WiFi→复活→开WiFi 闭环验证")
    ap.add_argument("--interval", type=int, default=60, help="保活检查间隔（秒）")
    ap.add_argument("--start-cmd", default=None,
                    help="手动指定 Shizuku 启动命令（App 里「查看命令」复制）")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    adb = Adb("%s:%d" % (args.host, args.port))

    if not args.start_cmd:
        args.start_cmd = load_saved_cmd()

    if args.status:
        return cmd_status(adb, args)
    if args.cycle:
        return cmd_cycle(adb, args)
    if args.wifi:
        return cmd_wifi(adb, args, args.wifi == "on")

    if not ensure_connected(adb):
        if not args.watch:
            return 1
        log("进入等待，每 %ds 重试（Ctrl+C 退出）" % args.interval)

    backoff = args.interval
    while True:
        if adb.state() != "device":
            log("连接不可用，重连中...")
            ensure_connected(adb)
            time.sleep(min(backoff, 300))
            backoff = min(int(backoff * 1.5), 300)
            if not args.watch:
                return 1
            continue

        backoff = args.interval
        if not adb.boot_completed():
            log("手机尚未完成启动，等待...")
        elif adb.shizuku_running():
            log("Shizuku 运行中（wifi=%s）" % adb.wifi_state())
        else:
            activate(adb, args.start_cmd)

        if not args.watch:
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        log("已退出")
