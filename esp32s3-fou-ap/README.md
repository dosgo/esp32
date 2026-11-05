# PocketGW · ESP32-S3 FOU 网关

用 ESP32-S3 起一个 Wi-Fi 热点，把热点下所有设备的流量整条塞进 FOU（IP-in-UDP），
交给同一热点下的安卓手机出网。手机跑 [PocketGateway](https://github.com/dosgo/pocket-gateway)，
不需要 root，也不需要额外的路由器。

```
   笔记本 / 平板 ──Wi-Fi──┐
                          ├─► ESP32-S3 (192.168.4.1) ──FOU/UDP 5555──► 手机 (192.168.4.2)
   其它设备 ─────Wi-Fi────┘        802.3 帧 → IP 包 → UDP                gVisor netstack 出站
```

## 功能

- 开机即起热点（默认 `PocketGW-ESP32` / `gateway888`，信道 6，最多 8 个客户端）
- 内置 DHCP 服务器：地址池 `192.168.4.2 ~ 192.168.4.200`，下发网关 `192.168.4.1`
  和公网 DNS（`223.5.5.5` / `119.29.29.29`）
- 内置 Web 设置页（`http://192.168.4.1`）：显示 Wi-Fi 账号密码、FOU 目标、转发计数，
  可改 SSID / 密码 / FOU 地址 / 端口 / 转发开关
- **两种出口同时支持**：手机连本热点（AP 模式）或手机在上游 LAN（AP+STA 双模，上行模式），
  默认自动选路，也可在网页里强制指定
- 板载 1.54" ST7789 屏直接显示热点名 / 密码 / 设置页地址 / FOU 目标 / 客户端与流量
- 串口每 5 秒打一行状态，附最近一次 DHCP 收发的关键字段，不开网页也能排障

## 硬件

ESP32-S3（16 MB Flash，2.4 GHz Wi-Fi）。开发用的是一块带 1.54" ST7789 圆角屏的
ESP32-S3 板（和 `esp32s3-voice-assistant` 同一块板），串口走板载 USB-UART（`COM4`）。

屏幕引脚（SPI2，只写不读）：

| 功能 | GPIO |
| --- | --- |
| SCLK | 38 |
| MOSI | 39 |
| CS   | 21 |
| DC   | 45（strapping） |
| RST  | 40 |
| BL   | 46（strapping，高电平亮） |

没有屏的裸板也能跑，把 `main.rs` 里 spawn `display::display_task` 的那段删掉即可。

## 编译与烧写

需要 `esp` 工具链（`espup install`）、`espflash`：

```bash
# Xtensa 交叉编译器必须进 PATH，否则报 linker not found。
# 本机实测可用的是 embuild 下载的这套（espup 写进用户环境变量的那条路径
# 在新的 Bash 会话里常常读不到）：
export PATH="$HOME/.cargo/bin:/d/.embuild/espressif/tools/xtensa-esp-elf/esp-14.2.0_20241119/xtensa-esp-elf/bin:$PATH"

cargo build --release
espflash flash --port COM4 --baud 921600 \
  target/xtensa-esp32s3-none-elf/release/esp32s3-fou-ap
```

**看日志**：这块板上 `espflash monitor` 连上后不打印任何东西（打开串口时
DTR/RTS 的状态把输出吞了）。要用 PowerShell 发标准 esptool 复位序列再读：

```powershell
$sp = New-Object System.IO.Ports.SerialPort("COM4",115200,"None",8,"One")
$sp.Open(); $sp.DiscardInBuffer()
$sp.DtrEnable = $false
$sp.RtsEnable = $true; Start-Sleep -Milliseconds 200
$sp.RtsEnable = $false
$deadline = (Get-Date).AddSeconds(8)
while ((Get-Date) -lt $deadline) {
  $sp.ReadExisting(); Start-Sleep -Milliseconds 80
}
$sp.Close()
```

## 使用

1. 手机连 `PocketGW-ESP32`（密码 `gateway888`），在 PocketGateway 里把 FOU 端口设成 `5555`
   并打开隧道；
2. 浏览器打开 `http://192.168.4.1`，确认「手机 IP」是对的（DHCP 分配的第一个地址，
   一般是 `192.168.4.2`）；
3. 其它设备连这个热点即可上网 —— 网关和 DNS 都由 DHCP 自动下发，不用手工配。

> 手机自己要不要能上网是另一回事：它连着这个热点时默认路由指向 `192.168.4.1`，
> 自己的出站包会被环路保护丢掉。安卓需要在 Wi-Fi 高级设置里打开
> 「自动切换到移动数据」，让手机自己走蜂窝。

## 开机即生效的空口设置

启动时统一执行一次（以前只在网页点「保存」时才调，重启后持久化的信道/带宽
配置其实没作用到驱动）：

| 设置 | 说明 |
| --- | --- |
| `esp_wifi_set_channel(ch, SECOND_CHAN_ABOVE)` | **关键**：HT40 的扩展信道位置由 `set_channel` 第二个参数决定。以前只调 `set_bandwidths(HT40)`，日志显示 HT40 但实际仍跑主 20MHz。修复后串口会打 `AP channel = 6 secondary = Above` |
| `esp_wifi_set_ps(WIFI_PS_NONE)` | AP 不需要省电，避免 TIM/DTIM 缓冲带来的延迟 |
| `esp_wifi_set_max_tx_power(80)` | 20dBm（单位 0.25dBm），影响客户端可用的 MCS |
| 协议位图 | 试过去掉 11b 免掉 ERP protection，IDF 规定 2.4G 位图必须含 11b（返回 0x102）。且 protection 只在真有 11b-only 设备关联时才启用，本来就没这笔开销，保持默认 |

## 配置持久化

网页里所有改动（AP 名称/密码、FOU 目标 IP 与端口、上行 Wi-Fi 名称/密码/开关、
上行目标 IP、出口策略、隧道开关、2.4G 信道、20/40MHz）保存后自动写入 flash，
**重启后自动恢复**，不用每次重配。

- 位置：flash 偏移 `0x3F0000` 的一个独立 4KB 扇区，在 factory app 镜像
  （目前约 630KB）之后，不碰 bootloader、分区表、PHY 校准，也不碰 IDF 的 NVS 分区。
- 格式：`PGCF` magic + 版本 + payload 长度 + 字段 + FNV-1a 校验和。校验不通过
  就当没存过，用默认值；首次启动会把默认值写一次盘。
- 保存由常驻任务合并 400ms 内的改动后串行执行；内容没变化会跳过擦写，省 flash 寿命。
- 信道和带宽在保存时立即通过驱动生效，重启后由配置恢复。

启动日志判断：

| 日志 | 含义 |
| --- | --- |
| `[cfg] restored: ch=6 wide=true up_on=false route=0` | 配置恢复成功 |
| `[cfg] no valid config, writing defaults` | 首次启动或校验失败 |
| `[cfg] saved N bytes` / `[cfg] unchanged, skip write` | 网页保存落盘结果 |

想恢复出厂：擦掉 `0x3F0000` 起始的那个扇区，下次启动会重新写默认值。

## 两种出口（可同时配置）

| 模式 | 手机在哪 | 封装后怎么发 | 适用 |
| --- | --- | --- | --- |
| **AP 模式** | 连在 ESP32 自己的热点上（默认 `192.168.4.2`） | 封成以太网帧，直接发给手机 MAC | 出门只有手机，热点即网关 |
| **上行模式** | 连在家里路由器 / 别的 Wi-Fi 上 | 封成 UDP，从 station 网卡发给上游 LAN 里的手机 | 想用家里 Wi-Fi 更稳的回程，或热点本身也要上网 |

两条路不需要二选一，配置可以同时填好，由「出口策略」决定：

- **自动（默认）**：ARP 表里能看到 AP 侧的手机，就用 AP 路径（少一跳、不占上行空口）；
  看不到就用上行。手机从热点切到路由器（或反过来）时不用改配置，最多丢一两个包。
- **只用本热点** / **只用上行**：强制走某一条，另一条完全不参与。

```
AP 模式：  客户端 ──AP──► ESP32 ──AP──► 手机 PocketGateway ──► 互联网
上行模式：客户端 ──AP──► ESP32 ──STA──► 路由器 ──► 手机 PocketGateway ──► 互联网
```

配置在 `http://192.168.4.1`：

| 字段 | 说明 |
| --- | --- |
| 本热点里的手机 IP（AP 模式） | 默认 `192.168.4.2`，DHCP 第一个租约 |
| 上行 Wi-Fi 里的手机 IP（上行模式） | 手机连上游路由器后拿到的地址，比如 `192.168.1.50`；留空则沿用上面的 |
| 出口策略 | 自动 / 只用本热点 / 只用上行 |
| 上游 Wi-Fi 名称 / 密码 + 启用上行 | 开启 APSTA 双模，ESP32 自己去连路由器 |

串口会打出当前选路：

```
  uplink on=true state=2 ip=3232235846 ch=6 target=3232235826 route=0 path=2
```

`path` 就是实际出口：`0` 没有可用出口、`1` AP 内的手机、`2` 上行 Wi-Fi。

注意：

- **双模共用一块射频**，AP 下客户端的流量和隧道流量抢同一个空口，吞吐会比纯 AP 低；
- 上游信道和 AP 信道必须一致（代码里 AP 用信道 `0` 让驱动自动跟随）；
- ARP 表有 120 秒老化，客户端只要还有流量就会不断续期；老化后自动模式才会切到另一条路；
- 配置只存在内存里，重启后回到「纯 AP + 默认账号」，需要重新填（后续可加 NVS 持久化）。

## 串口状态行

```
gw=192.168.4.1/24 fou=192.168.4.2:5555 up=true client=192.168.4.2 tx=26 rx=0 drop=0 loop=24 | air f=91 arp=16 ip=50 dhcp=3 oth=25 mac=aa:a9:ba:3b:7d:51
  dhcp_rx: RX len=352 dmac=ff:ff:ff:ff:ff:ff xid=d195bf7a flags=0000 type=1 yi=0.0.0.0 ...
  dhcp_tx: TX len=342 dmac=ff:ff:ff:ff:ff:ff xid=d195bf7a flags=0000 type=5 yi=192.168.4.2 ...
```

| 字段 | 含义 |
| --- | --- |
| `tx` / `rx` | 进隧道 / 从隧道回来的包数 |
| `drop` | 因为目标不在本网段或转发关闭被丢弃 |
| `loop` | 环路拦截：源地址就是 FOU 出口节点自己的包 |
| `air f` | 空中收到的 802.3 帧总数 |
| `air arp/ip/dhcp/oth` | 按类型拆分；`dhcp` 涨但 `ip` 不涨说明手机一直在要地址 |
| `mac` | 最近一帧的源 MAC |
| `uplink` | 上行开关 / 状态（0 关 1 连接中 2 已连 3 失败）/ 上游 IP / 信道 / 上行目标 / 策略 / 当前出口 |

排障顺序：`air f=0` → 手机没连上（射频/密码）；`dhcp` 涨但拿不到 IP → DHCP 回包问题；
`tx` 涨 `rx=0` → 手机侧 FOU 隧道没开或端口不对。

## 屏幕

240×240，开机先画一张 `POCKET GATEWAY` 启动图（800 ms），然后切成状态页：

```
POCKET GATEWAY              WAITING / LINKED
──────────────────────────────────────────
WIFI NAME
PocketGW-ESP32
PASSWORD
gateway888
──────────────────────────────────────────
SETUP   http://192.168.4.1
FOU     192.168.4.2:5555
──────────────────────────────────────────
tx 26   rx 0
client: 192.168.4.2
Waiting for phone to join
```

- 右上角 `LINKED` 表示 ARP 表里已经有客户端（真拿到 IP 了），`WAITING` 表示还没有
- 底部提示会区分四种情况：隧道关 / 有发无回 / 正常转发 / 等手机接入
- 内容变化才重绘（300 ms 采样），全屏 115 KB 走 40 MHz SPI 约 25 ms，不闪

**这块屏的三个坑**：

1. `display_offset` 必须是 `(0, 0)`。ST7789 显存是 240×320，这块面板显示的是 GRAM 的
   0~239 行；按常见的 `(0, 80)` 写会让顶部 80 行永远没被写过，上电就是一条雪花噪点。
2. `invert_colors(ColorInversion::Inverted)` 不能关，这块 IPS 面板需要全局反色，关掉变黑白。
3. 内置点阵字库只有 ASCII，界面全英文。中文要自己烧几 MB 字库，对一块状态屏不值当。

## 实现要点

- `src/apdrv.rs`：在以太网帧层包一层 `embassy-net-driver`。广播、ARP、目标 IP 是本机的
  帧交给 `embassy-net`（DHCP / HTTP / ARP 应答）；其余整条 IP 包封进 UDP 发给手机；
  手机回来的包解封装后按 ARP 表还原成以太网帧。超过 MTU 的内层包做分片。
- `src/dhcp.rs`：自己实现的 DHCP 服务器（见下节）。
- `src/http.rs`：手写 HTTP/1.1，不引框架，页面内联。
- `src/display.rs`：ST7789 + `mipidsi`，只在状态变化时重绘。
- `src/state.rs`：热路径上全是原子变量，配置用临界区保护。

## 踩过的坑

**1. DHCP 回包源地址成了 `0.0.0.0`**

`esp-hal-dhcp-server` 把 socket 绑在 `0.0.0.0:67`。收广播必须这么绑，但 smoltcp 在
`UdpMetadata.local_address` 为空时会拿 socket 的绑定地址当源地址，于是 OFFER 的源 IP
变成 `0.0.0.0`。手机内核把它当 martian 包直接丢弃 —— 串口上看 OFFER 明明发出去了，
手机却一直重发 DISCOVER，最后超时断开。修法是发送时显式指定
`local_address = Some(192.168.4.1)`。

**2. DHCP 回包太短、缺 option**

`edge_dhcp` 的 `options.reply()` 只在客户端 PRL（option 55）里列了某项时才回对应 option，
拼出来只有 279 字节，低于 RFC 2131 要求的 300 字节最小长度。手机对这种包同样静默丢弃。
最终改成手工拼包：固定写 message type / server id / lease / subnet mask / router / dns /
broadcast，并 pad 到 300 字节。

**3. FOU 出口节点自己的包又回到网关（环路）**

手机既是隧道出口，又把默认路由指向 `192.168.4.1`，它自己的出站包会回到 ESP32。
不拦截就是「解封装 → 出站 → 回到 ESP32 → 再封装」的无限乒乓，隧道流量一直涨却上不了网。
`apdrv` 里对源地址等于 FOU 目标 IP 的包直接丢弃并计入 `loop`。

**4. Wi-Fi 事件默认不开**

`esp-radio` 只默认使能 Start/Stop/Connected，ProbeRequest 要自己
`enable_wifi_events`。不开的话手机在扫但连不上时串口一片安静，完全看不出问题在哪。

## 相关

- 安卓端：[pocket-gateway](https://github.com/dosgo/pocket-gateway)（FOU / SOCKS5 / WireGuard 三条通道）
