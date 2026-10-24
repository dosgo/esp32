# ESP32-S3 USB HID 电脑电源控制器（Blinker 远程唤醒 / 睡眠）

把 ESP32-S3 插在电脑上，它把自己伪装成一个 **USB 键盘 + 系统控制设备**。  
手机 Blinker App 点一下按钮 → Blinker 云（MQTT）→ ESP32 → USB HID → 电脑睡眠 / 唤醒。

纯 Rust 实现（esp-hal 裸机 + esp-radio + embassy-usb），**不装 ESP-IDF，不写一行 C/C++**。

---

## 一、硬件 

- ESP32-S3 开发板（实测：N16R8，16MB flash，USB-A 直插 dongle 形态，无 RESET 键）
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
2. 浏览器打开 \*\*<http://192.168.4.1\*\*（安卓一般会自动弹"登录到此网络"）>
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

### 配网页上还有什么（`http://192.168.4.1`）

| 功能 | 说明 |
|---|---|
| **电源控制：唤醒 / 睡眠** | 不用碰板子，手机点一下就唤醒电脑。走 USB 远程唤醒，电脑睡眠时也能用；不依赖外网 |
| **WiFi 密码「明文」按钮** | 密码框右边，点一下切换明文/隐藏，方便核对有没有输错 |
| **Blinker 凭据（存 flash）** | 填 deviceName / iotId / iotToken，保存后**重启一次**生效，以后换 key 不用重烧固件 |
| **诊断日志 `/log`** | 见第八节，没有串口时唯一的观测通道 |

> 网页所有功能在**热点侧（192.168.4.1）**和**局域网侧（板子拿到的 IP）**是同一套页面。
> 但局域网侧能不能访问取决于路由器 —— 开了「客户端隔离 / AP 隔离」就连不上（本机实测就是被隔离的）。

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

### App 显示"离线"但其实连上了？这是心跳问题

Blinker 的在线判定跟 MQTT 连接**不是一回事**：

- App 打开时、进设备页时、之后每 30 秒，都会给设备发
  `{"fromDevice":"<app id>","data":{"get":"state"}}`
- 设备必须回到 `/device/<deviceName>/s` 一条
  `{"toDevice":"<app id>","data":{"state":"online","timer":"000","version":"0.1.0"}}`
- 不回就一直显示"离线"，**哪怕 MQTT 明明连着、按钮也能用**

固件已经实现了这个回复（`blinker_task` 里，任何带 `fromDevice` 的消息都会回一次）。
当初排查时踩了两个坑，记一下：

1. `wait_publish` 用 `select(future, Timer)` 做 30 秒 PINGREQ 会**取消正在进行的读**，
   读到半个报文就流错位，接着断线重连 —— 表现是 App 反复在线/离线。所以不要用 select 做保活，
   靠"回心跳"本身就是客户端活动，broker 的 keepalive 自然被刷新。
2. 解析 `fromDevice` 时，键名后面还有一个**收尾引号**才到冒号（`"fromDevice":"xxx"`），
   少跳这一步就永远取不到值。

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

## 七、常见问题

| 现象                   | 处理                                                            |
| -------------------- | ------------------------------------------------------------- |
| 插上电脑没反应 / 识别不出键盘     | 换到标 "USB" 的口；换根带数据线的 USB 线                                    |
| 找不到热点 `ESP32-POWER`  | 等 10 秒再刷 WiFi 列表；或者板子供电不足（换 USB 口）                            |
| 手机连上热点一直「正在获取 IP 地址」 | DHCP 服务没起来，看串口有没有 `[dhcp] 启动：`                                |
| 手机提示「无互联网」并自动断开      | 关掉手机的「自动切换网络 / 网络助理」；DNS 那块看 `src/dns.rs`                     |
| 配网保存后一直连不上           | 名字/密码错了，或路由器是 5G 频段（ESP32-S3 只支持 2.4G）。回 192.168.4.1 重填       |
| 串口一直刷 WiFi 连不上       | `[wifi] 连接失败` 多半是密码错；`连接超时` 多半是信号弱或 5G 频段                     |
| 想换 WiFi / 忘了配过哪个     | 连热点重新配一次就行；或长按 BOOT 3 秒恢复出厂                                   |
| MQTT 连不上             | 检查 deviceName / iotId / iotToken 三个值有没有复制全；Blinker App 里设备要在线 |
| App 点了没反应            | 串口日志看有没有 `recv:` 这一行；没有说明云端没转发，检查 App 按钮的键名                   |
| HID 写入超时警告           | 电脑没枚举设备（没插好 / 插错口），固件会自动跳过不会死机                                |
| 短按 BOOT 没唤醒          | 短按是**松手时**触发的；按太久（≥3 秒）会变成恢复出厂                                |

---

## 八、第一次上真机踩到的 3 个坑（已修，别再踩）

这三个 bug 的表现完全一样：**COM3 消失、设备管理器里也看不到 HID**。  
原因是它们都 panic 在 `main` 第一次 `.await` 之前，  
而 `builder.build()` 已经把 USB PHY 从 USB-Serial-JTAG 抢过去了 ——  
于是既没有串口看日志，也没有 HID 枚举，看起来像"板子死了"。

| # | 现象                  | 根因                                                                                                                                                                           | 修法                                                                                                                                       |
| - | ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| 1 | 第一次上电必崩             | `StationConfig::default()` 的 `auth_method` 是 `Wpa2Personal`，而首次开机 SSID 和密码都是空串。`esp_wifi_set_config(WIFI_IF_STA)` 返回 `ESP_ERR_WIFI_PASSWORD`，`wifi::new(...).unwrap()` panic | 没存过账号时只下发 `Config::AccessPoint`，配网成功后才切 `AccessPointStation`；`sta_config()` 里空密码一律按开放网络（`None`）处理；`wifi::new` 失败改为记日志 + 只跑 HID，不再 unwrap |
| 2 | 第二次崩                | `net_task` 被 spawn 两次（AP 一个、STA 一个），而 `#[embassy_executor::task]` 默认 `pool_size = 1`，第二次 `spawn` 返回 `SpawnError::Busy`，`.expect()` panic                                     | 加 `#[embassy_executor::task(pool_size = 2)]`                                                                                             |
| 3 | 第三次崩                | `StackResources<3>` 不够：联网侧要 web(TCP) + MQTT(TCP) + WOL(UDP) + DNS 查询(UDP) 共 **4** 个 socket，smoltcp 直接 `panic!("adding a socket to a full SocketSet")`                        | 两个 stack 都改成 `StackResources<6>`                                                                                                         |
| 4 | 枚举时偶发「设备描述符请求失败」    | `builder.build()` 返回后 D+ 上拉已就绪，Windows 立刻开始枚举；但描述符是在**被轮询的**控制任务里回复的，而紧接着的 flash 读 + WiFi 射频校准是同步阻塞 1~2 秒，执行器回不到 `usb_task`                                                  | `build()` 之后先 `Timer::after(800ms).await`，把执行时间让给枚举，再去做别的事                                                                               |
| 5 | **睡眠后按 BOOT 唤不醒电脑** | `embassy-usb` 的 `remote_wakeup()` 有门禁：必须 `suspended && remote_wakeup_enabled` 同时为真，否则直接返回 `InvalidState`，**一个字节都不往总线上打**。底层驱动本身是对的                                           | 失败时改为自己打 OTG 寄存器：`DCTL.RWUSIG` 拉高 10ms（USB 2.0 §7.1.7.7 规定的 resume K 状态），绕开门禁。见 `remote_wakeup_hw()`                                     |

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

> 注意：这个响应**必须带 `Content-Length`**。早先试过"不写长度、靠关连接表示结束"，  
> 结果手机浏览器死等 EOF、服务端死等下一个 accept，整个网页服务被堵死。

---

## 九、技术栈（都在纯 Rust 里）

| 功能              | 用的 crate                                                               |
| --------------- | ---------------------------------------------------------------------- |
| USB HID         | `esp-hal::otg_fs` + `embassy-usb` + `HidWriter`                        |
| WiFi（AP+STA 双开） | `esp-radio` 的 `Config::AccessPointStation` + `embassy-net`（两个独立 stack） |
| 热点 DHCP / DNS   | `esp-hal-dhcp-server` + 手写迷你 DNS（`src/dns.rs`）                         |
| 配网网页            | 手写极简 HTTP（`src/web.rs`）+ 内嵌 `setup.html`，不引框架                          |
| 凭证持久化           | `esp-storage` 写 flash `0x9000`（`src/store.rs`）                         |
| 远程控制            | 手写最小 MQTT 3.1.1 客户端（`src/mqtt.rs`），直连 Blinker broker                   |
| 异步运行时           | `embassy-executor` + `esp-rtos`                                        |

注意版本锁定：本工程用 **esp-hal 1.1.x 生态**（esp-radio 0.18 / esp-rtos 0.3 / esp-bootloader-esp-idf 0.5），  
和 `esp32s3-hello` 的 esp-hal 1.2.x 是两套，互不干扰，别混着升级。
