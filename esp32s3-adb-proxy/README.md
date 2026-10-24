# ESP32-S3 USB Host → ADB over TCP 代理

把 ESP32-S3 当作 USB 主机（OTG Host）插在安卓手机上，把手机的 **ADB USB 接口**原样桥接到
TCP 5555，于是电脑上只要：

```bash
adb connect 192.168.1.50:5555
adb shell
```

就能拿到手机的 shell —— 手机和电脑之间不用拉线，也不用在同一局域网互通。

**核心思路：ESP32 完全不理解 ADB 协议**，只做 `USB bulk 端点 ↔ TCP socket` 的字节流转发。
ADB 的 RSA 认证、CNXN 握手、stream 管理全部由电脑上的 adb 客户端和手机上的 adbd 完成。
所以手机上弹出的"允许 USB 调试吗"授权框，认的是**你电脑的 adb key**，只需点一次。

---

## 0. 选板要求（先看这条，买错板就全白搭）

本固件要求 ESP32-S3 跑 **USB host**，这对板子有硬性要求：

| 要求 | 为什么 | 不满足会怎样 |
|---|---|---|
| **GPIO19/20 有排针或焊盘引出** | host 模式要把 D+/D− 飞线到你自己焊的 USB-A 母座 | 引脚焊死在板载 USB 口上 → 永远做不了 host |
| **有独立的 USB 转 UART 口** | **USB-Serial-JTAG 与原生 USB 共用 GPIO19/20**，进 host 模式后烧写/日志通道就没了，必须走 UART0 | 只有单个 USB 口的 dongle → 做了 host 就再也烧不进程序、看不到日志 |
| **有 5V 引脚可取电** | ESP32-S3 无片上 5V，手机的 VBUS 要从这里取 | 手机检测不到 VBUS 就不枚举 |

**注意：USB 网卡/dongle 形态的 ESP32-S3 基本都不满足前两条**，
它们只有一个 USB 口、19/20 焊死在插头上、没有独立 UART。这类板子做不了本方案。

### 推荐

1. **ESP32-S3 N16R8 开发板（42 针 · 双 Type-C）—— 淘宝上最泛滥的那一款，直接够用。**
   模组是 ESP32-S3-WROOM-1-N16R8（16MB flash + 8MB octal PSRAM），排针上丝印 **IO19 / IO20** 已引出，
   另有独立的 CH343/CH340 USB-to-UART 口（Type-C #1，通常标 "COM"/"UART"）负责供电、烧写和日志。
   三条要求全满足，**不用换板**。
2. **ESP32-S3-DevKitC-1**（乐鑫官方，¥40–70）—— 同上，官方手册确认
   J3 排针 **第 19 脚 = GPIO20 (USB_D+)**、**第 20 脚 = GPIO19 (USB_D−)**，J1 有 5V/GND。
3. **ESP32-S3-USB-OTG** 一类板载 USB-A **母座** 的板子 —— 省掉飞线，
   通常还带 USB 开关 IC 和 500mA 限流芯片（如 MIC2005A）。但型号杂，买前确认是 S3 且带 USB-A 母座。

### 30 秒自检（不用看原理图）

1. **数 Type-C 口**：两个（一个标 COM/UART，一个标 USB/OTG）→ 有独立串口，符合。
   只有一个 → 危险，多半是 dongle，做不了 host。
2. **看排针丝印**：有没有 `IO19`、`IO20` 两个脚。有 → 符合。
3. 两条都过 → 直接按第 1 节接线。

> N16R8 备注：GPIO 26–32 用于片内 flash 未引出；**GPIO 33–37 是 octal PSRAM 占用，
> 即便排针上有也绝对别碰**（驱动它们会打乱 PSRAM 总线直接把芯片搞死）。本项目不用这些脚。

---

## 1. 硬件：必须改造，这是成败关键

### 为什么不能直接用板载 Type-C 口

ESP32-S3-DevKitC-1 上标 **"USB"** 的那个 Type-C 口（GPIO19/20），它的 **CC 引脚焊接了 5.1kΩ
下拉（Rd）**，向对端声明"我是 UFP（设备）"。

安卓手机是 DRP（双角色）。它检测到 ESP32 是 UFP，就会**自己切换成 host 并输出 5V** ——
角色正好反了，ESP32 变成被供电的设备，永远不会枚举到手机。

### 正确做法：飞线到 USB-A 母座

```
ESP32-S3                                USB-A 母座
GPIO20 (USB_D+)  丝印 IO20  [DevKitC-1: J3-19] ───► D+
GPIO19 (USB_D-)  丝印 IO19  [DevKitC-1: J3-20] ───► D-
GND              任意 GND 脚 [DevKitC-1: J1-22] ───► GND
5V               任意 5V 脚  [DevKitC-1: J1-21] ───► 母座 VBUS（先直连验证）
GPIO4            ──► P-MOS 栅极 ──► VBUS 开关 ──► 5V 进 USB-A 母座
                                    (外部 5V 电源)
手机 ◄── 标准 A-to-C 或 A-to-microUSB 线 ──► USB-A 母座
```

飞线注意：D+/D− 尽量 **短于 10cm**，两根长度接近（差分信号）。
板子本身用 **USB-to-UART 那个口** 插电脑供电和看日志，标 "USB" 的那个口空着别插。

**为什么用 USB-A 母座就够了**：标准 A-to-C 线的 Type-C 端，CC 引脚有 56kΩ 上拉到 VBUS，
向手机声明"对端是 legacy host（DFP）"。手机检测到后就会老实进入 **device 模式**，
正好是我们要的。不用自己处理 CC 逻辑。

### 供电：两路电要分清

这是最容易搞混的地方，其实是**两码事**：

| 给谁 | 电压 | 怎么来 | 要不要额外准备 |
|---|---|---|---|
| ESP32-S3 芯片自己 | 3.3V | 板载 USB 口（标 UART/COM 那个）插电脑，经板载 LDO | **不用**，照常插上就有 |
| USB 母座上的 VBUS | 5V | 给**手机**的那一路 | **必须**你另外提供 |

**ESP32 本身不需要"自供电"** —— 它插电脑就有电。真正要额外准备的是给手机的 VBUS 5V。
手机只有在检测到 VBUS 有电时才会枚举（枚举阶段本身只取几十到几百 mA，
真正吃电的是手机想充电的时候）。

三种接法，按推荐度排序：

1. **外部 5V/1A 适配器（推荐）**：单独供 VBUS，**GND 必须和开发板共地**，否则电平没参考、枚举不了。
2. **加 P-MOS 开关（最规范）**：外部 5V → P-MOS → VBUS，GPIO4 输出高电平导通。
   代码里 `vbus_pin.set_high()` 就是干这个的。好处是可以软件开关、过流时切断。
3. **直接借开发板 5V 排针（仅临时验证）**：从板子 `5V` 排针引一根线到母座 VBUS。
   电脑 USB 口一般能给 500mA，手机枚举通常够（手机可能提示"充电慢"，不影响 adb）。
   缺点：手机想大电流充电时会把电压拉垮 → ESP32 重启、设备反复断开。

**判断标准很简单**：插上手机后如果串口反复打印"设备插入 / 拔出"，就是掉压了，改用方案 1。

### 单口 dongle（只有板载 USB 公头）的救星：A 母对母转接头

如果你的板子是 **USB-A 公头 dongle**、没有排针、没有第二个口 —— 不用换板，
用一颗 **USB-A 母对母转接头**（gender changer，几块钱）就能把 D+/D− 引出来。

原理：dongle 那个公头的 4 个触点里，**D+/D− 就是 GPIO19/20 本身**。转接头四根线
全部直通，插上去之后 19/20 就等于连到了转接头另一端的母口，等于"免费引出排针"。

```
                    ┌────────────────────┐
外部 5V ──┬─────────┤ VBUS（同时给两边） │────── A-to-C 线 ──── 手机
（充电宝）│         │ D+ / D− 直连       │
          │         └────────────────────┘
          └────────► 转接头 VBUS 引脚 ──► 也给 dongle 自己供电
                          ▲
                          │ 公头插进去
                    ┌─────┴──────┐
                    │  dongle    │  GPIO19/20 = 公头 D−/D+
                    └────────────┘
```

要点：

- **VBUS 必须由外部灌入**。dongle 的 VBUS 引脚是它的**电源输入**，
  手机也是 sink，两边谁都不输出 5V。外部 5V 灌进转接头的 VBUS，
  **同时**给 dongle 供电和给手机供电 —— 这正好构成 ESP32(host) ↔ 手机(device) 的总线。
- **推荐充电宝**：一路 USB 口输出 5V，剥一根旧线取 VBUS/GND 即可，最省事。
- 转接头选型：搜「**USB A 母对母 转接头**」「USB A female to female adapter」，
  要**直通型**（pass-through，无芯片）。别买到带 OTG 识别芯片的那种。
- 如果转接头带外壳不方便取电，可以买「USB A 母座 转接板 /  breakout」，
  D+/D−/VBUS/GND 四个焊盘直接露在外面，更好接。

**hub 能不能用？** 分两种用法，别搞混：

| 用法 | 行不行 | 说明 |
|---|---|---|
| hub 当作**纯 5V 电源**（上行口不插电脑，从任一口取电） | ✅ | 就是把 hub 当充电宝用，完全没问题 |
| 手机和 dongle **都插进 hub 的口**，想让它们通信 | ❌ | 中间隔了 hub，两者都变成 hub 下游的 device，ESP32 永远做不了 host |

USB 总线上 **host 只能有一个**，而且不能有 hub 居中。ESP32 必须和手机**直接**在同一对
D+/D− 上。

### 日志和烧写（dongle 与开发板不一样）

ESP32-S3 的 **USB-Serial-JTAG 与 USB OTG 共用 GPIO19/20 引脚**。进入 host 模式后
USB-Serial-JTAG 不可用，所以：

**开发板（有独立 USB-to-UART 口）**：

- **烧写 / 看日志**：都用标 "UART"/"COM" 那个口，波特率 115200

**单口 dongle（没有 UART）**：

- **看日志**：串口没戏了，改用 **网络日志**。固件会把 `info!/warn!/error!`
  通过 **UDP 广播到 4444 端口**，电脑上跑：
  ```bash
  python tools/netlog_listen.py
  ```
  WiFi 连上之前产生的日志会先堆在缓冲区，联网后一次性吐出来。
- **烧写**：**必须先把手机拔掉**（让 19/20 悬空），dongle 直插电脑，
  此时 USB-Serial-JTAG 正常，照常 `cargo run --release` 即可。
  烧完再插回转接头接手机。
- **救援**：如果固件跑飞导致连不上，需要手动进下载模式 —— **按住 BOOT(GPIO0) 上电**。
  所以动手前先确认你的 dongle **有没有 BOOT 按键**；没有的话，
  先想好 GPIO0 焊盘的飞线方案再烧，否则可能只能烧一次。

---

## 2. 软件配置

编辑 `src/main.rs` 顶部：

```rust
const WIFI_SSID: &str = "YOUR_WIFI_SSID";
const WIFI_PASSWORD: &str = "YOUR_WIFI_PASSWORD";
const TCP_PORT: u16 = 5555;   // 一般不改

// VBUS 是否由 GPIO4 控制。转接头 / dongle 方案（外部电源常供）必须设 false
const VBUS_SWITCHED: bool = false;
```

`VBUS_SWITCHED` 怎么选：

| 场景 | 值 | 说明 |
|---|---|---|
| 转接头 / dongle，VBUS 由外部电源**常供** | `false` | 单口板引不出 GPIO，必须 false |
| 开发板 + GPIO4 → P-MOS 控制 VBUS 通断 | `true` | 可以软件开关、过流切断 |

`true` 时控制脚在 `main()` 里：

```rust
let mut vbus = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
```

---

## 3. 编译与烧写

```bash
cd D:\esp32\esp32s3-adb-proxy

# 注意：需要把 Xtensa GCC 加进 PATH，否则报 linker xtensa-esp32s3-elf-gcc not found
$env:PATH = "$env:USERPROFILE\.rustup\toolchains\esp\xtensa-esp-elf\bin;$env:PATH"

cargo build --release
cargo run --release          # 或者 espflash flash --monitor
```

`rust-toolchain.toml` 已锁定 `esp` 工具链，`.cargo/config.toml` 已配好目标与 GNU ld 链接参数。

已编译产物：`firmware-adb.bin`（509 KB）。

---

## 4. 使用步骤

1. **手机开启 USB 调试**：设置 → 关于手机 → 连点"版本号"解锁开发者选项 →
   开发者选项 → 打开"USB 调试"。
2. 手机用 A-to-C 线插到 ESP32 的 USB-A 母座上。
3. 看串口日志，应该依次出现：
   ```
   [usb] 设备插入，速率 Full
   [usb] ep0 MPS = 64
   [usb] 设备 VID=0x18d1 PID=0x4ee2        <- 各厂商不同，也可能是 0x2d01 等
   [usb] 配置描述符总长 = 39 字节
   [usb] 接口 #0 class=0x06 sub=0x01 proto=0x01 (非 ADB)   <- MTP
   [usb] 找到 ADB 接口 #1
   [usb]   ADB bulk IN  EP=0x82 MPS=512
   [usb]   ADB bulk OUT EP=0x02 MPS=512
   [usb] 已激活配置 1，ADB 接口就绪
   [adb] bulk 管道已建立 IN=0x82 OUT=0x02
   [adb] 监听 192.168.1.50:5555
   ```
4. 电脑执行 `adb connect <串口里显示的 IP>:5555`。
5. 第一次连接**手机屏幕上会弹"允许 USB 调试吗？"**，勾选"一律允许"并确定。
6. `adb shell` 进入。

---

## 5. 排错

| 现象 | 原因与处理 |
|---|---|
| 串口完全没有"设备插入" | VBUS 没 5V；D+/D- 接反；GND 没共地 |
| 反复"设备插入/拔出" | VBUS 电流不足，手机一取电就掉压 → 换 1A 以上电源 |
| 有插入但枚举失败 / SET_ADDRESS 失败 | 供电或信号完整性问题；D+/D- 飞线尽量短（<8cm）并双绞 |
| 看到 VID/PID 但"未找到完整 ADB 接口" | 手机 USB 调试没开；或"USB 用途"选成了"仅充电"（部分 ROM 需要选"传输文件"才会同时暴露 ADB） |
| 提示"分配 bulk 管道失败" | 通道数不够 → 调大 `usb_host.rs` 里的 `CHANNEL_COUNT` |
| `adb connect` 连上就断 | 手机没点授权；或电脑上已有 adb server 缓存了旧 key，试 `adb kill-server` |
| `adb shell` 卡住 | 手机弹了授权框没确认；或手机锁屏状态下 adbd 未完全就绪 |

---

## 6. 已知限制

- **只服务一个 adb 连接**。再开一个 `adb connect` 会排队，直到前一个断开。
- **USB Full-Speed 12Mbps**。`adb shell`、小文件 `push/pull` 没问题；
  大文件传输大概几百 KB/s，别指望快。
- Android 的 USB 配置里可能同时有 MTP / ADB / RNDIS 等接口，代码会挑
  `class=0xFF, subclass=0x42, protocol=0x01` 的那个（ADB 标准标识）。
- 手机休眠时 USB gadget 可能挂起，需要亮屏才会恢复。

---

## 7. 用 Shizuku 让手机上的 App 拿到 adb 级能力

### 为什么走 Shizuku

ADB 的权限天花板是 **UID 2000 (shell)**，由手机上的 `adbd` 和 ROM 编译类型决定，
**跟传输方式无关**（真 USB 线 / 本 TCP 桥 / AOA 全都一样）。
Shizuku 的做法就是用 adb 起一个 shell 权限的 Java 进程，App 通过 Binder 调它——
不需要 root 就能用 `pm` / `appops` / `settings` / `cmd` / 安装器等 adb 级 API。

### 启动命令：别抄网上的，版本差异很大

| 版本 | 命令 |
|---|---|
| ≤ 13.5.4 | `adb shell sh /sdcard/Android/data/moe.shizuku.privileged.api/start.sh` |
| **13.6.0+** | 官方 release note 写了 "Update the start command"，App 里显示的变成一长串 `/data/app/~~xxx==/moe.shizuku.privileged.api-yyy==/lib/<abi>/libshizuku.so` |

实测发现 13.6.0 上 `start.sh` **多数还在**（内容是拷贝 `starter` 到
`/data/local/tmp/shizuku_starter` 再执行），但不同渠道/版本表现不一致，
所以脚本做的是**多级自动探测**而不是写死一条：

1. 数据目录里的 `start.sh`
2. 数据目录里的 `starter`（拷到 `/data/local/tmp` 执行）
3. 用 `pm path moe.shizuku.privileged.api` 推出 `libshizuku.so` 的位置
4. 老路径兜底

**最稳的办法**：打开 Shizuku App → 「查看命令」，把显示的那条复制出来，
用 `--start-cmd` 传给脚本（会自动存到 `.shizuku_keeper_cmd` 下次复用）。

### 痛点一：每次重启都要重新激活（已解决）

| 激活方式 | 每次重启后要做什么 |
|---|---|
| 无线调试（Android 11+） | 在手机上打开无线调试 → 生成配对码 → 在 Shizuku 通知里输入 |
| 连接电脑 | 插线，在电脑上跑命令 |
| **本方案（ESP32 桥）** | **什么都不用做**，脚本自动完成 |

原因有两个：ESP32 长期挂在手机上；且 adb 的 RSA 授权只要点过一次「总是允许」
就永久有效（存在手机的 `/data/misc/adb/adb_keys`）。所以重启后 adbd 认得你的 key，
脚本直接就能执行启动命令。

### 痛点二：关 WiFi 就自杀 —— 这是 USB 桥唯一不可替代的价值

**这个死循环无线调试方案永远破不了**：

```
想用 Shizuku 关 WiFi
      ↓
关掉 WiFi → 系统自动禁用「无线调试」（官方原话："Wireless debugging is bound to
            the network it was paired on. Switching Wi-Fi networks - or dropping
            to mobile data - breaks the connection."）
      ↓
adb 通道本身跑在 WiFi 上 → 一起断
      ↓
Shizuku 服务失效，而且**没有 adb 就没法把 WiFi 开回来**
```

**本方案走的是 USB**（手机 ←USB→ ESP32 ←WiFi→ 电脑）。手机自己的 WiFi 开关
**影响不到这条通道**。所以关掉 WiFi 之后脚本照样能连上、照样能把 Shizuku 拉起来、
照样能再执行 `set-wifi-enabled enabled` 把 WiFi 开回来。

差别只有一句话：**adb 通道跑在哪条物理链路上**。跑在 WiFi 上就一起死，
跑在 USB 上就能自愈。

### 用法

```bash
python tools/shizuku_keeper.py --host 192.168.1.50 --status   # 只看状态
python tools/shizuku_keeper.py --host 192.168.1.50            # 启动一次
python tools/shizuku_keeper.py --host 192.168.1.50 --watch    # 常驻保活，默认 60s 一轮

# 远程开关 WiFi（关掉后自动复活 Shizuku）
python tools/shizuku_keeper.py --host 192.168.1.50 --wifi off
python tools/shizuku_keeper.py --host 192.168.1.50 --wifi on

# 一次性验证整个闭环：关 WiFi → 复活 → 开回来
python tools/shizuku_keeper.py --host 192.168.1.50 --cycle
```

`--watch` 会检查 `sys.boot_completed` 和 Shizuku 进程，没起来就自动执行启动命令，
手机重启后也会自动重连并重激活。需要一台常开的机器跑它（PC / NAS / 软路由都行）。

**建议第一次先跑 `--cycle`** —— 它会把「关 WiFi → Shizuku 掉 → 自动复活 → 开回来」
整个流程跑一遍并打印每步状态。这一步过了，整套方案的价值就落地了。

### 手机侧必须做的设置

按 Shizuku 官方手册，以下几项不做的话会出现「Shizuku 随机停止」：

- 开发者选项 → **默认 USB 配置** = 「不进行数据传输」（避免弹窗切换 USB 用途导致掉线）。
  若改完 adb 连不上，改回「文件传输」。
- Android 11+ → 启用 **「停用 adb 授权超时功能」**。
- 允许 **Shizuku 后台运行**（关掉电池优化 / 加白名单），否则服务会被系统杀。
- 不要关「USB 调试」和「开发者选项」。
- **MIUI / HyperOS**：额外开「USB 调试（安全设置）」—— 它和「USB 调试」是两个独立开关。
- **ColorOS / OxygenOS**：关掉「权限监控」。**EMUI**：开「仅充电模式下允许 ADB 调试」。

### 关 WiFi 这类"开关"命令：必须走 shell 命令，别走 App 的 API

这是最常踩的一个坑，**Shizuku 给的权限是够的，错的是调用方式**。

Android 10 起 `WifiManager.setWifiEnabled()` 对**所有非系统 App 恒返回 false**
（官方文档原文）。而 Shizuku 有两种用法，效果完全不同：

| 用法 | 身份 | 关 WiFi |
|---|---|---|
| App 通过 Shizuku API 调系统服务 | 还是 **App 自己** | ❌ 被 Android 10 限制挡住 |
| 通过 Shizuku **执行 shell 命令** | **shell UID(2000)** | ✅ 有效 |

所以要在 shell 里执行，别用 App 的 Java API：

```bash
# 关 / 开 WiFi（Android 11+，注意是 enabled/disabled，不是 enable/disable）
adb shell cmd -w wifi set-wifi-enabled disabled
adb shell cmd -w wifi set-wifi-enabled enabled

# 飞行模式
adb shell cmd connectivity airplane-mode enable
adb shell cmd connectivity airplane-mode disable

# 移动数据
adb shell svc data enable
adb shell svc data disable
```

`svc wifi disable` **需要 root**，非 root 手机上会静默失败（退出码还是 0，很难发现）。
`settings put global wifi_on 0` 也早就作废了，别用。

想从脚本里跑，就包装成一条：

```bash
adb -s 192.168.1.50:5555 shell cmd -w wifi set-wifi-enabled disabled
```

如果连这个都失败（部分厂商 ROM 的 SELinux 拦了 shell），那这台机器就只能 root 了。

### 权限边界（别指望太多）

Shizuku 给的是 shell 级，**不是 root**。能做的：`pm install/uninstall`、
`pm grant`（危险权限）、`appops set`、`settings put`、`cmd`、`am`、
读取 shell 可见的日志/进程/网络状态。
做不到的：读其他 App 的 `/data/data`、`setprop`、`tcpdump`、
签名级/特权级权限、绕过 SELinux。真要这些，只能 root（Magisk）或 userdebug ROM。

---

## 8. 代码结构

| 文件 | 作用 |
|---|---|
| `src/usb_host.rs` | ESP32-S3 OTG **host 模式**的芯片初始化。esp-hal 1.1.2 的 `otg_fs` 只实现了 device（硬编码 `IDDIG=High` 强制 device），这里把角色信号翻转成 host 并挂中断 |
| `src/enumerate.rs` | 最小 USB 枚举：GET_DESCRIPTOR / SET_ADDRESS / SET_CONFIGURATION + 描述符解析，定位 ADB 接口 |
| `src/main.rs` | WiFi + TCP server + USB host 主循环 + bulk↔TCP 双向转发 |
| `src/netlog.rs` | 网络日志：host 模式下串口失效，把 log 经 UDP 广播到 4444 |
| `tools/netlog_listen.py` | 网络日志接收器（单口 dongle 没有串口，只能靠它看日志） |
| `tools/shizuku_keeper.py` | Shizuku 自动激活/保活脚本（走本桥的 TCP 通道） |
| `firmware-adb.bin` | 已编译镜像 |

### 依赖选择说明

- `embassy-usb-synopsys-otg 0.4.0` 开 `host` feature —— ESP32-S3 用的就是 Synopsys DWC2 这个 IP，
  它的 host 驱动是**已发布**的稳定版（含 FIFO 分配、port power、中断）。
- 没有用 `embassy-usb` 的 host 模块 —— 那个只在 git 版里，发布版 0.6.0 还没有。
  枚举逻辑自己写反而更可控（我们只需要定位一个 vendor-specific 接口）。
- `esp-hal` 仍锁 1.1.2（与 `esp-radio 0.18` 配套）。
