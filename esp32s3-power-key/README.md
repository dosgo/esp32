# ESP32-S3 USB HID 电脑电源控制器（Blinker 远程唤醒 / 睡眠）

把 ESP32-S3 插在电脑上，它把自己伪装成一个 **USB 键盘 + 系统控制设备**。  
手机 Blinker App 点一下按钮 → Blinker 云（MQTT）→ ESP32 → USB HID → 电脑睡眠 / 唤醒。

纯 Rust 实现（esp-hal 裸机 + esp-radio + embassy-usb），**不装 ESP-IDF，不写一行 C/C++**。

---

## 零、网页现在也能远程键鼠（从 esp32s3-hid-kvm 合并进来）

主页仍然是**设置页**（配网 / Blinker / 日志 / 恢复出厂），键鼠页在 **`/kvm`**：

| 地址 | 页面 | 干什么 |
|---|---|---|
| `http://192.168.4.1/`（或局域网 IP） | **设置页（主页）** | 配网、Blinker 凭据、诊断日志 `/log`、恢复出厂 |
| `/kvm` | **键鼠控制页** | 虚拟全键盘 + 媒体键 + 鼠标触控区 + 唤醒/睡眠 |

```
键鼠页：虚拟全键盘（104 键 + 小键盘） · 媒体键
        鼠标触控区（点一下锁定指针，Esc 解锁）
        顶栏：唤醒 / 睡眠 / 释放所有键 / 设置
```

设置页顶部有个「打开远程键鼠 →」的大按钮；键鼠页顶栏的「设置」按钮跳回来
（**跳之前会自动断开 WebSocket**，虽然现在每个网有 4 条会话槽，但这个习惯仍然省事）。

任何一个没匹配上的路径（含手机连热点时弹的"登录到此网络"）都落到设置页。

**这样一块 dongle 就够用了**：电脑睡着 → 手机点「唤醒」（USB 远程唤醒，和 Blinker 那条路一样）
→ 需要输密码 / 进 BIOS 就在同一个页面上敲键盘、拖鼠标。

键盘走的是 **boot 6KRO 报告**，BIOS / 无驱动环境直接能用，没有「切模式」这一步。

### 硬件上多了两个 interface

| # | interface | 用途 |
|---|---|---|
| ① | 键盘（boot 6KRO）+ 系统控制（Report ID 1/2） | 原来的电源唤醒，**没动** |
| ② | 鼠标（boot，4 字节） | 网页触控区 |
| ③ | Consumer Control（2 字节） | 网页上的音量 / 播放等媒体键 |

键盘**没有新建**，网页发来的 NKRO bitmap 在固件里转成 6KRO 走 ①，
所以设备管理器不会多出一堆"键盘"，Windows 里那个「允许此设备唤醒计算机」也不用重配
（换过固件还是建议 `powercfg /devicequery wake_armed` 复查一眼）。

### 两个使用上的坑

- **电脑睡着时先点「唤醒」，再敲键盘**：睡眠期间 USB 总线是挂起的，直接发 HID 报文
  会写超时（日志里能看到 `[hid] 鼠标报告超时`），唤醒那一下会先把总线拉起来。
- **每个网 4 条并发连接**：热点侧和局域网侧各自并排跑 4 个网页会话
  （`main.rs` 的 `WEB_SESSIONS`），也就是最多 8 个页面同时在线，第 5 个才开始被拒。
  同一台设备上**多人同时操作仍会互相覆盖**（每个 `n` 帧都是全量快照，后到的算数），
  所以适合"一个人操作、其他人看着"，不适合两个人抢着拖鼠标。
- **唤醒按钮走 WS，不走 HTTP**：所以顶栏那两个按钮不占额外连接；
  而 `/wake` `/sleep` 这两个 HTTP 路由仍然保留，给 curl / 脚本用。

---

## 一、硬件 

- **GNPE "Pocket Dongle S3-0.96"**（实测板：N16R8，16MB flash + 8MB OPI PSRAM，
  USB-A 直插 dongle 形态，**无 RESET 键**，板载 0.96" ST7735 IPS 屏 + TF 卡槽）
  > 注意：它**不是** LilyGO T-Dongle-S3 的克隆 —— 外形和屏一样，但**接线完全不同**。
  > 完整引脚表见 [第八·五节](#八五lcd-状态显示已启用)。
- 用板子上标 **"USB"** 的那个口接电脑（D- = GPIO19，D+ = GPIO20）。
  > S3 板常见两个口：一个是 "UART"（USB Serial/JTAG，烧录/调试用），一个是 "USB"（OTG，HID 用）。**别插错**。  
  > 如果只有一个口（比如某些小板的 USB-C 中间那个，或者 USB-A 直插的 dongle 板），那就是共用的，直接用。  
  > 共用口的板子要注意：**OTG 一接管，串口就没了**，日志改用热点里的 <http://192.168.4.1/log（见第八节）。>

---

## 二、填配置（要改的地方都在 `src/main.rs` 顶部）

### 1. WiFi —— **不用写代码了，网页配**

固件现在**同时开着热点和联网**，WiFi 账号密码存在 flash 里，用网页配：

```
           ┌── 热点 ESP32-POWER ──► 192.168.4.1   （永远开着：配网、救急）
ESP32-S3 ──┤
           └── 你家 WiFi ────────► 192.168.1.x   （配过就自动连，MQTT 靠它）
```

第一次用：

1. 手机/电脑连热点 **`ESP32-POWER`**，密码 `power1234`
2. 浏览器打开 \*\*<http://192.168.4.1\*\*>（安卓一般会自动弹"登录到此网络"，落地就是设置页）
3. 填 WiFi 名字和密码 → **保存并连接**
4. 等 10~20 秒，页面上「联网」变成 **已连接** 并显示局域网 IP

配好之后开机自动连，**换 WiFi 也不用重新烧固件**。热点一直在，随时能回来改。

> ⚠️ **必须连上有外网的 WiFi** —— Blinker 走公网 MQTT，连不上就控制不了。  
> 热点只是给你配网用的，它本身没有外网。

热点名 / 密码在 `src/store.rs`：

```rust
pub const AP_SSID: &str = "ESP32-POWER";
pub const AP_PASSWORD: &str = "power1234";   // 留空 = 开放网络；否则必须 8~63 字符
pub const AP_IP: IpV4Address = Ipv4Address::new(192, 168, 4, 1);
```

> 如果你家路由器网段正好是 `192.168.4.x`，把 `AP_IP` 改成别的（比如 `192.168.71.1`）。

### 设置页上还有什么（`http://192.168.4.1/`，主页）

| 功能 | 说明 |
|---|---|
| **键鼠控制页 `/kvm`** | 虚拟键盘 + 鼠标触控区 + 媒体键 + 唤醒按钮，见第零节 |
| **电源控制：唤醒 / 睡眠** | 不用碰板子，手机点一下就唤醒电脑。走 USB 远程唤醒，电脑睡眠时也能用；不依赖外网 |
| **WiFi 密码「明文」按钮** | 密码框右边，点一下切换明文/隐藏，方便核对有没有输错 |
| **Blinker 凭据（存 flash）** | 填 deviceName / iotId / iotToken，保存后**重启一次**生效，以后换 key 不用重烧固件 |
| **诊断日志 `/log`** | 见第八节，没有串口时唯一的观测通道 |

> 网页所有功能在**热点侧（192.168.4.1）**和**局域网侧（板子拿到的 IP）**是同一套页面。
> 局域网侧实测**可以访问**：电脑上 `http://192.168.88.214/` 返回 200。
> （之前误判成"路由器开了客户端隔离"，其实是照着屏上被截断的 IP 打错了地址 —— 见第八·五节。）
> 如果哪天连不上，先确认 IP 对不对：屏上那行、或路由器后台的 DHCP 列表。

### 恢复出厂设置

清空整个配置区（16KB）并重启，之后只剩热点、没配过网：

| 方式  | 操作                                        |
| --- | ----------------------------------------- |
| 板子上 | **长按 BOOT 键 3 秒**（短按是唤醒电脑）                |
| 网页  | 打开 `http://192.168.4.1` → 最下面「恢复出厂设置」→ 确认 |

### 2. Blinker 凭据（3 个值）

**不用改代码了，网页里填**：连热点打开 `http://192.168.4.1` → 「Blinker 凭据」卡片 →
填 deviceName / iotId / iotToken → 保存（设备会重启一次）。

三个值从哪来：先在手机装 Blinker App，注册并**添加一个 DIY 独立设备**，拿到 `authKey`。
然后浏览器打开（把 `你的authKey` 替换掉）：

```
https://iot.diandeng.tech/api/v1/user/device/diy/auth?authKey=你的authKey&protocol=mqtt
```

返回的 JSON 里取 `deviceName` / `iotId` / `iotToken`。

> 网页里没填过时，固件会回退到 `src/main.rs` 顶部这三个常量；
> 填过之后以 flash 里存的为准（恢复出厂会一起清掉）。
> 凭据存 flash 第二块扇区 `0xA000`，WiFi 账号密码在 `0x9000`，互不影响。

### 3. 电脑网卡 MAC（可选，但强烈建议填）

WOL 魔术包唤醒比 USB HID 唤醒可靠得多。Windows 上用 `ipconfig /all` 查"物理地址"：

```rust
const PC_MAC: [u8; 6] = [0xA4, 0xBB, 0x6D, 0x11, 0x22, 0x33];
```

全 0 表示不启用 WOL，只走 USB HID 唤醒。

---

## 三、Blinker App 上加按钮

设备页面 → 编辑 → 添加组件 → **按键**，建议这样设置：

| 键名          | 作用     |
| ----------- | ------ |
| `btn-wake`  | 唤醒电脑   |
| `btn-sleep` | 电脑睡眠   |
| `btn-power` | 关机（慎用） |

固件做的是**关键字包含匹配**（忽略大小写），所以消息里只要出现 `wake` / `sleep` / `power` 就会触发对应动作。键名带这些词即可，不用严格一致。

> 键名必须是**英文**：写「唤醒」不行，因为固件只在 JSON 里找 `wake` 这个字符串。



---

## 四、编译和烧写

```bash
cd D:\esp32\esp32s3-power-key
cargo run --release          # 自动编译 + 烧录 + 打开串口监视
```

或分步：

```bash
cargo build --release
espflash flash --monitor target\xtensa-esp32s3-none-elf\release\esp32s3-power-key
```

退出串口监视：`Ctrl + ]`

已编译镜像在 `firmware-powerkey.bin`（约 604 KB），也可以直接烧：

```powershell
espflash flash --chip esp32s3 firmware-powerkey.bin
```

### 串口不见了怎么重烧

固件一旦 panic，`otg_fs` 已经把 USB PHY 从 USB-Serial-JTAG 抢走了，COM 口会**彻底消失**，  
这时不能靠 DTR/RTS 自动复位，只能手动进 ROM 下载模式：

1. 拔掉 USB
2. 按住 **BOOT** 键不松手
3. 插回 USB，等 2 秒
4. 松开 BOOT → COM 口重新出现，可以烧了

> 这块 N16R8 板子**没有 RESET 键**，所以只能用拔插的方法。

---

## 五、先验证 HID 通不通（不用联网）

固件里板载 **BOOT 键（GPIO0）短按一下就发一次唤醒**。

1. 板子插电脑（"USB" 口）
2. 等 Windows 识别出"HID 键盘"（设备管理器 → 键盘 会多一个）
3. 按一下 BOOT 键 → 如果电脑亮屏/解除屏保，说明 HID 链路全通

这一步通了，再去配 Blinker。

> BOOT 键两种用法：**短按（< 3 秒）= 唤醒电脑**，**长按 3 秒 = 恢复出厂设置**。  
> 注意短按是**松手那一刻**才发唤醒的，所以按住不放不会误触发。

---

## 六、重要限制（先看这个，避免白折腾）

### 唤醒能不能成功

| 方式              | 条件                                                                                                        |
| --------------- | --------------------------------------------------------------------------------------------------------- |
| **WOL 魔术包**（推荐） | 电脑网卡要在 BIOS 和 Windows 网卡属性里开启"允许此设备唤醒计算机"+"唤醒魔术包"；台式机有线网卡基本都支持                                            |
| **USB HID**     | ① 设备管理器 → 找到这个 HID 键盘 → 电源管理 → 勾"允许此设备唤醒计算机"；② **电脑睡眠时 USB 口必须还有电**（很多主板在 S3/S4 会断 USB 供电，断了就唤不醒，只能走 WOL） |

固件收到 `wake` 时**两种方式都会发**（WOL 先，HID 后），哪个生效算哪个。

### 睡眠能不能成功

- 固件发的是 HID 标准 **System Sleep**（0x82 系统控制）。Windows 一般能响应，但个别版本/USB 控制器会忽略。
- 如果按 `sleep` 没反应，最稳妥的替代方案：在电脑上跑一个小脚本监听指令（比如一个最简 HTTP 服务）执行 `rundll32.exe powrprof.dll,SetSuspendState 0,1,0`。需要的话我可以帮你写这个 Windows 端小助手。
- 关机（0x81）在 Windows 上通常会弹"是否关闭计算机"确认框，不是强制关机。

### 其他

- 电脑完全断电（S5/关机）后，**只能靠 WOL**，USB HID 无效。
- Blinker 走的是明文 MQTT（1883），凭据写死在固件里，别把固件源码公开。

---

                       |

---

                                                                                                       
### 关于 USB 唤醒（重要）

- 固件里 `usb_config.supports_remote_wakeup = true` **必须开**，否则配置描述符的 `bmAttributes` 不带 remote-wakeup 位，  
  Windows 设备管理器里**根本不会出现**「允许此设备唤醒计算机」这个勾，设备也进不了 `wake_programmable` 列表。
- 烧完新固件后要在 Windows 上确认一次：
  ```powershell
  # 应该能看到 HID Keyboard Device 和 符合 HID 标准的系统控制器
  powercfg /devicequery wake_armed
  ```
  没看到就去 设备管理器 → 对应 HID 设备 → 属性 → 电源管理 → 勾「允许此设备唤醒计算机」。
- 唤醒时固件会先发 `System Wake Up` 再补一个**空格键**（S0 待机把键盘输入当用户活动，最保险）。  
  副作用是唤醒后光标处会多一个空格，介意的话把 `hid_task` 里那段 Space 报文删掉即可。
- 本机实测：Windows 只有 **S0 低电量待机（Modern Standby）**，没有 S3。这套方案在 S0 下工作正常。

> 排查方法：把 USB 初始化整段用 `if false { ... }` 包起来做一版"诊断固件"，  
> USB-Serial-JTAG 就不会被抢，串口日志一直可用，能直接看到 panic 信息。

### 这块板子没有串口，怎么调试

N16R8 dongle 板只有一个 USB-A 公头，`otg_fs` 一接管，USB-Serial-JTAG 就没了，  
串口日志一个字节也读不到。所以固件内置了一个**环形日志缓冲**，用手机就能读：

```
手机连热点 ESP32-POWER → 打开 http://192.168.4.1/log
```

会输出 `[usb] enabled/configured/suspended/远程唤醒允许`、`[boot] 短按`、`[usb] 软唤醒失败…`  
这类关键事件，最多 48 行、滚动的。`main.rs` 里的 `dlog!("…")` 往里面写。



---

## 八·五、LCD 状态显示（已启用 ✅）

板载 **0.96" ST7735S IPS，80×160**，驱动在 `src/lcd.rs`
（`mipidsi` + `embedded-graphics`）。屏幕每秒刷新一次，显示四块信息：

```
WiFi AP          ← 热点名
ESP32-POWER
pw power1234     ← 热点密码
────────────
STA      OK
192.168.88.12    ← 联网状态 + 拿到的 IP
────────────
Blinker
on-line          ← 与 App 的连通状态
────────────
config
192.168.4.1      ← 配网地址
BOOT = wake
```

### 真实引脚（官方原理图，别再猜了）

来源：[jcorp-nomad 的 PocketDongleS3.md](https://github.com/beastboost/jcorp-nomad/blob/main/docs/PocketDongleS3.md)，
引脚表由 GNPE 提供的原理图给出。

| 信号 | GPIO | 说明 |
|---|---|---|
| SCL / SCLK | **10** | SPI2（FSPI）时钟 |
| SDA / MOSI | **11** | 数据 |
| CS | **12** | 片选 |
| DC / RS | **13** | 命令/数据选择 |
| RST / RES | **14** | 复位 |
| 背光 | **无** | 原理图里没有背光脚，**硬连常亮**，软件控制不了 |

同板其它接线（顺带记下，将来用得上）：

| 功能 | GPIO |
|---|---|
| BOOT 键 | 0（拉低按下；长按 3 秒恢复出厂） |
| TF 卡（**普通 SPI，不是 SDMMC**） | SCLK 17 / MOSI 18 / MISO 16 / CS 47 |
| RGB LED | **没有** |
| 八线 PSRAM | 33~37 被永久占用，任何设计都不能用 |

### 显示参数

- 方向：竖屏 80×160（面板原生 grayscale 窗口在 132×162 GRAM 里，偏移 **X=26 / Y=1**）
- 颜色顺序：**BGR**（`ColorOrder::Bgr`，等价于官方 `MADCTL=0xA8`）
- 反色：**开**（IPS 面板要 `Inverted`）
- SPI：**26MHz**（官方在这块板上用 27MHz 实测正常）

### 刷新策略（别写成每秒全屏重画）

第一版是"每秒 `clear(BG)` + 全屏重画"，结果肉眼能看到 **1Hz 的整屏闪烁**。
现在的做法：

- **静态内容只画一次**：热点名、密码、STA/Blinker/config 的分隔线
- **动态内容只在值变化时重画**：STA 状态、IP、Blinker 在线状态，
  每次先 `clear_box()` 把那一小块涂回背景色再写新字（否则新旧文字会叠在一起）

字体也要挑：屏只有 80px 宽，`FONT_6X10` 一行只放得下 **13 个字符**，
而 `192.168.88.214` 有 15 个 —— 会被**截断**成 `192.168.88.21`，
照着这个地址打当然打不开。所以 IP 那行单独用 `FONT_5X8`（每行 16 字符）。
**这个坑很隐蔽：页面本身是好的，只是地址被显示短了。**

这样屏幕平时是定住的，只有联网状态真的变化时才跳一下。



## 九、技术栈（都在纯 Rust 里）

| 功能              | 用的 crate                                                               |
| --------------- | ---------------------------------------------------------------------- |
| USB HID         | `esp-hal::otg_fs` + `embassy-usb` + `HidWriter`                        |
| WiFi（AP+STA 双开） | `esp-radio` 的 `Config::AccessPointStation` + `embassy-net`（两个独立 stack） |
| 热点 DHCP / DNS   | `esp-hal-dhcp-server` + 手写迷你 DNS（`src/dns.rs`）                         |
| 配网网页            | 手写极简 HTTP（`src/web.rs`）+ 内嵌 `setup.html`，不引框架                          |
| 凭证持久化           | `esp-storage` 写 flash `0x9000`（`src/store.rs`）                         |
| 远程控制            | 手写最小 MQTT 3.1.1 客户端（`src/mqtt.rs`），直连 Blinker broker                   |
| 网页键鼠            | 手写 WebSocket（`sha1` 只用来算握手 accept）+ 鼠标/媒体键描述符（`src/hid.rs`） |
| 异步运行时           | `embassy-executor` + `esp-rtos`                                        |

注意版本锁定：本工程用 **esp-hal 1.1.x 生态**（esp-radio 0.18 / esp-rtos 0.3 / esp-bootloader-esp-idf 0.5），  
和 `esp32s3-hello` 的 esp-hal 1.2.x 是两套，互不干扰，别混着升级。
