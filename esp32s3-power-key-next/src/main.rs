//! ESP32-S3 USB HID 电脑电源控制器 + Blinker 远程控制
//!
//! 把自己模拟成 USB 键盘 + 系统控制设备，插在电脑上。
//! 手机 Blinker App 点按钮 -> Blinker 云(MQTT) -> ESP32 -> USB HID -> 电脑睡眠/唤醒。
//!
//! ## 网络：AP + STA 同时开，不用切模式
//!
//!   - **热点（AP）永远开着**：默认 `ESP32-POWER` / 192.168.4.1（名字和密码可在网页里改），
//!     用来配网和救急
//!   - **联网（STA）**：配过就开机自动连；MQTT 需要外网，没连上就控制不了
//!   - **配网**：连热点 → 浏览器开 192.168.4.1 → 填 WiFi 账号密码 → 保存
//!   - **恢复出厂**：长按 BOOT 3 秒（短按是唤醒电脑）
//!
//! 烧写：cargo run --release

#![no_std]
#![no_main]

mod ble;
mod dns;
mod hid;
mod lcd;
mod mqtt;
mod soc_temp;
mod store;
mod web;

use core::fmt::Write as _;
use core::str;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::{
    Config as NetConfig, IpAddress, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
    StaticConfigV4,
    dns::DnsQueryType,
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embassy_usb::class::hid::{
    Config as HidConfig, HidBootProtocol, HidSubclass, HidWriter, State as HidState,
};
use embassy_usb::{Builder, Config as UsbConfig, Handler};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    rng::Rng,
    timer::timg::TimerGroup,
    usb::otg::{
        Usb,
        embassy_usb_device::{Config as UsbOtgConfig, Driver},
    },
};
use esp_hal_dhcp_server::{
    Ipv4Addr as DhcpIp, run_dhcp_server, simple_leaser::SimpleDhcpLeaser,
    structs::DhcpServerConfig,
};
use embassy_usb_synopsys_otg::otg_v1::Otg;
use esp_println::println;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig, Interface, Password,
    PowerSaveMode, Ssid, WifiController, ap::AccessPointConfig, sta::StationConfig,
};
use esp_storage::FlashStorage;
use heapless::String as HString;
use log::{error, info, warn};

esp_bootloader_esp_idf::esp_app_desc!();

// ===================== 省电配置 =====================
//
// 背景：Wi-Fi AP + STA 双开、BLE 常驻广播、240MHz 主频，实测 5V/1.2W（约 240mA）。
// 上面每一项都有独立的省电档位，集中放在这里方便调。
//
// 有一条硬性下限：esp-radio 要求 CPU ≥ 80MHz（内部 assert），所以不能低于 80。

/// CPU 主频。**80MHz 是 esp-radio 允许的下限**（内部 assert ≥80）。
///
/// 键盘/鼠标不受影响：一次按键的 CPU 处理只有几微秒（AES 有硬件加速），
/// 而链路的延迟量级是 10ms（USB 轮询）到 几十 ms（WiFi DTIM），CPU 占比 <1%。
/// 降主频唯一能感觉到的是网页渲染慢一点、以及大流量时输入抖动从 <1ms 涨到几 ms。
const CPU_CLOCK: CpuClock = CpuClock::_80MHz;

/// Wi-Fi 省电模式。esp-radio 默认是 `None`（射频常开，最费电）。
/// - `Minimum`：DTIM 级别休眠，延迟几十毫秒，网页/MQTT 几乎无感 ← 用这个
/// - `Maximum`：睡得更狠，但下载 24KB 页面会明显变慢
const WIFI_PS: PowerSaveMode = PowerSaveMode::Minimum;

/// Wi-Fi 最大发射功率，单位 0.25dBm：**72 = 18dBm**（芯片默认上限是 84 = 20dBm）。
///
/// 走过一轮弯路，记录一下：
///   13dBm(52) → 手机 3 米外只剩 2 格；17dBm(68) 偏弱；20dBm(84) 覆盖最好。
/// 最后定在 **18dBm**：比拉满只低 2dB（覆盖差别很小），
/// 但发射峰值电流能降一些 —— 这块 USB-A 直插 dongle 天线贴着机箱，
/// 效率本来就差，功率不能降太多，18 是折中。
///
/// **这项对平均功耗影响很小**（只影响发射瞬间的峰值电流，空闲不耗电），
/// 真正省电的是 `WIFI_PS` 和 `CPU_CLOCK`，别在这里抠。
const WIFI_TX_POWER: i8 = 72;

/// BLE 广播间隔（毫秒）。默认 160ms；调到 500ms 手机照样能扫到，
/// 但射频占用降到 1/3。已经连上之后就不广播了，所以只影响"找设备"阶段。
const BLE_ADV_INTERVAL_MS: u64 = 500;

// ===================== 需要你填写的配置 =====================
//
// WiFi 账号密码**不用写在这里** —— 连热点在网页里配（见 src/store.rs 和 setup.html）。

/// Blinker 设备凭据：浏览器打开
/// https://iot.diandeng.tech/api/v1/user/device/diy/auth?authKey=你的authKey&protocol=mqtt
/// 从返回的 JSON 里取 deviceName / iotId / iotToken 三个值填到这里
const BLINKER_DEVICE_NAME: &str = "110C67F2RW4MXTEHG29MQ8YZ"; // deviceName，也是 MQTT clientId
const BLINKER_IOT_ID: &str = "110C67F2RW4MXTEHG29MQ8YZ"; // iotId，MQTT username
const BLINKER_IOT_TOKEN: &str = "mh5401ehl78TQnBkszzFuZNX50AzlLD"; // iotToken，MQTT password

/// 电脑网卡 MAC，用于 WOL 魔术包唤醒。
/// 本机 WiFi 网卡 Intel AX210：28-0C-50-F4-8C-9C
/// 注意：现在走纯 USB 唤醒，`ENABLE_WOL = false`，这个值只备用。
const PC_MAC: [u8; 6] = [0x28, 0x0C, 0x50, 0xF4, 0x8C, 0x9C];

/// 要不要在唤醒时顺带发 WOL 魔术包。
/// 关掉 = 纯 USB 唤醒（本机只用这个）。打开 = 多一条网络唤醒的后路。
const ENABLE_WOL: bool = false;
// ===========================================================

const BROKER_HOST: &str = "broker.diandeng.tech";
const BROKER_PORT: u16 = 1883;

/// 想固定局域网地址就开这个（省得每次找 IP）。开着不影响热点。
const USE_STATIC_IP: bool = false;
const STATIC_IP: Ipv4Address = Ipv4Address::new(192, 168, 1, 61);
const GATEWAY_IP: Ipv4Address = Ipv4Address::new(192, 168, 1, 1);

/// STA 单次连接最多等多久（秒）
const STA_CONNECT_TIMEOUT: u64 = 15;
/// 连不上时隔多久重试（秒）
const STA_RETRY_INTERVAL: u64 = 10;
/// 长按 BOOT 这么久 → 恢复出厂设置（清空配置区）并重启。
/// 短按（不到这个时长）仍然是"唤醒电脑"。
const BOOT_RESET_MS: u64 = 3000;

/// 冷启动保护：上电后这段时间内，**MQTT 下发的电源指令一律丢弃**。
///
/// 原因：blinker_task 一启动就会连 broker 并订阅，broker 可能推来 retained 的
/// 旧指令、App 也可能在设备上线瞬间同步一次状态。没有这道保护，
/// 表现就是"配好网、插上电脑没多久就自己睡了"。
///
/// 5 秒：够覆盖"连 WiFi → DHCP → MQTT 握手 → 订阅 → 心跳"这段自动噪音期，
/// 又不至于让断电重启后手机点不动。要更保守可以改回 10_000。
///
/// 网页按钮 / 串口 / BOOT 键不受这个限制 —— 那些是人当场操作的。
const MQTT_GUARD_MS: u64 = 5_000;

/// 临时总闸：MQTT 是否允许触发电源动作。
///
/// 2026-10-25 排查期间一度设成 `false`。现在元凶已经定位到 Wake 分支里的
/// `[2, 0x04]`（已删除），配合 parse_power_cmd 的严格匹配 + 冷启动保护，
/// 这条链路已经安全，恢复启用。
const MQTT_POWER_ENABLE: bool = true;

/// 诊断开关（2026-10-25）：`true` = 彻底禁掉一切主动 HID 输出。
///
/// 二分结论：
///   - 全部抑制 → 电脑不睡
///   - BOOT 键启用但报告仍抑制（remotewakeup 裸寄存器照跑）→ 电脑不睡
/// 所以 `remote_wakeup_hw()` 是无辜的，元凶在 **HID 报告**本身。
/// 现在改回 `false` 恢复发送，改用 POWER_REPORT_ENABLE 精确控制危险报告。
const DIAG_NO_HID_OUTPUT: bool = false;

/// 系统控制报告（Sleep / PowerDown）总开关。
///
/// 2026-10-25 二分结论：`[2, 0x04]` System Wake Up 才是让电脑"一插上就睡"的元凶
/// （Windows 事件日志 `Button or Lid`）。它已从 Wake 分支里永久删除。
///
/// Sleep / PowerDown 这两种报告本身**没有**被证明有害，所以恢复启用。
/// 真出问题时改回 `false` 即可一键隔离。
const POWER_REPORT_ENABLE: bool = true;

/// Wake 去抖：GPIO0 抖动 / 报文重发时，这段时间内只响应一次。
const WAKE_DEBOUNCE_MS: u64 = 2000;

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const HID_REPORT_DESC: &[u8] = &[
    // ---------- 键盘 (Report ID 1) ----------
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x03, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
    // ---------- 系统控制 (Report ID 2) ----------
    0x05, 0x01, 0x09, 0x80, 0xA1, 0x01, 0x85, 0x02, 0x19, 0x81, 0x29, 0x83, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x95, 0x05, 0x81, 0x03, 0xC0,
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PowerCmd {
    Wake,
    Sleep,
    PowerDown,
}

static CMD_CHAN: Channel<CriticalSectionRawMutex, PowerCmd, 8> = Channel::new();

/// 唤醒请求：hid_task 收到 Wake 时先 signal 一下，让 usb_task 去把挂起的总线拉起来。
/// 电脑睡（S0 低电量待机）之后 USB 总线是挂起状态，这时候直接 write 会超时，
/// 必须先做 USB 远程唤醒（remote wakeup）把总线恢复到 active。
static USB_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

// ================= 网页键鼠（从 hid-kvm 移植）=================
//
// 热点和局域网各跑一个 web server，两边都能下发键鼠指令，但 USB 的 HidWriter
// 只有一份、不能 clone，所以走队列：web 任务只管塞事件，hid_task 单独消费写 USB。
//
// 键盘**复用**原来那个 boot 6KRO 接口（Report ID 1），所以这里存的是 8 字节
// 裸报告（mod + 保留 + 6 键），写的时候前面补一个 0x01。

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidEvent {
    /// boot 键盘报告 8 字节：[mod, 0, k1..k6]
    Kbd([u8; 8]),
    /// 鼠标：[btn, dx, dy, wheel]
    Mouse([u8; 4]),
    /// 媒体键：16 位 usage（小端），0 = 松开
    Cc([u8; 2]),
}

pub static HID_Q: Channel<CriticalSectionRawMutex, HidEvent, 24> = Channel::new();

// 下面 push_hid / release_all 两个函数放在 dlog! 宏定义之后（macro_rules 是文本作用域）

// ================= 诊断日志：手机连热点打开 http://192.168.4.1/log 查看 =========
//
// 为什么需要：这块是 USB-A 直插的 dongle 板，只有一个 USB 口，OTG 一旦接管，
// USB-Serial-JTAG 就没了，串口日志一个字节也看不到。但板子在电脑睡眠期间
// 仍然有电、热点还在广播，所以把关键事件塞进环形缓冲，用手机就能读回来。

pub const LOG_LINES: usize = 48;
pub const LOG_LINE_LEN: usize = 96;

pub struct LogRing {
    pub buf: [[u8; LOG_LINE_LEN]; LOG_LINES],
    pub len: [u8; LOG_LINES],
    next: usize,
    pub total: u32,
}

impl LogRing {
    const fn new() -> Self {
        Self {
            buf: [[0u8; LOG_LINE_LEN]; LOG_LINES],
            len: [0u8; LOG_LINES],
            next: 0,
            total: 0,
        }
    }

    fn push(&mut self, s: &[u8]) {
        let i = self.next;
        let n = s.len().min(LOG_LINE_LEN);
        self.buf[i][..n].copy_from_slice(&s[..n]);
        self.len[i] = n as u8;
        self.next = (i + 1) % LOG_LINES;
        self.total = self.total.wrapping_add(1);
    }

    /// 返回 (起始下标, 有效行数)
    pub fn order(&self) -> (usize, usize) {
        let t = self.total as usize;
        if t < LOG_LINES {
            (0, t)
        } else {
            (self.next, LOG_LINES)
        }
    }
}

pub static LOGS: Mutex<CriticalSectionRawMutex, LogRing> = Mutex::new(LogRing::new());

/// 写一行诊断日志。拿不到锁就丢弃 —— 绝不能因为写日志把 USB 关键路径卡住。
pub fn push_log(args: core::fmt::Arguments<'_>) {
    struct W<'a> {
        s: &'a mut [u8],
        n: usize,
    }
    impl core::fmt::Write for W<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let room = self.s.len() - self.n;
            let sb = s.as_bytes();
            let take = sb.len().min(room);
            self.s[self.n..self.n + take].copy_from_slice(&sb[..take]);
            self.n += take;
            Ok(())
        }
    }
    if let Ok(mut g) = LOGS.try_lock() {
        let mut line = [0u8; LOG_LINE_LEN];
        let mut w = W {
            s: &mut line,
            n: 0,
        };
        let _ = core::fmt::write(&mut w, args);
        let n = w.n;
        g.push(&line[..n]);
    }
}

macro_rules! dlog {
    ($($a:tt)*) => {
        $crate::push_log(format_args!($($a)*))
    };
}

/// 队列满就丢一帧（鼠标位移丢一帧无所谓，不能让整条队列把键盘堵住）
///
/// 同时喂两路：USB（HID_Q）和 BLE（ble::BLE_Q）。
/// 两个队列各自独立限深，蓝牙断了也不影响 USB 那条老路。
fn push_hid(ev: HidEvent) {
    if HID_Q.try_send(ev).is_err() {
        dlog!("[hid] 队列满，丢一帧");
    }
    // 蓝牙没连上时事件没人取，队列满了直接丢，不打扰 USB
    let _ = ble::BLE_Q.try_send(ev);
}

/// 松开所有键：切窗口 / 断开连接 / 点「释放所有键」都走这里，防卡键。
pub fn release_all() {
    push_hid(HidEvent::Kbd([0u8; 8]));
    push_hid(HidEvent::Cc([0u8; 2]));
    push_hid(HidEvent::Mouse([0u8; 4]));
}

/// 网络状态，给配网页的 /status 用
pub struct NetInfo {
    /// 0=没配过 1=连接中 2=已连接 3=失败
    pub state: u8,
    /// STA 在局域网里拿到的地址
    pub ip: Option<[u8; 4]>,
    /// 最近一次连接失败的原因，配网页上直接显示出来，省得来回猜
    pub err: heapless::String<56>,
    /// STA 的信号强度（dBm，负值，越接近 0 越好）。没连上是 0。
    pub rssi: i8,
}

pub static NET: Mutex<CriticalSectionRawMutex, NetInfo> = Mutex::new(NetInfo {
    state: 0,
    ip: None,
    err: heapless::String::new(),
    rssi: 0,
});

/// flash 句柄（保存配网要写）。`FlashStorage::new` 只能调一次，又不能 clone，
/// 所以丢进这个全局盒子给 web / 按键任务用。
pub static FLASH: Mutex<CriticalSectionRawMutex, Option<FlashStorage<'static>>> = Mutex::new(None);

/// 芯片温度（单位 0.1℃），由 lcd_task 每秒更新，/status 直接读。
/// 用原子量而不是 Mutex：读的地方多、写的地方只有一个，没必要加锁。
pub static TEMP_DECI: core::sync::atomic::AtomicI16 = core::sync::atomic::AtomicI16::new(0);

/// flash 里有没有蓝牙配对信息。
/// 给设置页显示用，避免每次轮询 /status 都去读 flash（读一次也行，但没必要）。
pub static BOND_PRESENT: AtomicBool = AtomicBool::new(false);

/// 运行时生效的 Blinker 凭据。优先用 flash 里存的（网页可改），
/// 没存过就回退到下面那几个编译期常量。
pub static BLINKER_CFG: Mutex<CriticalSectionRawMutex, store::BlinkerCreds> =
    Mutex::new(store::BlinkerCreds::new());

/// Blinker 是否在线（MQTT 已订阅成功）。给 LCD 显示用。
/// 注意这和 App 上显示的"在线"是两码事：App 看的是心跳回复，这里看的是 MQTT 会话。
pub static BLINKER_ONLINE: AtomicBool = AtomicBool::new(false);

/// 网页保存 Blinker 凭据：写 flash + 更新内存。调用方负责重启。
pub async fn save_blinker_cfg(c: store::BlinkerCreds) -> bool {
    let mut g = FLASH.lock().await;
    let ok = match g.as_mut() {
        Some(f) => store::save_blinker(f, &c),
        None => false,
    };
    drop(g);
    if ok {
        *BLINKER_CFG.lock().await = c;
    }
    ok
}

/// 网页保存热点（AP）名字/密码：写 flash + 更新内存。**调用方负责重启**，
/// 因为热点参数是射频初始化时用的，改完必须重新 start 才会广播新名字。
pub async fn save_ap_cfg(c: store::ApCfg) -> bool {
    let mut g = FLASH.lock().await;
    let ok = match g.as_mut() {
        Some(f) => store::save_ap(f, &c),
        None => false,
    };
    drop(g);
    if ok {
        *AP_CFG.lock().await = c;
    }
    ok
}

/// 网页提交了新的 WiFi 账号密码 → 通知 wifi_task 去连
pub static NEW_CREDS: Signal<CriticalSectionRawMutex, store::Creds> = Signal::new();

/// 当前生效的热点（AP）配置。开机时从 flash 读一次，网页改完写 flash + 重启生效。
pub static AP_CFG: Mutex<CriticalSectionRawMutex, store::ApCfg> = Mutex::new(store::ApCfg::new());

/// 热点（AP）的配置。每次改 WiFi 模式都得带上它，否则热点会被关掉。
///
/// 名字/密码来自 flash（网页可改），没配过就用出厂值 `ESP32-POWER` / `power1234`。
/// 把字符串转成 esp-radio 的 SSID / Password。
///
/// esp-radio 1.0 把 SSID 和密码都换成了强类型（`Ssid` / `Password`），只能 TryFrom。
/// SSID 超 32 字节、密码不在 8~63 都会失败，这里全部降级处理，绝不 unwrap
/// （no_std 里 panic 就是整机死机，而这是网页表单来的输入）。
fn to_ssid(s: &str) -> esp_radio::wifi::Ssid {
    // 出厂 SSID 一定合法，兜底用它
    Ssid::try_from(s).unwrap_or_else(|_| Ssid::try_from("ESP32-POWER").unwrap())
}

fn ap_config(c: &store::ApCfg) -> AccessPointConfig {
    let base = AccessPointConfig::default()
        .with_ssid(to_ssid(c.ssid_or_default()))
        .with_max_connections(4);
    let pass = c.pass_or_default();
    if pass.is_empty() {
        // 开放网络：不能带认证，否则 esp_wifi 会报参数错误
        base
    } else {
        match Password::try_from(pass) {
            Ok(pw) => base.with_authentication(AuthenticationMethodConfig::Wpa2Personal(pw)),
            // 密码非法（长度不在 8~63）时降级成开放网络，总比整机起不来强
            Err(_) => base,
        }
    }
}

/// 联网（STA）的配置：认证方式必须跟着密码走。
///
/// 坑：如果密码是空的（没配过网、或者用户填的是开放网络），
/// esp_wifi_set_config 会返回 ESP_ERR_WIFI_PASSWORD，整个 wifi 初始化就失败了。
/// 所以空密码一律按开放网络（Open）处理。
fn sta_config(c: &store::Creds) -> StationConfig {
    let sta = StationConfig::default().with_ssid(to_ssid(c.ssid.as_str()));
    if c.pass.is_empty() {
        sta.with_authentication(AuthenticationMethodConfig::Open)
    } else {
        match Password::try_from(c.pass.as_str()) {
            Ok(pw) => sta.with_authentication(AuthenticationMethodConfig::Wpa2Personal(pw)),
            Err(_) => sta.with_authentication(AuthenticationMethodConfig::Open),
        }
    }
}

/// 还没配网时用的"占位" STA 配置，永远不会被真正去连。
///
/// 为什么要它：WiFi 模式从 AP 切成 APSTA 时，esp-radio 会 `stop()` 再 `start()`
/// 整个射频，热点会瞬间消失 —— 手机正在提交配网表单，那条 HTTP 连接就被掐断，
/// 页面一直转圈（这就是之前"保存卡住"的根因之一）。
/// 开机就用 APSTA（哪怕 STA 是空的、不连），模式从头到尾不变，
/// 后面配网时 `set_config` 只热更新参数，射频不重启，热点不断。
fn placeholder_sta() -> StationConfig {
    StationConfig::default()
        .with_ssid(to_ssid("esp32-idle"))
        .with_authentication(AuthenticationMethodConfig::Open)
}

/// 网页上点「唤醒」走这里。热点侧和局域网侧的 web 任务共用同一个通道。
pub async fn request_wake() {
    dlog!("[web] 请求 Wake");
    CMD_CHAN.send(PowerCmd::Wake).await;
}

/// 网页上点「睡眠」走这里。
pub async fn request_sleep() {
    dlog!("[web] 请求 Sleep");
    CMD_CHAN.send(PowerCmd::Sleep).await;
}

/// 只清蓝牙配对（WiFi / Blinker 保留），返回是否成功。
/// 清完由调用方负责重启 —— 协议栈里的 bond 只在内存里，不重启是清不干净的。
pub async fn clear_bond_flash() -> bool {
    let ok = {
        let mut g = FLASH.lock().await;
        match g.as_mut() {
            Some(f) => store::clear_bond(f),
            None => false,
        }
    };
    if ok {
        BOND_PRESENT.store(false, Ordering::Relaxed);
        dlog!("[web] 蓝牙配对已清空（需重启生效）");
    }
    ok
}

/// 恢复出厂：擦掉整个配置区（WiFi 账号密码 + 将来所有持久化设置）
pub async fn factory_reset_flash() -> bool {
    let mut g = FLASH.lock().await;
    match g.as_mut() {
        Some(f) => store::factory_reset(f),
        None => {
            warn!("[boot] flash 不可用");
            false
        }
    }
}

struct MyDeviceHandler {
    configured: AtomicBool,
}

impl MyDeviceHandler {
    fn new() -> Self {
        Self {
            configured: AtomicBool::new(false),
        }
    }
}

impl Handler for MyDeviceHandler {
    fn enabled(&mut self, enabled: bool) {
        self.configured.store(false, Ordering::Relaxed);
        dlog!("[usb] enabled={enabled}");
        info!("USB enabled={enabled}");
    }
    fn reset(&mut self) {
        self.configured.store(false, Ordering::Relaxed);
        dlog!("[usb] bus reset");
    }
    fn addressed(&mut self, addr: u8) {
        self.configured.store(false, Ordering::Relaxed);
        dlog!("[usb] addressed={addr}");
    }
    fn configured(&mut self, configured: bool) {
        self.configured.store(configured, Ordering::Relaxed);
        dlog!("[usb] configured={configured}");
        info!("USB configured={configured}");
    }
    /// 总线进/出挂起。电脑进睡眠时应该看到 suspended=true —— 没有就说明
    /// 主机根本没挂起这根口，那 HID 报文本该能直接写进去。
    fn suspended(&mut self, suspended: bool) {
        dlog!("[usb] suspended={suspended}");
    }
    /// 主机是否允许本设备远程唤醒（对应 SET_FEATURE DEVICE_REMOTE_WAKEUP）。
    /// 这个 false 的话 remote_wakeup() 一定失败。
    fn remote_wakeup_enabled(&mut self, enabled: bool) {
        dlog!("[usb] 远程唤醒允许={enabled}");
    }
}

#[embassy_executor::task]
async fn usb_task(mut device: embassy_usb::UsbDevice<'static, Driver<'static>>) {
    // 不能只写 device.run().await —— 那样没机会调 remote_wakeup()。
    // 这里用 select 把 run() 和"唤醒请求"并起来：收到请求就把 run() 的 future 丢掉，
    // 拿回 &mut device 去做远程唤醒，然后重新进 run()。
    loop {
        match select(device.run(), USB_WAKE.wait()).await {
            // run() 返回类型是 `!`，永远走不到这里
            Either::First(_) => {}
            Either::Second(()) => {
                dlog!("[usb] 收到唤醒请求");
                // 先走 embassy 的正规通道（同时会把内部 suspended 标记清掉）
                match device.remote_wakeup().await {
                    Ok(()) => {
                        dlog!("[usb] 软唤醒 OK");
                    }
                    Err(e) => {
                        // 常见是 InvalidState：要么总线没挂起，要么主机没发
                        // SET_FEATURE(DEVICE_REMOTE_WAKEUP)。这时候库里什么都不做，
                        // 必须自己打寄存器，否则一个字节都不会发到总线上。
                        dlog!("[usb] 软唤醒失败 {e:?}，改为直接打 DCTL.RWUSIG");
                        remote_wakeup_hw().await;
                    }
                }
            }
        }
    }
}

/// 硬件级 USB 远程唤醒：直接把 OTG 的 `DCTL.RWUSIG` 拉高 10ms，
/// 在 D+/D- 上打出 resume K 状态。这条路不看 embassy-usb 的
/// `suspended / remote_wakeup_enabled` 门禁，也不看主机有没有发 SET_FEATURE。
///
/// USB 2.0 §7.1.7.7 规定 TDRSMUP 是 1~15ms，这里取 10ms。
async fn remote_wakeup_hw() {
    // 复用 esp-hal 的 USB_FS 基址，避免硬编码地址
    let regs = unsafe { Otg::from_ptr(esp_hal::peripherals::USB_FS::PTR.cast_mut().cast::<()>()) };

    // 挂起期间 PHY 时钟会被门控掉，先放出来，否则 RWUSIG 写进去也不动
    regs.pcgcctl().modify(|w| {
        w.set_stppclk(false);
    });
    regs.dctl().modify(|w| w.set_rwusig(true));
    Timer::after(Duration::from_millis(10)).await;
    regs.dctl().modify(|w| w.set_rwusig(false));
    dlog!("[usb] 已发出 resume 信号（RWUSIG 10ms）");
}

#[embassy_executor::task]
async fn hid_task(
    mut writer: HidWriter<'static, Driver<'static>, 9>,
    mut mouse: HidWriter<'static, Driver<'static>, 4>,
    mut cc: HidWriter<'static, Driver<'static>, 2>,
    stack: Stack<'static>,
) {
    info!("hid task started");
    // socket 只建一次：mk_static! 是对 StaticCell 做 write，写第二次会 panic，
    // 所以不能像原来那样放在 send_wol() 里面（每次唤醒都重新 mk_static）。
    let mut wol = wol_socket(stack);
    // Wake 去抖用：上次真正执行 Wake 的时刻
    let mut last_wake: Option<Instant> = None;
    loop {
        // 电源指令（Blinker / 网页按钮 / BOOT 键）和网页键鼠挤在同一个任务里：
        // USB 的 HidWriter 只有一份，没法再开一个任务各拿一个。
        let cmd = match select(CMD_CHAN.receive(), HID_Q.receive()).await {
            Either::First(c) => c,
            Either::Second(ev) => {
                if DIAG_NO_HID_OUTPUT {
                    // 诊断期连网页键鼠也不发，保证整个固件零 HID 输出
                    dlog!("[diag] HID 事件已禁用: {ev:?}");
                    continue;
                }
                match ev {
                    // 复用电源键原来的键盘 interface（Report ID 1），前面补一个 ID
                    HidEvent::Kbd(r) => send(&mut writer, &[1, r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]).await,
                    HidEvent::Mouse(r) => {
                        if with_timeout(Duration::from_millis(200), mouse.write(&r))
                            .await
                            .is_err()
                        {
                            dlog!("[hid] 鼠标报告超时（电脑没枚举？）");
                        }
                    }
                    HidEvent::Cc(r) => {
                        if with_timeout(Duration::from_millis(200), cc.write(&r))
                            .await
                            .is_err()
                        {
                            dlog!("[hid] 媒体键报告超时");
                        }
                    }
                }
                continue;
            }
        };
        info!("executing: {cmd:?}");

        match cmd {
            PowerCmd::Wake => {
                dlog!("[hid] 收到 Wake");
                // 去抖：GPIO0 抖动或报文重发时，短时间内只响应一次，
                // 否则会连着往主机灌报告，把总线搞乱。
                if let Some(t) = last_wake {
                    if (Instant::now() - t).as_millis() < WAKE_DEBOUNCE_MS {
                        dlog!("[hid] Wake 去抖，忽略（{WAKE_DEBOUNCE_MS}ms 内重复）");
                        continue;
                    }
                }
                last_wake = Some(Instant::now());

                // ① 可选：WOL 魔术包（本机默认关闭，走纯 USB）
                if ENABLE_WOL {
                    send_wol(&mut wol, stack).await;
                }
                // ② 让 usb_task 把总线拉起来（含硬件级 RWUSIG 兜底）
                USB_WAKE.signal(());
                // 主机恢复总线要几十毫秒，等一会儿再发 HID 报文，否则会写超时
                Timer::after(Duration::from_millis(120)).await;
                // ③ 只发一个空格键（S0 待机把键盘输入当用户活动，最保险）
                //
                // 原来这里还有一条 `[2, 0x04]` = System Wake Up (0x83)。
                // 2026-10-25 排查"插上就睡"时把它删了：Windows 事件日志给的
                // 睡眠原因是 "Button or Lid"，而 0x83 在系统**醒着**时被收到，
                // 有把主机电源状态搞乱的风险；空格键已经足够唤醒，不需要它。
                send(&mut writer, &[1, 0x00, 0x00, 0x2C, 0, 0, 0, 0, 0]).await; // Space
                Timer::after(Duration::from_millis(40)).await;
                send(&mut writer, &[1, 0, 0, 0, 0, 0, 0, 0, 0]).await; // release
            }
            PowerCmd::Sleep => {
                if !POWER_REPORT_ENABLE {
                    dlog!("[hid] System Sleep 报告已被 POWER_REPORT_ENABLE 禁用");
                    continue;
                }
                // 这行日志以前没有，导致 /log 里看不到"是谁把电脑弄睡的"，排查时抓瞎
                dlog!("[hid] 收到 Sleep -> 发送系统睡眠报告");
                send(&mut writer, &[2, 0x02]).await; // System Sleep
                Timer::after(Duration::from_millis(60)).await;
                send(&mut writer, &[2, 0x00]).await;
            }
            PowerCmd::PowerDown => {
                if !POWER_REPORT_ENABLE {
                    dlog!("[hid] System Power Down 报告已被 POWER_REPORT_ENABLE 禁用");
                    continue;
                }
                dlog!("[hid] 收到 PowerDown -> 发送关机报告");
                send(&mut writer, &[2, 0x01]).await; // System Power Down
                Timer::after(Duration::from_millis(60)).await;
                send(&mut writer, &[2, 0x00]).await;
            }
        }
    }
}

async fn send(writer: &mut HidWriter<'static, Driver<'static>, 9>, data: &[u8]) {
    if DIAG_NO_HID_OUTPUT {
        // 二分诊断第二轮：所有 HID 报告都在这里被拦掉，
        // 但 Wake 分支里的 USB_WAKE.signal / remote_wakeup_hw 仍然会跑 ——
        // 就是为了单独验证"裸寄存器发 resume 信号"这条路径有没有问题。
        dlog!("[diag] 抑制 HID 报告 {:02X?}", data);
        return;
    }
    if with_timeout(Duration::from_millis(300), writer.write(data))
        .await
        .is_err()
    {
        dlog!("[hid] 写入超时 report={:02X?}", data);
        warn!("HID write timeout (host not enumerated?)");
    }
}

/// 本地 BOOT 键（只有一个键，靠按住时长区分）：
///
/// | 按住 | 结果 |
/// |---|---|
/// | 松手 < 3 秒 | **唤醒电脑**（原来那个验证 HID 的功能，保留了） |
/// | 按住 ≥ 3 秒 | **恢复出厂设置**并重启（清空配置区） |
#[embassy_executor::task]
async fn button_task(mut button: Input<'static>) {
    info!("button task started：短按唤醒 / 长按 {BOOT_RESET_MS}ms 恢复出厂");
    loop {
        button.wait_for_low().await;
        let t0 = Instant::now();

        // 一直按着就等到 3 秒 → 恢复出厂；提前松手 → 当作短按唤醒
        let mut factory = false;
        while button.is_low() {
            if (Instant::now() - t0).as_millis() >= BOOT_RESET_MS {
                factory = true;
                break;
            }
            Timer::after(Duration::from_millis(50)).await;
        }

        if factory {
            info!("[boot] 长按 {BOOT_RESET_MS}ms -> 恢复出厂设置并重启");
            if !factory_reset_flash().await {
                warn!("[boot] 恢复出厂失败（flash 里那块没擦掉）");
            }
            Timer::after(Duration::from_millis(400)).await;
            esp_hal::system::software_reset();
        } else {
            dlog!("[boot] 短按 -> Wake");
            info!("BOOT 短按 -> 唤醒电脑");
            CMD_CHAN.send(PowerCmd::Wake).await;
            // 等松手再继续，免得一直重复触发
            while button.is_low() {
                Timer::after(Duration::from_millis(50)).await;
            }
            Timer::after(Duration::from_millis(200)).await;
        }
    }
}

/// 建一次 WOL 用的 UDP socket。缓冲必须是 static，所以只能建一次。
fn wol_socket(stack: Stack<'static>) -> UdpSocket<'static> {
    let rx_meta = mk_static!([PacketMetadata; 2], [PacketMetadata::EMPTY; 2]);
    let rx_buf = mk_static!([u8; 128], [0u8; 128]);
    let tx_meta = mk_static!([PacketMetadata; 2], [PacketMetadata::EMPTY; 2]);
    let tx_buf = mk_static!([u8; 256], [0u8; 256]);
    UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf)
}

/// WOL 魔术包：6 个 0xFF + MAC 重复 16 次。
///
/// 目的地址发两个：**子网广播**（很多路由器/AP 会吞掉 255.255.255.255）
/// 和受限广播都补一发，哪个能到算哪个。
async fn send_wol(sock: &mut UdpSocket<'static>, stack: Stack<'static>) {
    if PC_MAC == [0u8; 6] {
        return;
    }
    let mut packet = [0xFFu8; 102];
    for i in 0..16 {
        packet[6 + i * 6..12 + i * 6].copy_from_slice(&PC_MAC);
    }

    if !sock.is_open() && sock.bind(9).is_err() {
        warn!("WOL: bind 失败");
        return;
    }

    let mut dsts = [Ipv4Address::new(255, 255, 255, 255); 2];
    if let Some(cfg) = stack.config_v4() {
        // 按当前前缀算子网广播地址，比如 192.168.88.160/24 -> 192.168.88.255
        let ip = cfg.address.address().octets();
        let pl = cfg.address.prefix_len();
        let mut bcast = [0u8; 4];
        for i in 0..4 {
            let bits = (pl as i32 - (i as i32) * 8).clamp(0, 8) as u8;
            let mask = if bits == 0 { 0u8 } else { 0xFFu8 << (8 - bits) };
            bcast[i] = ip[i] | !mask;
        }
        dsts[0] = Ipv4Address::new(bcast[0], bcast[1], bcast[2], bcast[3]);
    } else {
        warn!("WOL: 联网侧还没拿到 IP，只发受限广播");
        dsts[1] = Ipv4Address::new(255, 255, 255, 255);
    }

    for d in dsts {
        match sock.send_to(&packet, (IpAddress::Ipv4(d), 9)).await {
            Ok(()) => info!("WOL 魔术包 -> {d} (MAC {:02X?})", PC_MAC),
            Err(e) => warn!("WOL 发往 {d} 失败: {e:?}"),
        }
    }
}

/// Blinker：连 MQTT，订阅设备 topic，把指令转成电源动作
/// 极简 JSON 字段提取：找 "key" 之后第一个冒号，再取引号里的字符串。
/// 不够健壮，但只为从 Blinker 的心跳请求里抠出一个 id，够用了。
fn json_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(p) = s[from..].find(key) {
        let after = from + p + key.len();
        // 注意：JSON 里键名是带引号的，所以 key 后面还跟着一个收尾的 `"`，
        // 不能直接 expect 冒号。就是漏了这一步，之前一直取不到 fromDevice。
        let rest = s[after..].trim_start();
        let rest = rest.strip_prefix('"').unwrap_or(rest);
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix(':') {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('"') {
                if let Some(e) = rest.find('"') {
                    return Some(&rest[..e]);
                }
            }
        }
        from = after;
    }
    None
}

/// 把 Blinker 消息解析成电源指令。**只做精确匹配，绝不 contains**。
///
/// 历史坑（2026-10-25）：原来这里是 `msg.contains("sleep")` 这种子串判断，
/// 后果是 MQTT 收到**任何**带 `sleep` 字样的报文（Blinker 的配置下发、属性回显、
/// 甚至设备名）都会让电脑立刻休眠 —— 表现就是"一插上电脑就黑屏"。
/// 更糟的是 `power` -> `PowerDown` 那条，而设备名 / 热点名本身就是 `ESP32-POWER`。
///
/// 现在只认三种形式：
///   ① 整条消息就是一个裸词：`sleep`
///   ② JSON **键名**（Blinker 按钮组件发的 `{"btn-sleep":"tap"}`）
///   ③ JSON **值**（`{"cmd":"sleep"}`）—— 但值里不认裸 `power`，
///      只认 `powerdown` / `shutdown`，避免 `{"fromDevice":"power"}` 这种误伤
///
/// 键名归一化：去掉 `btn-` / `btn_` 前缀，大小写不敏感。
fn parse_power_cmd(msg: &str) -> Option<PowerCmd> {
    // ① 裸词：整条消息就是一个词
    if let Some(cmd) = cmd_from_word(msg.trim(), true) {
        return Some(cmd);
    }

    // ② / ③ 扫 JSON 里的字符串 token，用"后面是否紧跟冒号"区分键名和值
    let bytes = msg.as_bytes();
    let mut i = 0;
    let mut last_key: Option<&str> = None;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b'"' {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let token = &msg[start..j];
        if msg[j + 1..].trim_start().starts_with(':') {
            // 键名。注意这里有个坑：Blinker **下发组件配置**时，键名同样长成
            // `{"btn-power":{...}}` 这个样子，和"按下按钮"的 `{"btn-power":"tap"}`
            // 只差一个值。只认键名的话，一配上 App 就会把电脑关掉。
            // 所以 btn-* 这种按钮键，必须值真的是 tap / press 才算按下。
            let after = msg[j + 1..].trim_start();
            let after = after[1..].trim_start(); // 跳过 ':'
            let is_button = token
                .get(..4)
                .is_some_and(|p| eq_ci(p, "btn-") || eq_ci(p, "btn_"));

            if !is_button || is_press_value(after) {
                if let Some(cmd) = cmd_from_word(token, true) {
                    return Some(cmd);
                }
            }
            last_key = Some(token);
        } else if last_key.is_some_and(is_cmd_field) {
            // 值：只有键名是公认的指令字段才认，
            // 否则 `{"mac":"sleep"}` 这种八竿子打不着的值也会把电脑弄睡
            if let Some(cmd) = cmd_from_word(token, false) {
                return Some(cmd);
            }
        }
        i = j + 1;
    }
    None
}

/// 哪些键名下的"值"可以当指令看（`{"cmd":"sleep"}` / `{"data":"sleep"}`）。
fn is_cmd_field(key: &str) -> bool {
    eq_ci(key, "cmd")
        || eq_ci(key, "command")
        || eq_ci(key, "action")
        || eq_ci(key, "data")
        || eq_ci(key, "power")
        || eq_ci(key, "button")
}

/// 值是不是"按了一下"。Blinker 按键组件被点时发 `{"btn-xxx":"tap"}`。
///
/// 只认 tap / press / click 这几种，其余（对象、数字、别的字符串）一律当配置看待。
fn is_press_value(after_colon: &str) -> bool {
    let Some(rest) = after_colon.strip_prefix('"') else {
        return false; // 值是对象 / 数组 / 数字 → 组件配置，不是按键事件
    };
    let Some(end) = rest.find('"') else {
        return false;
    };
    let v = &rest[..end];
    eq_ci(v, "tap") || eq_ci(v, "press") || eq_ci(v, "click")
}

/// 单个 token -> 指令。
///
/// `allow_power` 决定要不要把裸 `power` 当成关机：键名允许
/// （兼容 App 里已经配好的 `btn-power` 按钮），值不允许。
fn cmd_from_word(word: &str, allow_power: bool) -> Option<PowerCmd> {
    let w = word.trim();
    let w = strip_prefix_ci(w, "btn-")
        .or_else(|| strip_prefix_ci(w, "btn_"))
        .unwrap_or(w);

    if eq_ci(w, "wake") {
        return Some(PowerCmd::Wake);
    }
    if eq_ci(w, "sleep") {
        return Some(PowerCmd::Sleep);
    }
    if (allow_power && eq_ci(w, "power"))
        || eq_ci(w, "powerdown")
        || eq_ci(w, "power-off")
        || eq_ci(w, "poweroff")
        || eq_ci(w, "shutdown")
    {
        return Some(PowerCmd::PowerDown);
    }
    None
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    let head = s.get(..p.len())?; // get 而不是直接切片：非字符边界时返回 None 不 panic
    if eq_ci(head, p) {
        Some(&s[p.len()..])
    } else {
        None
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

#[embassy_executor::task]
async fn blinker_task(stack: Stack<'static>) {
    info!("blinker task started");
    // 冷启动保护的起点：上电后 MQTT_GUARD_MS 内收到的指令一律不执行
    let boot_guard = Instant::now();

    // 缓冲和 socket 只建一次，必须放在重连循环**外面**。
    //
    // 原来这几行写在循环里，而 `mk_static!` 是对 StaticCell 做 write、只能写一次，
    // 所以只要第一次没连上（或中途掉线），第二圈就会 panic；而 esp-backtrace 的
    // panic handler 是死循环 —— 整颗芯片停摆。表现就是：STA 明明连上了（路由器能看到），
    // 几秒后 STA 和 AP 一起消失，而且再也回不来。
    let rx = mk_static!([u8; 1024], [0u8; 1024]);
    let tx = mk_static!([u8; 512], [0u8; 512]);
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(30)));

    // 凭据：网页存过就用 flash 里的，否则用编译期常量
    let mut bcfg: store::BlinkerCreds = BLINKER_CFG.lock().await.clone();
    if !bcfg.is_set() {
        let _ = bcfg.device.push_str(BLINKER_DEVICE_NAME);
        let _ = bcfg.iot_id.push_str(BLINKER_IOT_ID);
        let _ = bcfg.token.push_str(BLINKER_IOT_TOKEN);
    }
    dlog!("[mqtt] 设备名: {}", bcfg.device);

    let mut topic: HString<96> = HString::new();
    write!(topic, "/device/{}/r", bcfg.device.as_str()).ok();

    // 设备上报 topic：心跳回复要发到这里
    let mut stopic: HString<96> = HString::new();
    write!(stopic, "/device/{}/s", bcfg.device.as_str()).ok();

    let mut payload = [0u8; 512];

    loop {
        stack.wait_config_up().await;
        info!("network up, resolving {BROKER_HOST}");

        let addrs = match stack.dns_query(BROKER_HOST, DnsQueryType::A).await {
            Ok(a) => {
                dlog!("[mqtt] DNS 解析到 {} 个地址", a.len());
                a
            }
            Err(e) => {
                dlog!("[mqtt] DNS 失败: {e:?}");
                warn!("DNS failed: {e:?}");
                Timer::after(Duration::from_secs(10)).await;
                continue;
            }
        };
        let Some(addr) = addrs.first().copied() else {
            dlog!("[mqtt] DNS 没返回地址");
            warn!("DNS returned no address");
            Timer::after(Duration::from_secs(10)).await;
            continue;
        };

        if let Err(e) = socket.connect((addr, BROKER_PORT)).await {
            dlog!("[mqtt] TCP 连接 broker 失败: {e:?}");
            warn!("TCP connect failed: {e:?}");
            socket.abort();
            let _ = socket.flush().await;
            Timer::after(Duration::from_secs(10)).await;
            continue;
        }
        dlog!("[mqtt] TCP 已连上 broker");
        info!("connected to {BROKER_HOST}");

        // 读超时必须明显大于心跳周期（30s）。
        // 原来这里是 30s，和心跳撞在一起：每次都是"读超时 → Err(Socket) → 断开重连"，
        // 表现就是 Blinker 上设备一直闪断、显示离线。
        socket.set_timeout(Some(Duration::from_secs(120)));

        // 用块把 client 对 socket 的可变借用限制住，出了块才能再操作 socket
        let mut failed = false;
        {
            // keepalive 给大一点（120s → broker 侧 180s 才判超时）。
            // 我们不主动发 PINGREQ，靠"回心跳"当活动；App 关掉时连接会静默超时，
            // 超时后自动重连（2 秒），不影响使用。
            let mut client = mqtt::MqttClient::new(&mut socket, 120);

            if let Err(e) = client
                .connect(bcfg.device.as_str(), bcfg.iot_id.as_str(), bcfg.token.as_str())
                .await
            {
                dlog!("[mqtt] MQTT CONNECT 失败: {e:?}");
                warn!("MQTT connect failed: {e:?}");
                failed = true;
            } else if let Err(e) = client.subscribe(&topic, 1).await {
                dlog!("[mqtt] SUBSCRIBE 失败: {e:?}");
                warn!("MQTT subscribe failed: {e:?}");
                failed = true;
            } else {
                dlog!("[mqtt] 已连上 Blinker 并订阅成功");
                BLINKER_ONLINE.store(true, Ordering::Relaxed);
                info!("subscribed, waiting for commands");
                // 注意：这里**不能**用 select(wait_publish, Timer) 来做"每 30 秒发 PINGREQ"。
                // 那样每 30 秒会取消一次正在进行的 read，一旦正好读到半个报文，MQTT 流就错位，
                // 接着就是断线重连 —— 表现就是 App 上设备反复在线/离线。
                //
                // 现在改成只等报文：因为收到心跳就会回一条 PUBLISH，那本身就是客户端活动，
                // broker 的 keepalive 计时会被自然重置，不需要额外的 PINGREQ。
                loop {
                    match client.wait_publish(&mut payload).await {
                        Ok(n) => {
                            let msg = core::str::from_utf8(&payload[..n]).unwrap_or("");
                            dlog!("[mqtt] recv: {msg}");
                            info!("recv: {msg}");

                            // ---- Blinker 的"在线"心跳 ----
                            //
                            // App 发来的消息形如
                            //   {"fromDevice":"a361c1b3...","data":{"get":"state"}}
                            // 设备必须回一条（带 toDevice 指回 App）：
                            //   {"toDevice":"a361c1b3...","data":{"state":"online","timer":"000","version":"0.1.0"}}
                            // 发到 /device/<deviceName>/s。不回，App 就一直显示"离线"。
                            //
                            // 这里对**任何**带 fromDevice 的消息都回一次，不只是心跳请求，
                            // 这样按完按钮在线状态也会顺带刷新。
                            let is_heartbeat = msg.contains("get") && msg.contains("state");
                            let mut out: HString<192> = HString::new();
                            match json_str(msg, "fromDevice") {
                                Some(app) => {
                                    let _ = write!(
                                        out,
                                        "{{\"toDevice\":\"{app}\",\"data\":{{\"state\":\"online\",\"timer\":\"000\",\"version\":\"0.1.0\"}}}}"
                                    );
                                }
                                None if is_heartbeat => {
                                    let _ = write!(
                                        out,
                                        "{{\"data\":{{\"state\":\"online\",\"timer\":\"000\",\"version\":\"0.1.0\"}}}}"
                                    );
                                }
                                None => {}
                            }
                            if !out.is_empty() {
                                if client.publish(&stopic, out.as_bytes()).await.is_ok() {
                                    dlog!("[mqtt] 已回心跳: {out}");
                                } else {
                                    dlog!("[mqtt] 心跳回复发送失败");
                                }
                            }

                            // 严格匹配：只认精确的键名 / 值，不再做子串包含。
                            // 见 parse_power_cmd() 的注释 —— 原来的 contains 判断
                            // 会让任意带 sleep / power 字样的报文把电脑弄睡或关机。
                            if let Some(cmd) = parse_power_cmd(msg) {
                                if !MQTT_POWER_ENABLE {
                                    // 总闸关着：只记日志，绝不执行
                                    dlog!("[mqtt] 电源控制已关闭，忽略 {cmd:?}（报文: {msg}）");
                                    warn!("mqtt power disabled, ignore {cmd:?}");
                                } else if (Instant::now() - boot_guard).as_millis() < MQTT_GUARD_MS {
                                    // 冷启动保护：上电头几秒 MQTT 来的指令一律不执行
                                    dlog!("[mqtt] 冷启动保护期内，丢弃指令 {cmd:?}");
                                    warn!("cold-start guard: drop {cmd:?}");
                                } else {
                                    info!("-> {cmd:?}");
                                    dlog!("[mqtt] 执行指令 {cmd:?}");
                                    CMD_CHAN.send(cmd).await;
                                }
                            }
                        }
                        Err(e) => {
                            dlog!("[mqtt] 连接错误: {e:?}");
                            BLINKER_ONLINE.store(false, Ordering::Relaxed);
                            warn!("MQTT error: {e:?}, reconnecting");
                            break;
                        }
                    }
                }
            }
        }

        // 立刻 RST 回到 Closed，下一轮 socket.connect 才不会报 InvalidState
        socket.abort();
        let _ = socket.flush().await;
        Timer::after(Duration::from_secs(if failed { 10 } else { 2 })).await;
    }
}

/// 把 `log` 的记录也塞进 `/log` 环形缓冲。
///
/// 为什么需要：这块板子的 USB-Serial-JTAG 一被 OTG 接管，串口日志就彻底没了，
/// 而 trouble（蓝牙协议栈）和 esp-radio 的关键错误都是走 `log` 的 error!/warn!，
/// 不接过来就完全看不见 —— 排查配对失败时就是这么卡住的。
///
/// 只收：target 以 trouble 开头的（蓝牙协议栈内部）、以及所有 Warn 及以上的。
/// 其余（esp-hal 的 debug 之类）丢掉，否则 48 行的环形缓冲瞬间被刷没了。
struct RingLogger;

impl log::Log for RingLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target().starts_with("trouble")
            || metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // 只留 target 的最后一段，省字符（一行只有 96 字节）
        let target = record.target().rsplit("::").next().unwrap_or("log");
        // 开机头几秒 USB-Serial-JTAG 还没被 OTG 抢走，串口能同步看到一份
        esp_println::println!("[{}/{}] {}", target, record.level().as_str(), record.args());
        push_log(format_args!(
            "[{}/{}] {}",
            target,
            record.level().as_str(),
            record.args()
        ));
    }

    fn flush(&self) {}
}

static RING_LOGGER: RingLogger = RingLogger;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // 注意：不要再调 esp_println::logger::init_logger_from_env()。
    // 它内部会 set_logger 占住全局位置，我们的环形 logger 就装不上了。
    let _ = log::set_logger(&RING_LOGGER);
    log::set_max_level(log::LevelFilter::Info);

    // 内部 DRAM 堆。这块是 .bss 里的静态数组，**加多了会直接链接失败**，
    // 而且链接脚本里主栈就是 RWDATA 剩下的空间 —— 堆每加 1KB，主栈少 1KB。
    // 现在能定到 128KB，是因为网页收发缓冲（32KB）已经挪到 PSRAM（见下面）。
    esp_alloc::heap_allocator!(size: 128 * 1024);

    let config = esp_hal::Config::default().with_cpu_clock(CPU_CLOCK);
    let peripherals = esp_hal::init(config);

    // PSRAM（板载 8MB OPI）也挂进全局堆，但只带 External 能力位。
    // esp-radio 的 Wi-Fi / 蓝牙分配走 `malloc_internal`（限内部 RAM），
    // 所以永远拿不到 PSRAM —— 这正是我们要的：**PSRAM 只装普通数据**。
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);
    info!(
        "[power] CPU {:?} / Wi-Fi PS={:?} / TX {}dBm / BLE adv {}ms",
        CPU_CLOCK,
        WIFI_PS,
        (WIFI_TX_POWER as i32 / 4).min(20),
        BLE_ADV_INTERVAL_MS
    );
    info!(
        "[psram] 就绪，PSRAM 可用 {} B，内部堆 {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::External.into()),
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into()),
    );

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    // esp-hal 1.2：SoftwareInterruptControl 被拆成了单个 SoftwareInterrupt，
    // esp-rtos 0.4 的 start() 直接要那个 FROM_CPU_INTR0 外设。
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    println!("\n=== ESP32-S3 USB HID power key ===");

    // ---------------- USB HID ----------------
    // esp-hal 1.2：otg_fs 模块改名为 usb::otg，外设 USB0 改名为 USB_FS，
    // 且 Usb::new 拆成了 new_fs / new_hs（S3 只有全速口，用 new_fs）。
    let usb = Usb::new_fs(peripherals.USB_FS, peripherals.GPIO20, peripherals.GPIO19);
    let ep_out_buffer = mk_static!([u8; 1024], [0u8; 1024]);
    let driver = Driver::new(usb, ep_out_buffer, UsbOtgConfig::default());

    let mut usb_config = UsbConfig::new(0x303A, 0x8001);
    usb_config.manufacturer = Some("dosgo");
    usb_config.product = Some("ESP32-S3 Power Key");
    usb_config.serial_number = Some("0001");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;
    // 必须声明支持远程唤醒，否则配置描述符的 bmAttributes 不带 remote-wakeup 位，
    // Windows 设备管理器里连"允许此设备唤醒计算机"这个勾都不会给，
    // 电脑一睡总线就挂起，HID 报文永远发不出去。
    usb_config.supports_remote_wakeup = true;

    let config_desc = mk_static!([u8; 256], [0u8; 256]);
    let bos_desc = mk_static!([u8; 256], [0u8; 256]);
    let msos_desc = mk_static!([u8; 256], [0u8; 256]);
    let control_buf = mk_static!([u8; 128], [0u8; 128]);
    let handler = mk_static!(MyDeviceHandler, MyDeviceHandler::new());

    let mut builder = Builder::new(
        driver,
        usb_config,
        config_desc,
        bos_desc,
        msos_desc,
        control_buf,
    );
    builder.handler(handler);

    let hid_state = mk_static!(HidState, HidState::new());
    let hid_config = HidConfig {
        report_descriptor: HID_REPORT_DESC,
        request_handler: None,
        poll_ms: 10,
        max_packet_size: 64,
        hid_subclass: HidSubclass::No,
        hid_boot_protocol: HidBootProtocol::None,
    };
    let writer = HidWriter::<Driver<'static>, 9>::new(&mut builder, hid_state, hid_config);

    // ② 鼠标：boot protocol，4 字节（网页上的触控区）
    let mouse_state = mk_static!(HidState, HidState::new());
    let mouse = HidWriter::<Driver<'static>, 4>::new(
        &mut builder,
        mouse_state,
        HidConfig {
            report_descriptor: hid::MOUSE_DESC,
            request_handler: None,
            // 鼠标要跟手，轮询间隔压到 1ms（键盘 10ms 足够）
            poll_ms: 1,
            max_packet_size: 8,
            hid_subclass: HidSubclass::Boot,
            hid_boot_protocol: HidBootProtocol::Mouse,
        },
    );

    // ③ Consumer Control：音量 / 播放暂停等媒体键（2 字节，usage 16 位）
    let cc_state = mk_static!(HidState, HidState::new());
    let cc = HidWriter::<Driver<'static>, 2>::new(
        &mut builder,
        cc_state,
        HidConfig {
            report_descriptor: hid::CONSUMER_DESC,
            request_handler: None,
            poll_ms: 10,
            max_packet_size: 8,
            // 不是 boot device，用 No
            hid_subclass: HidSubclass::No,
            hid_boot_protocol: HidBootProtocol::None,
        },
    );

    let device = builder.build();

    spawner
        .spawn(usb_task(device).expect("failed to spawn usb task"));

    let button = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );
    if DIAG_NO_HID_OUTPUT {
        // 第二轮诊断：BOOT 键**重新启用**，但 HID 报告仍然全部抑制（见 send()）。
        // 这样只有 USB_WAKE.signal -> remote_wakeup / remote_wakeup_hw 这条裸寄存器
        // 路径是活的，用来验证它是不是让电脑进入 "Button or Lid" 睡眠的元凶。
        warn!("[diag] BOOT 键已启用（HID 报告仍抑制）");
        spawner
            .spawn(button_task(button).expect("failed to spawn button task"));
    } else {
        spawner
            .spawn(button_task(button).expect("failed to spawn button task"));
    }

    // 关键：这里必须**真的让出足够时间**，不能只 yield 一下就继续。
    //
    // 原因：`builder.build()` 一返回，OTG 的 D+ 上拉就已经挂上去了，Windows 会立刻
    // 开始枚举；而设备描述符这些控制传输是在**被轮询的控制任务**里回复的
    // （esp-hal 的中断只把 SETUP 收下来，数据阶段要靠 poll）。
    // 后面紧接着的 flash 读 + WiFi 射频校准是同步阻塞 1~2 秒，期间执行器回不到
    // usb_task，Windows 拿不到描述符就报"设备描述符请求失败"。
    // 之前用 yield_now() 只是运气好才枚举成功。
    Timer::after(Duration::from_millis(800)).await;

    // ---------------- 读上次存的 WiFi 账号密码 ----------------
    let mut flash = FlashStorage::new(peripherals.FLASH).multicore_auto_park();
    // 热点自己的名字/密码（网页里改过的）
    let apcfg = store::load_ap(&mut flash).unwrap_or_default();
    let creds = match store::load(&mut flash) {
        Some(c) => {
            info!("[store] 已存 WiFi: {}", c.ssid);
            Some(c)
        }
        None => {
            info!("[store] 没配过 WiFi，先用热点 {} 配网", apcfg.ssid_or_default());
            None
        }
    };
    // Blinker 凭据也顺手读一下（网页改过就用网页的，没改过就用固件内置的）
    let bcred = store::load_blinker(&mut flash);
    FLASH.lock().await.replace(flash);
    *AP_CFG.lock().await = apcfg;
    match bcred {
        Some(b) => {
            info!("[store] 已存 Blinker 设备: {}", b.device);
            *BLINKER_CFG.lock().await = b;
        }
        None => info!("[store] 没配过 Blinker，用固件内置凭据"),
    }

    // ---------------- WiFi：始终 APSTA，热点永远不断 ----------------
    //
    // 坑1：StationConfig::default() 的 auth_method 是 Wpa2Personal，而第一次上电
    // 既没 SSID 也没密码，esp_wifi_set_config(WIFI_IF_STA) 会返回
    // ESP_ERR_WIFI_PASSWORD，wifi::new 直接 Err —— 原来这里 .unwrap() 就 panic，
    // 而 panic 发生在 main 第一次 await 之前，导致 usb_task 永远没被 poll，
    // 现象就是"COM3 消失、HID 不出现"。
    //
    // 坑2：模式从 AP 变 APSTA 会让射频 stop/start，热点瞬断，正在提交配网表单的
    // 手机连接被掐。所以没配网时也用 APSTA + 占位 STA，模式全程不变。
    let initial = {
        let ap = AP_CFG.lock().await.clone();
        match creds.as_ref() {
            Some(c) if c.is_set() => WifiConfig::AccessPointStation(sta_config(c), ap_config(&ap)),
            _ => {
                info!(
                    "[wifi] 还没配过网，APSTA + 占位 STA（热点 {}）",
                    ap.ssid_or_default()
                );
                WifiConfig::AccessPointStation(placeholder_sta(), ap_config(&ap))
            }
        }
    };

    // esp-radio 1.0：控制器和网络接口分开创建。
    // WifiController::new 只返回控制器，接口要用 Interface::station() / access_point() 单独拿。
    let mut controller = match esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(initial),
    ) {
        Ok(v) => v,
        Err(e) => {
            // WiFi 起不来也不能拖死 USB HID：HID / BOOT 键继续可用，只是没法远程控制
            error!("[wifi] 初始化失败: {e:?}（USB HID 仍可用，远程控制不可用）");
            loop {
                Timer::after(Duration::from_secs(60)).await;
            }
        }
    };
    let ap_iface = Interface::access_point();
    let sta_iface = Interface::station();

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let seed2 = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A;

    // socket 槽位要从宽：每个网现在并排跑 WEB_SESSIONS 个网页监听 socket，
    // 联网侧还要加 MQTT(TCP) + WOL(UDP) + DNS 查询(UDP)，
    // 热点侧加 DHCP(UDP) + 假 DNS(UDP)。
    // 不够的话 smoltcp 会直接 panic（adding a socket to a full SocketSet）。
    // 热点侧：ESP32 自己就是网关，地址固定
    let (ap_stack, ap_runner) = embassy_net::new(
        ap_iface,
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(store::AP_IP, 24),
            gateway: None,
            dns_servers: heapless::Vec::new(),
        }),
        mk_static!(StackResources<10>, StackResources::<10>::new()),
        seed,
    );

    // 联网侧：MQTT 要外网，所以这个才是真正干活的那条
    let sta_net_config = if USE_STATIC_IP {
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(STATIC_IP, 24),
            gateway: Some(GATEWAY_IP),
            dns_servers: heapless::Vec::new(),
        })
    } else {
        NetConfig::dhcpv4(Default::default())
    };
    let (stack, sta_runner) = embassy_net::new(
        sta_iface,
        sta_net_config,
        mk_static!(StackResources<10>, StackResources::<10>::new()),
        seed2,
    );

    spawner
        .spawn(net_task(ap_runner).expect("failed to spawn ap net task"));
    spawner
        .spawn(net_task(sta_runner).expect("failed to spawn sta net task"));
    // HID 任务要拿联网侧的 stack 发 WOL，所以挪到这里 spawn
    spawner
        .spawn(hid_task(writer, mouse, cc, stack).expect("failed to spawn hid task"));

    // LCD 放在这里 spawn：USB 枚举和 WiFi 初始化都已经过去了，
    // 屏初始化的几十毫秒阻塞不会影响枚举。
    //
    // 【临时停用】引脚还没确定（实测 4/5/3/2/1 点不亮），改用文件末尾的引脚扫描。
    // 找到正确引脚后把这段恢复（去掉注释）、删掉扫描循环，并改 lcd.rs 顶部的引脚。
    //
    // spawner.spawn(
    //     lcd::lcd_task(
    //         peripherals.SPI2,
    //         peripherals.GPIO5, // SCL
    //         peripherals.GPIO3, // SDA
    //         peripherals.GPIO4, // CS
    //         peripherals.GPIO2, // DC
    //         peripherals.GPIO1, // RST
    //     )
    //     .expect("failed to spawn lcd task"),
    //     );

    // 每个网起 WEB_SESSIONS 个网页会话任务（各自一个监听 socket + 一块 2KB×2 缓冲）。
    // 缓冲从 PSRAM 里一次性取 16KB（两个网共 32KB），内部 DRAM 一字节都不占。
    spawn_web_sessions(
        spawner,
        ap_stack,
        "AP",
        AP_WEB_BUF.init(alloc_psram_buf(WEB_BUF * WEB_SESSIONS * 2)),
    );
    spawn_web_sessions(
        spawner,
        stack,
        "STA",
        STA_WEB_BUF.init(alloc_psram_buf(WEB_BUF * WEB_SESSIONS * 2)),
    );

    // 热点侧必须自带 DHCP + 假 DNS，否则手机连上会"无互联网"然后断开
    spawner
        .spawn(dhcp_task(ap_stack).expect("failed to spawn dhcp task"));
    spawner
        .spawn(dns_task(ap_stack).expect("failed to spawn dns task"));

    // ---- 省电：射频省电模式 + 发射功率 ----
    //
    // 必须在 wifi_task 拿走 controller 之前设。两项都是 esp-radio 的 unstable API。
    // 失败只记日志：省电没设上也照样能用，只是费电。
    match controller.set_power_saving(WIFI_PS) {
        Ok(()) => dlog!("[power] Wi-Fi 省电模式 = {:?}", WIFI_PS),
        Err(e) => warn!("[power] 设置省电模式失败: {e:?}"),
    }
    match controller.set_max_tx_power(WIFI_TX_POWER) {
        // 原始值是 0.25dBm 单位，但 ESP 的实际上限是 20dBm（原始值 84 会按 20 处理），
        // 所以这里钳一下，别打印出 21dBm 这种不存在的档位
        Ok(()) => dlog!(
            "[power] Wi-Fi 发射功率 = {}dBm（原始值 {}）",
            (WIFI_TX_POWER as i32 / 4).min(20),
            WIFI_TX_POWER
        ),
        Err(e) => warn!("[power] 设置发射功率失败: {e:?}"),
    }

    spawner
        .spawn(wifi_task(controller, creds.clone()).expect("failed to spawn wifi task"));
    spawner
        .spawn(status_task(stack).expect("failed to spawn status task"));
    spawner
        .spawn(blinker_task(stack).expect("failed to spawn blinker task"));

    // BLE 可行性验证：Wi-Fi 起来之后再初始化蓝牙控制器（S3 单射频，靠 coex 共存）。
    // 这一版只广播 + 接受连接，还没有 HID 服务；失败也不影响 USB/Wi-Fi。
    spawner
        .spawn(ble::ble_task(peripherals.BT).expect("failed to spawn ble task"));

    println!("USB HID ready. Plug USB(GPIO19/20) into the PC.");
    {
        let ap = AP_CFG.lock().await;
        println!(
            "热点已开: {} / {}  -> 配网请打开 http://{}",
            ap.ssid_or_default(),
            if ap.pass_or_default().is_empty() {
                "（开放网络）"
            } else {
                ap.pass_or_default()
            },
            store::AP_IP
        );
    }
    println!("BOOT: 短按唤醒 / 长按 {BOOT_RESET_MS}ms 恢复出厂");

    // LCD：GNPE Pocket-Dongle-S3-0.96 的真实接线（官方原理图）
    //   文档：github.com/beastboost/jcorp-nomad/blob/main/docs/PocketDongleS3.md
    //   SCL=10  SDA=11  CS=12  DC=13  RST=14，背光硬连常亮（无 GPIO）
    //   注意：这块板**不是** LilyGO T-Dongle-S3 克隆，接线完全不同。
    spawner
        .spawn(
            lcd::lcd_task(
                peripherals.SPI2,
                peripherals.GPIO10, // SCLK
                peripherals.GPIO11, // MOSI / SDA
                peripherals.GPIO12, // CS
                peripherals.GPIO13, // DC
                peripherals.GPIO14, // RST
            )
            .expect("failed to spawn lcd task"),
        );

    loop {
        Timer::after(Duration::from_secs(30)).await;
        match stack.config_v4() {
            Some(cfg) => info!("STA IP = {}", cfg.address),
            None => info!("STA 未连接（热点 {} 一直可用）", store::AP_IP),
        }
    }
}

/// STA 连接管理：开机自动连上次存的 WiFi；网页改了配置就立刻重连。
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>, creds: Option<store::Creds>) {
    let mut cur: Option<store::Creds> = creds;
    let mut connected = false;

    if let Some(c) = &cur {
        connected = connect_sta(&mut controller, c).await;
    } else {
        NET.lock().await.state = 0;
    }

    loop {
        // 没配过 → 什么都不做，只等网页提交配置（热点一直开着）
        if cur.is_none() {
            let c = NEW_CREDS.wait().await;
            info!("[wifi] 收到新配置: {}", c.ssid);
            cur = Some(c);
            connected = false;
        }

        if !connected {
            let c = cur.clone().unwrap_or_default();
            connected = connect_sta(&mut controller, &c).await;

            if !connected {
                // 连接失败：等一会儿重试，期间用户可以改配置
                match select(
                    NEW_CREDS.wait(),
                    Timer::after(Duration::from_secs(STA_RETRY_INTERVAL)),
                )
                .await
                {
                    Either::First(c) => {
                        info!("[wifi] 收到新配置: {}", c.ssid);
                        cur = Some(c);
                        connected = false;
                    }
                    Either::Second(_) => {}
                }
            }
            continue;
        }

        // 已连上：等断线，或者用户提交了新的账号密码
        // 已连上：等断线、等新配置，顺便每 5 秒采一次 RSSI 给 /status 用
        match select3(
            NEW_CREDS.wait(),
            controller.wait_for_disconnect_async(),
            Timer::after(Duration::from_secs(5)),
        )
        .await
        {
            Either3::First(c) => {
                info!("[wifi] 收到新配置: {}", c.ssid);
                cur = Some(c);
                connected = false;
            }
            Either3::Second(_) => {
                warn!("[wifi] STA 断线，准备重连");
                NET.lock().await.state = 0;
                connected = false;
            }
            Either3::Third(_) => {
                // 信号强度：只能给用户看，顺便帮判断发射功率该不该加大
                if let Ok(r) = controller.rssi() {
                    NET.lock().await.rssi = r as i8;
                }
            }
        }
    }
}

/// 连一次 STA。返回是否连上。
/// 注意：切配置必须带上 AP，否则 `set_config` 会把热点关掉，人就再也连不回来了。
async fn connect_sta(controller: &mut WifiController<'static>, c: &store::Creds) -> bool {
    if !c.is_set() {
        NET.lock().await.state = 0;
        return false;
    }

    let cfg = WifiConfig::AccessPointStation(sta_config(c), {
        // 热点参数随时可能被网页改过，每次切配置都从内存里取最新的
        let ap = AP_CFG.lock().await.clone();
        ap_config(&ap)
    });

    if let Err(e) = controller.set_config(&cfg) {
        error!("[wifi] 设置 AP+STA 失败: {e:?}");
        dlog!("[wifi] set_config 失败: {e:?}");
        let mut n = NET.lock().await;
        n.state = 3;
        n.err.clear();
        let _ = core::fmt::Write::write_fmt(&mut n.err, format_args!("set_config: {e:?}"));
        return false;
    }

    info!("[wifi] 连接 {}（最多等 {STA_CONNECT_TIMEOUT}s）", c.ssid);
    dlog!("[wifi] 连接 {} …", c.ssid);
    {
        let mut n = NET.lock().await;
        n.state = 1;
        n.err.clear();
    }

    match with_timeout(Duration::from_secs(STA_CONNECT_TIMEOUT), controller.connect_async()).await
    {
        Ok(Ok(_)) => {
            info!("[wifi] 已连上 {}", c.ssid);
            dlog!("[wifi] 已连上 {}", c.ssid);
            NET.lock().await.state = 2;
            true
        }
        Ok(Err(e)) => {
            warn!("[wifi] 连接失败: {e:?}");
            // 这句会显示在配网页上，是排查"连不上"的关键线索
            dlog!("[wifi] 连接失败: {e:?}");
            let mut n = NET.lock().await;
            n.state = 3;
            n.err.clear();
            let _ = core::fmt::Write::write_fmt(&mut n.err, format_args!("{e:?}"));
            false
        }
        Err(_) => {
            warn!("[wifi] 连接超时（{STA_CONNECT_TIMEOUT}s）");
            dlog!("[wifi] 连接超时");
            let mut n = NET.lock().await;
            n.state = 3;
            n.err.clear();
            let _ = core::fmt::Write::write_fmt(
                &mut n.err,
                format_args!("超时 {STA_CONNECT_TIMEOUT}s（密码错 / 只支持 5G / 信号弱）"),
            );
            false
        }
    }
}

/// 把 STA 拿到的 IP 刷进 NET（配网页的 /status 读它）
#[embassy_executor::task]
async fn status_task(stack: Stack<'static>) {
    loop {
        {
            let mut n = NET.lock().await;
            n.ip = stack.config_v4().map(|c| c.address.address().octets());
        }
        Timer::after(Duration::from_millis(1000)).await;
    }
}

/// 热点的 DHCP 服务：没有它，手机连上会一直卡在"正在获取 IP 地址"
#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    static GATEWAYS: [DhcpIp; 1] = [DhcpIp::new(192, 168, 4, 1)];
    static DNS: [DhcpIp; 1] = [DhcpIp::new(192, 168, 4, 1)];

    let config = DhcpServerConfig {
        ip: DhcpIp::new(192, 168, 4, 1),
        lease_time: Duration::from_secs(3600),
        gateways: &GATEWAYS,
        subnet: Some(DhcpIp::new(255, 255, 255, 0)),
        dns: &DNS,
        // 让安卓弹"登录到此网络"的通知
        use_captive_portal: true,
    };
    let mut leaser = SimpleDhcpLeaser {
        start: DhcpIp::new(192, 168, 4, 2),
        end: DhcpIp::new(192, 168, 4, 10),
        leases: heapless::Vec::new(),
    };

    info!("[dhcp] 启动：分配 192.168.4.2 ~ 192.168.4.10");
    if let Err(e) = run_dhcp_server(stack, config, &mut leaser).await {
        error!("[dhcp] 退出: {e:?}");
    }
}

/// 把所有域名解析到自己，配合手机的俘虏门户探测
#[embassy_executor::task]
async fn dns_task(stack: Stack<'static>) {
    dns::run(stack, store::AP_IP).await;
}

/// 每个网同时能接几条网页连接。
///
/// 以前是 1 条：一个 stack 只有一个 TcpSocket，WebSocket 升级后它就一直占着，
/// 第二个人打开 /kvm 直接连不上（新 SYN 没有 listen 状态的 socket 可接，回 RST），
/// 已经打开的页面也会一直"3 秒后重连"。
/// 现在每个网并排跑 `WEB_SESSIONS` 个监听 socket —— smoltcp 允许多个 socket 同时
/// listen 同一个端口，新连接自然落到空闲的那个上，不需要额外的任务分发器。
const WEB_SESSIONS: usize = 4;
/// 单条连接的收发缓冲。WS 帧最大 42 字节、HTTP 请求头几百字节，2KB 足够；
/// 24KB 的页面靠 write_all 分片发，局域网里感觉不到差别。
const WEB_BUF: usize = 2048;

/// 每个网要 `WEB_SESSIONS × 2` 块缓冲，两个网共 32KB。
/// 这 32KB 现在从 **PSRAM** 里出：smoltcp 只是拿它当字节数组读写，
/// 不碰 DMA、不放原子量，所以放外部 RAM 完全没问题（省下来的内部 DRAM
/// 全部让给蓝牙控制器和主栈）。
static AP_WEB_BUF: static_cell::StaticCell<&'static mut [u8]> = static_cell::StaticCell::new();
static STA_WEB_BUF: static_cell::StaticCell<&'static mut [u8]> = static_cell::StaticCell::new();

/// 从 PSRAM 分配一块普通数据缓冲，PSRAM 不可用时退回内部堆。
fn alloc_psram_buf(len: usize) -> &'static mut [u8] {
    unsafe {
        let layout = core::alloc::Layout::from_size_align_unchecked(len, 4);
        let mut p =
            esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout);
        if p.is_null() {
            dlog!("[psram] 分配 {}B 失败，退回内部堆", len);
            p = esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::Internal.into(), layout);
        }
        assert!(!p.is_null(), "网页缓冲分配失败");
        let s = core::slice::from_raw_parts_mut(p, len);
        s.fill(0);
        s
    }
}

/// 当前活着的 WebSocket 条数（两套栈共享）。
///
/// 只有最后一条断开时才 `release_all()`：以前不管几条，谁断就把键盘鼠标全清零，
/// 多人同时用时会把别人正按住的键一起松掉（拖拽直接断）。
static WS_CLIENTS: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

#[embassy_executor::task(pool_size = WEB_SESSIONS * 2)]
async fn web_task(
    stack: Stack<'static>,
    tag: &'static str,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
) {
    web_server(stack, tag, rx, tx).await;
}

/// 把一块缓冲切成 2×`WEB_SESSIONS` 份（前一半 rx、后一半 tx），然后起 `WEB_SESSIONS` 个会话任务。
///
/// 注意 buffer 必须由调用方传进来：`mk_static!` 展开成函数内的 static，
/// 这个函数会被 AP / STA 各调一次，函数内 static 只有一份，第二次写必 panic。
fn spawn_web_sessions(
    spawner: Spawner,
    stack: Stack<'static>,
    tag: &'static str,
    buf: &'static mut [u8],
) {
    debug_assert_eq!(buf.len(), WEB_BUF * WEB_SESSIONS * 2);
    // 一遍 split_at_mut 把缓冲切成 2×WEB_SESSIONS 块。
    // 不能用 `&mut rx[i*WEB_BUF..]`：那样每轮都重新借一次 `*rx`，
    // 而 spawn 要求 'static，借用检查器会拦（E0499）。
    let (mut rest_rx, mut rest_tx) = buf.split_at_mut(WEB_BUF * WEB_SESSIONS);
    for _ in 0..WEB_SESSIONS {
        let (r, r2) = rest_rx.split_at_mut(WEB_BUF);
        let (t, t2) = rest_tx.split_at_mut(WEB_BUF);
        spawner
            .spawn(web_task(stack, tag, r, t).expect("failed to spawn web session"));
        rest_rx = r2;
        rest_tx = t2;
    }
}

/// 配网 HTTP 服务。热点侧和局域网侧各跑一份。
///
/// 缓冲从外面传进来（而不是在函数里 mk_static）：这个函数会被两个任务各调一次，
/// 函数内的 static 只有一份，第二次写 StaticCell 会 panic。
async fn web_server(
    stack: Stack<'static>,
    tag: &'static str,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
) {
    let mut socket = TcpSocket::new(stack, rx, tx);

    info!("[web/{tag}] 监听 {}:{}", "0.0.0.0", web::HTTP_PORT);

    loop {
        // 空等新连接可以等很久（这里 120 秒只是让日志偶尔刷一行）
        socket.set_timeout(Some(Duration::from_secs(120)));
        if let Err(e) = socket.accept(web::HTTP_PORT).await {
            warn!("[web/{tag}] accept 失败: {e:?}");
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }

        // 收请求头只给 10 秒。
        //
        // 原因：手机切了 WiFi / 息屏 / 标签页被冻结时，对端不会发 FIN，
        // 这条连接会一直占着一个会话槽。槽位现在有 WEB_SESSIONS 个，被占一个不至于
        // 全瘫，但也没必要白等 —— 10 秒足够一台手机把请求头发完。
        socket.set_timeout(Some(Duration::from_secs(10)));

        match web::serve_once(&mut socket).await {
            web::Served::WebSocket => {
                let alive = WS_CLIENTS.fetch_add(1, Ordering::Relaxed) + 1;
                dlog!("[web/{tag}] WS 已升级（在线 {alive} 条）");
                // 升级成功后换成长超时：页面每 20 秒发一次 `_p` 心跳，
                // 读超时太小会把正常空闲的连接误杀（页面又会闪断重连）。
                socket.set_timeout(Some(Duration::from_secs(120)));
                let _ = web::ws_text(&mut socket, "_ok").await;

                // 这条连接一直读，直到客户端关闭 / 超时。别的会话槽照常在 accept，
                // 所以同时开几个页面都行（同一台设备同时操作会互相覆盖，见 README）。
                let mut buf = [0u8; 256];
                loop {
                    match web::ws_read(&mut socket, &mut buf).await {
                        Ok((web::OP_TEXT, n)) => dispatch(&buf[..n]),
                        Ok((web::OP_PING, _)) => {
                            let _ = web::ws_pong(&mut socket).await;
                        }
                        Ok((web::OP_CLOSE, _)) => {
                            dlog!("[web/{tag}] 客户端主动关闭");
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }

                // 只在最后一个客户端离开时清空键鼠。
                // 多人同时用时，谁断都 release_all() 会把别人按住的键一起松掉。
                let left = WS_CLIENTS.fetch_sub(1, Ordering::Relaxed) - 1;
                if left == 0 {
                    dlog!("[web/{tag}] WS 断开（已无客户端），释放所有键");
                    release_all();
                } else {
                    dlog!("[web/{tag}] WS 断开，还有 {left} 条在线");
                }
            }
            web::Served::Page => {}
        }

        socket.close();
        let _ = socket.flush().await;
        socket.abort();
    }
}

// 坑：这个任务要被 spawn 两次（AP 一个、STA 一个），而 embassy-executor 的
// #[task] 默认 pool_size = 1，第二个 spawn 会返回 SpawnError::Busy，
// .expect() 直接 panic。必须显式开 2 个槽位。
#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

// ==================== 网页键鼠：线协议解析 ====================
//
// 从 esp32s3-hid-kvm 的 dispatch() 移植，唯一区别是键盘只有一路 boot 6KRO
// （电源键原有的那个 interface），所以 `n`（NKRO bitmap）和 `k`（6KRO 数组）
// 两种帧都归一成 6KRO 再发。
//
//   n <40位hex>                   NKRO 键盘报告（20 字节，这里转成 6KRO）
//   k <mod> <k1..k6>              boot 6KRO 键盘报告（页面不发，留着方便手动调试）
//   m <btn> <dx> <dy> <wheel>     鼠标（相对位移）
//   c <usage>                     媒体键，usage=0 表示松开
//   p wake|sleep|power            电源指令（网页顶栏那两个按钮）
//   r                             释放所有键
//   _p                            心跳（20 秒一次，只为保活）
//
// 没有 BIOS 模式开关：这个键盘本来就是 boot 6KRO（6 键上限，BIOS 直接认），
// 所以 `n` 帧在固件里转成 6KRO 就完事，不需要再让用户切来切去。

fn dispatch(cmd: &[u8]) {
    let text = match str::from_utf8(cmd) {
        Ok(s) => s.trim(),
        Err(_) => return,
    };
    let mut it = text.split(' ');

    match it.next() {
        Some("n") => {
            let mut rep = [0u8; 20];
            if decode_hex(it.next().unwrap_or(""), &mut rep) != 20 {
                return;
            }
            push_hid(HidEvent::Kbd(nkro_to_boot(&rep)));
        }
        Some("k") => {
            let mut rep = [0u8; 8];
            rep[0] = parse_u(it.next()) as u8;
            for i in 0..6 {
                rep[2 + i] = parse_u(it.next()) as u8;
            }
            push_hid(HidEvent::Kbd(rep));
        }
        Some("m") => {
            let btn = parse_u(it.next()) as u8;
            let dx = parse_i(it.next());
            let dy = parse_i(it.next());
            let w = parse_i(it.next());
            push_hid(HidEvent::Mouse([btn, dx as u8, dy as u8, w as u8]));
        }
        Some("c") => {
            let u = parse_u(it.next());
            push_hid(HidEvent::Cc([(u & 0xFF) as u8, ((u >> 8) & 0xFF) as u8]));
        }
        // 网页上的「唤醒 / 睡眠」走电源那条老链路，远程唤醒的门禁兜底一起用
        Some("p") => {
            let c = match it.next().unwrap_or("") {
                "wake" => PowerCmd::Wake,
                "sleep" => PowerCmd::Sleep,
                _ => PowerCmd::PowerDown,
            };
            dlog!("[web] 请求 {c:?}");
            if CMD_CHAN.try_send(c).is_err() {
                dlog!("[web] 电源指令队列满");
            }
        }
        Some("r") => release_all(),
        Some("_p") => {}
        _ => {}
    }
}

/// NKRO 20 字节报告（1 修饰键 + 152 位 bitmap）→ boot 6KRO 8 字节报告。
///
/// bitmap 是无序的，这里按 usage 从小到大取前 6 个 —— 超过 6 键同按会丢键，
/// 这是 boot protocol 的硬上限，日常使用基本碰不到。
fn nkro_to_boot(n: &[u8; 20]) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0] = n[0];
    let mut slot = 2usize;

    for byte in 1..20 {
        let bits = n[byte];
        if bits == 0 {
            continue;
        }
        for bit in 0..8 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            let usage = ((byte - 1) * 8 + bit) as u8;
            if usage == 0 {
                continue;
            }
            if slot < 8 {
                out[slot] = usage;
                slot += 1;
            }
        }
    }
    out
}

/// 把 hex 串解进 out，返回解出的字节数
fn decode_hex(s: &str, out: &mut [u8]) -> usize {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut n = 0usize;
    while i + 1 < b.len() && n < out.len() {
        let hi = match web::hex_val(b[i]) {
            Some(v) => v,
            None => break,
        };
        let lo = match web::hex_val(b[i + 1]) {
            Some(v) => v,
            None => break,
        };
        out[n] = (hi << 4) | lo;
        n += 1;
        i += 2;
    }
    n
}

fn parse_u(s: Option<&str>) -> u32 {
    let s = s.unwrap_or("0").trim();
    let mut v = 0u32;
    for b in s.bytes() {
        if !b.is_ascii_digit() {
            break;
        }
        v = v * 10 + (b - b'0') as u32;
    }
    v
}

fn parse_i(s: Option<&str>) -> i16 {
    let s = s.unwrap_or("0").trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let v = parse_u(Some(s)) as i16;
    if neg {
        -v
    } else {
        v
    }
}
