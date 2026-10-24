# ESP32-S3 USB HID 电脑电源控制器（Blinker 远程唤醒 / 睡眠）

把 ESP32-S3 插在电脑上，它把自己伪装成一个 **USB 键盘 + 系统控制设备**。
手机 Blinker App 点一下按钮 → Blinker 云（MQTT）→ ESP32 → USB HID → 电脑睡眠 / 唤醒。

纯 Rust 实现（esp-hal 裸机 + esp-radio + embassy-usb），**不装 ESP-IDF，不写一行 C/C++**。

---

## 一、硬件

- ESP32-S3 开发板
- 用板子上标 **"USB"** 的那个口接电脑（D- = GPIO19，D+ = GPIO20）。
  > S3 板常见两个口：一个是 "UART"（USB Serial/JTAG，烧录/调试用），一个是 "USB"（OTG，HID 用）。**别插错**。
  > 如果只有一个口（比如某些小板的 USB-C 中间那个），那就是共用的，直接用。

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
2. 浏览器打开 **http://192.168.4.1**（安卓一般会自动弹"登录到此网络"）
3. 填 WiFi 名字和密码 → **保存并连接**
4. 等 10~20 秒，页面上「联网」变成 **已连接** 并显示局域网 IP

配好之后开机自动连，**换 WiFi 也不用重新烧固件**。热点一直在，随时能回来改。

> ⚠️ **必须连上有外网的 WiFi** —— Blinker 走公网 MQTT，连不上就控制不了。
> 热点只是给你配网用的，它本身没有外网。

热点名 / 密码在 `src/store.rs`：

```rust
pub const AP_SSID: &str = "ESP32-POWER";
pub const AP_PASSWORD: &str = "power1234";   // 留空 = 开放网络；否则必须 8~63 字符
pub const AP_IP: Ipv4Address = Ipv4Address::new(192, 168, 4, 1);
```

> 如果你家路由器网段正好是 `192.168.4.x`，把 `AP_IP` 改成别的（比如 `192.168.71.1`）。

### 恢复出厂设置

清空整个配置区（16KB）并重启，之后只剩热点、没配过网：

| 方式 | 操作 |
|---|---|
| 板子上 | **长按 BOOT 键 3 秒**（短按是唤醒电脑） |
| 网页 | 打开 `http://192.168.4.1` → 最下面「恢复出厂设置」→ 确认 |

### 2. Blinker 凭据（3 个值）

先在手机装 Blinker App，注册并**添加一个 DIY 独立设备**，拿到 `authKey`。

然后在电脑浏览器打开（把 `你的authKey` 替换掉）：

```
https://iot.diandeng.tech/api/v1/user/device/diy/auth?authKey=你的authKey&protocol=mqtt
```

会返回一段 JSON，把里面的三个值填进 `src/main.rs`：

```rust
const BLINKER_DEVICE_NAME: &str = "<deviceName>";  // 形如 41A7E676A8DL98SZNS2AP7AB
const BLINKER_IOT_ID:      &str = "<iotId>";       // MQTT 用户名
const BLINKER_IOT_TOKEN:   &str = "<iotToken>";    // MQTT 密码
```

### 3. 电脑网卡 MAC（可选，但强烈建议填）

WOL 魔术包唤醒比 USB HID 唤醒可靠得多。Windows 上用 `ipconfig /all` 查"物理地址"：

```rust
const PC_MAC: [u8; 6] = [0xA4, 0xBB, 0x6D, 0x11, 0x22, 0x33];
```

全 0 表示不启用 WOL，只走 USB HID 唤醒。

---

## 三、Blinker App 上加按钮

设备页面 → 编辑 → 添加组件 → **按键**，建议这样设置：

| 键名 | 作用 |
|------|------|
| `btn-wake` | 唤醒电脑 |
| `btn-sleep` | 电脑睡眠 |
| `btn-power` | 关机（慎用） |

固件做的是**关键字包含匹配**（忽略大小写），所以消息里只要出现 `wake` / `sleep` / `power` 就会触发对应动作。键名带这些词即可，不用严格一致。

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

| 方式 | 条件 |
|------|------|
| **WOL 魔术包**（推荐） | 电脑网卡要在 BIOS 和 Windows 网卡属性里开启"允许此设备唤醒计算机"+"唤醒魔术包"；台式机有线网卡基本都支持 |
| **USB HID** | ① 设备管理器 → 找到这个 HID 键盘 → 电源管理 → 勾"允许此设备唤醒计算机"；② **电脑睡眠时 USB 口必须还有电**（很多主板在 S3/S4 会断 USB 供电，断了就唤不醒，只能走 WOL） |

固件收到 `wake` 时**两种方式都会发**（WOL 先，HID 后），哪个生效算哪个。

### 睡眠能不能成功

- 固件发的是 HID 标准 **System Sleep**（0x82 系统控制）。Windows 一般能响应，但个别版本/USB 控制器会忽略。
- 如果按 `sleep` 没反应，最稳妥的替代方案：在电脑上跑一个小脚本监听指令（比如一个最简 HTTP 服务）执行 `rundll32.exe powrprof.dll,SetSuspendState 0,1,0`。需要的话我可以帮你写这个 Windows 端小助手。
- 关机（0x81）在 Windows 上通常会弹"是否关闭计算机"确认框，不是强制关机。

### 其他

- 电脑完全断电（S5/关机）后，**只能靠 WOL**，USB HID 无效。
- Blinker 走的是明文 MQTT（1883），凭据写死在固件里，别把固件源码公开。

---

## 七、常见问题

| 现象 | 处理 |
|------|------|
| 插上电脑没反应 / 识别不出键盘 | 换到标 "USB" 的口；换根带数据线的 USB 线 |
| 找不到热点 `ESP32-POWER` | 等 10 秒再刷 WiFi 列表；或者板子供电不足（换 USB 口） |
| 手机连上热点一直「正在获取 IP 地址」 | DHCP 服务没起来，看串口有没有 `[dhcp] 启动：` |
| 手机提示「无互联网」并自动断开 | 关掉手机的「自动切换网络 / 网络助理」；DNS 那块看 `src/dns.rs` |
| 配网保存后一直连不上 | 名字/密码错了，或路由器是 5G 频段（ESP32-S3 只支持 2.4G）。回 192.168.4.1 重填 |
| 串口一直刷 WiFi 连不上 | `[wifi] 连接失败` 多半是密码错；`连接超时` 多半是信号弱或 5G 频段 |
| 想换 WiFi / 忘了配过哪个 | 连热点重新配一次就行；或长按 BOOT 3 秒恢复出厂 |
| MQTT 连不上 | 检查 deviceName / iotId / iotToken 三个值有没有复制全；Blinker App 里设备要在线 |
| App 点了没反应 | 串口日志看有没有 `recv:` 这一行；没有说明云端没转发，检查 App 按钮的键名 |
| HID 写入超时警告 | 电脑没枚举设备（没插好 / 插错口），固件会自动跳过不会死机 |
| 短按 BOOT 没唤醒 | 短按是**松手时**触发的；按太久（≥3 秒）会变成恢复出厂 |

---

## 八、技术栈（都在纯 Rust 里）

| 功能 | 用的 crate |
|------|-----------|
| USB HID | `esp-hal::otg_fs` + `embassy-usb` + `HidWriter` |
| WiFi（AP+STA 双开） | `esp-radio` 的 `Config::AccessPointStation` + `embassy-net`（两个独立 stack） |
| 热点 DHCP / DNS | `esp-hal-dhcp-server` + 手写迷你 DNS（`src/dns.rs`） |
| 配网网页 | 手写极简 HTTP（`src/web.rs`）+ 内嵌 `setup.html`，不引框架 |
| 凭证持久化 | `esp-storage` 写 flash `0x9000`（`src/store.rs`） |
| 远程控制 | 手写最小 MQTT 3.1.1 客户端（`src/mqtt.rs`），直连 Blinker broker |
| 异步运行时 | `embassy-executor` + `esp-rtos` |

注意版本锁定：本工程用 **esp-hal 1.1.x 生态**（esp-radio 0.18 / esp-rtos 0.3 / esp-bootloader-esp-idf 0.5），
和 `esp32s3-hello` 的 esp-hal 1.2.x 是两套，互不干扰，别混着升级。
