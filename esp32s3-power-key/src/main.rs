//! ESP32-S3 USB HID 电脑电源控制器 + Blinker 远程控制
//!
//! 把自己模拟成 USB 键盘 + 系统控制设备，插在电脑上。
//! 手机 Blinker App 点按钮 -> Blinker 云(MQTT) -> ESP32 -> USB HID -> 电脑睡眠/唤醒。
//!
//! ## 网络：AP + STA 同时开，不用切模式
//!
//!   - **热点（AP）永远开着**：`ESP32-POWER` / 192.168.4.1，用来配网和救急
//!   - **联网（STA）**：配过就开机自动连；MQTT 需要外网，没连上就控制不了
//!   - **配网**：连热点 → 浏览器开 192.168.4.1 → 填 WiFi 账号密码 → 保存
//!   - **恢复出厂**：长按 BOOT 3 秒（短按是唤醒电脑）
//!
//! 烧写：cargo run --release

#![no_std]
#![no_main]

mod dns;
mod mqtt;
mod store;
mod web;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
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
    interrupt::software::SoftwareInterruptControl,
    otg_fs::{Usb, asynch::Driver},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_hal_dhcp_server::{
    Ipv4Addr as DhcpIp, run_dhcp_server, simple_leaser::SimpleDhcpLeaser,
    structs::DhcpServerConfig,
};
use esp_println::println;
use esp_radio::wifi::{
    AuthenticationMethod, Config as WifiConfig, ControllerConfig, Interface, WifiController,
    ap::AccessPointConfig, sta::StationConfig,
};
use esp_storage::FlashStorage;
use heapless::String as HString;
use log::{error, info, warn};

esp_bootloader_esp_idf::esp_app_desc!();

// ===================== 需要你填写的配置 =====================
//
// WiFi 账号密码**不用写在这里** —— 连热点在网页里配（见 src/store.rs 和 setup.html）。

/// Blinker 设备凭据：浏览器打开
/// https://iot.diandeng.tech/api/v1/user/device/diy/auth?authKey=你的authKey&protocol=mqtt
/// 从返回的 JSON 里取 deviceName / iotId / iotToken 三个值填到这里
const BLINKER_DEVICE_NAME: &str = "YOUR_DEVICE_NAME"; // deviceName，也是 MQTT clientId
const BLINKER_IOT_ID: &str = "YOUR_IOT_ID"; // iotId，MQTT username
const BLINKER_IOT_TOKEN: &str = "YOUR_IOT_TOKEN"; // iotToken，MQTT password

/// 电脑网卡 MAC，用于 WOL 魔术包唤醒（最可靠的唤醒方式）
/// 填全 0 则只走 USB HID 唤醒
const PC_MAC: [u8; 6] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
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

/// 网络状态，给配网页的 /status 用
pub struct NetInfo {
    /// 0=没配过 1=连接中 2=已连接 3=失败
    pub state: u8,
    /// STA 在局域网里拿到的地址
    pub ip: Option<[u8; 4]>,
}

pub static NET: Mutex<CriticalSectionRawMutex, NetInfo> = Mutex::new(NetInfo {
    state: 0,
    ip: None,
});

/// flash 句柄（保存配网要写）。`FlashStorage::new` 只能调一次，又不能 clone，
/// 所以丢进这个全局盒子给 web / 按键任务用。
pub static FLASH: Mutex<CriticalSectionRawMutex, Option<FlashStorage<'static>>> = Mutex::new(None);

/// 网页提交了新的 WiFi 账号密码 → 通知 wifi_task 去连
pub static NEW_CREDS: Signal<CriticalSectionRawMutex, store::Creds> = Signal::new();

/// 热点（AP）的固定配置。每次改 WiFi 模式都得带上它，否则热点会被关掉。
fn ap_config() -> AccessPointConfig {
    let base = AccessPointConfig::default()
        .with_ssid(store::AP_SSID)
        .with_max_connections(4);
    if store::AP_PASSWORD.is_empty() {
        base
    } else {
        base.with_password(store::AP_PASSWORD.into())
            .with_auth_method(AuthenticationMethod::Wpa2Personal)
    }
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
        let _ = enabled;
        info!("USB enabled={enabled}");
    }
    fn reset(&mut self) {
        self.configured.store(false, Ordering::Relaxed);
    }
    fn addressed(&mut self, addr: u8) {
        self.configured.store(false, Ordering::Relaxed);
        let _ = addr;
    }
    fn configured(&mut self, configured: bool) {
        self.configured.store(configured, Ordering::Relaxed);
        info!("USB configured={configured}");
    }
}

#[embassy_executor::task]
async fn usb_task(mut device: embassy_usb::UsbDevice<'static, Driver<'static>>) {
    device.run().await;
}

#[embassy_executor::task]
async fn hid_task(mut writer: HidWriter<'static, Driver<'static>, 9>) {
    info!("hid task started");
    loop {
        let cmd = CMD_CHAN.receive().await;
        info!("executing: {cmd:?}");

        match cmd {
            PowerCmd::Wake => {
                send(&mut writer, &[2, 0x04]).await; // System Wake Up
                send(&mut writer, &[2, 0x00]).await;
                Timer::after(Duration::from_millis(60)).await;
                send(&mut writer, &[1, 0x00, 0x00, 0x2C, 0, 0, 0, 0, 0]).await; // Space
                Timer::after(Duration::from_millis(40)).await;
                send(&mut writer, &[1, 0, 0, 0, 0, 0, 0, 0, 0]).await; // release
            }
            PowerCmd::Sleep => {
                send(&mut writer, &[2, 0x02]).await; // System Sleep
                Timer::after(Duration::from_millis(60)).await;
                send(&mut writer, &[2, 0x00]).await;
            }
            PowerCmd::PowerDown => {
                send(&mut writer, &[2, 0x01]).await; // System Power Down
                Timer::after(Duration::from_millis(60)).await;
                send(&mut writer, &[2, 0x00]).await;
            }
        }
    }
}

async fn send(writer: &mut HidWriter<'static, Driver<'static>, 9>, data: &[u8]) {
    if with_timeout(Duration::from_millis(300), writer.write(data))
        .await
        .is_err()
    {
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

/// WOL 魔术包：6 个 0xFF + MAC 重复 16 次，广播到 255.255.255.255:9
async fn send_wol(stack: Stack<'static>) {
    if PC_MAC == [0u8; 6] {
        return;
    }
    let mut packet = [0xFFu8; 102];
    for i in 0..16 {
        packet[6 + i * 6..12 + i * 6].copy_from_slice(&PC_MAC);
    }

    let rx_meta = mk_static!([PacketMetadata; 2], [PacketMetadata::EMPTY; 2]);
    let rx_buf = mk_static!([u8; 128], [0u8; 128]);
    let tx_meta = mk_static!([PacketMetadata; 2], [PacketMetadata::EMPTY; 2]);
    let tx_buf = mk_static!([u8; 256], [0u8; 256]);
    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);

    if socket.bind(0).is_err() {
        warn!("WOL: bind failed");
        return;
    }
    let bcast: IpAddress = IpAddress::Ipv4(Ipv4Address::new(255, 255, 255, 255));
    match socket.send_to(&packet, (bcast, 9)).await {
        Ok(()) => info!("WOL magic packet sent to {:02X?}", PC_MAC),
        Err(e) => warn!("WOL send failed: {e:?}"),
    }
    socket.close();
}

/// Blinker：连 MQTT，订阅设备 topic，把指令转成电源动作
#[embassy_executor::task]
async fn blinker_task(stack: Stack<'static>) {
    info!("blinker task started");
    loop {
        stack.wait_config_up().await;
        info!("network up, resolving {BROKER_HOST}");

        let addrs = match stack.dns_query(BROKER_HOST, DnsQueryType::A).await {
            Ok(a) => a,
            Err(e) => {
                warn!("DNS failed: {e:?}");
                Timer::after(Duration::from_secs(10)).await;
                continue;
            }
        };
        let Some(addr) = addrs.first().copied() else {
            warn!("DNS returned no address");
            Timer::after(Duration::from_secs(10)).await;
            continue;
        };

        let rx = mk_static!([u8; 1024], [0u8; 1024]);
        let tx = mk_static!([u8; 512], [0u8; 512]);
        let mut socket = TcpSocket::new(stack, rx, tx);
        socket.set_timeout(Some(Duration::from_secs(30)));

        if let Err(e) = socket.connect((addr, BROKER_PORT)).await {
            warn!("TCP connect failed: {e:?}");
            socket.close();
            Timer::after(Duration::from_secs(10)).await;
            continue;
        }
        info!("connected to {BROKER_HOST}");

        let mut client = mqtt::MqttClient::new(socket, 60);

        let mut topic: HString<96> = HString::new();
        write!(topic, "/device/{}/r", BLINKER_DEVICE_NAME).ok();

        if let Err(e) = client
            .connect(BLINKER_DEVICE_NAME, BLINKER_IOT_ID, BLINKER_IOT_TOKEN)
            .await
        {
            warn!("MQTT connect failed: {e:?}");
            client.close();
            Timer::after(Duration::from_secs(10)).await;
            continue;
        }
        info!("MQTT connected, subscribing {topic}");

        if let Err(e) = client.subscribe(&topic, 1).await {
            warn!("MQTT subscribe failed: {e:?}");
            client.close();
            Timer::after(Duration::from_secs(10)).await;
            continue;
        }
        info!("subscribed, waiting for commands");

        let mut payload = [0u8; 512];
        loop {
            match client.wait_publish(&mut payload).await {
                Ok(n) => {
                    let msg = core::str::from_utf8(&payload[..n]).unwrap_or("");
                    info!("recv: {msg}");
                    // 宽容匹配：消息里含 wake / sleep / power 关键字即可
                    let lower = msg.to_ascii_lowercase();
                    if lower.contains("wake") {
                        info!("-> WAKE");
                        send_wol(stack).await;
                        CMD_CHAN.send(PowerCmd::Wake).await;
                    } else if lower.contains("sleep") {
                        info!("-> SLEEP");
                        CMD_CHAN.send(PowerCmd::Sleep).await;
                    } else if lower.contains("power") {
                        info!("-> POWER DOWN");
                        CMD_CHAN.send(PowerCmd::PowerDown).await;
                    }
                }
                Err(e) => {
                    warn!("MQTT error: {e:?}, reconnecting");
                    break;
                }
            }
        }
        client.close();
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    esp_alloc::heap_allocator!(size: 100 * 1024);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    println!("\n=== ESP32-S3 USB HID power key ===");

    // ---------------- USB HID ----------------
    let usb = Usb::new(peripherals.USB0, peripherals.GPIO20, peripherals.GPIO19);
    let ep_out_buffer = mk_static!([u8; 1024], [0u8; 1024]);
    let driver = Driver::new(usb, ep_out_buffer, esp_hal::otg_fs::asynch::Config::default());

    let mut usb_config = UsbConfig::new(0x303A, 0x8001);
    usb_config.manufacturer = Some("dosgo");
    usb_config.product = Some("ESP32-S3 Power Key");
    usb_config.serial_number = Some("0001");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

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
    let device = builder.build();

    spawner
        .spawn(usb_task(device).expect("failed to spawn usb task"));
    spawner
        .spawn(hid_task(writer).expect("failed to spawn hid task"));

    let button = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );
    spawner
        .spawn(button_task(button).expect("failed to spawn button task"));

    // ---------------- 读上次存的 WiFi 账号密码 ----------------
    let mut flash = FlashStorage::new(peripherals.FLASH).multicore_auto_park();
    let creds = match store::load(&mut flash) {
        Some(c) => {
            info!("[store] 已存 WiFi: {}", c.ssid);
            Some(c)
        }
        None => {
            info!("[store] 没配过 WiFi，先用热点 {} 配网", store::AP_SSID);
            None
        }
    };
    FLASH.lock().await.replace(flash);

    // ---------------- WiFi：AP + STA 同时开 ----------------
    let initial = WifiConfig::AccessPointStation(
        StationConfig::default()
            .with_ssid(creds.as_ref().map(|c| c.ssid.as_str()).unwrap_or(""))
            .with_password(
                creds
                    .as_ref()
                    .map(|c| c.pass.as_str().into())
                    .unwrap_or_default(),
            ),
        ap_config(),
    );

    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(initial),
    )
    .unwrap();

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let seed2 = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A;

    // 热点侧：ESP32 自己就是网关，地址固定
    let (ap_stack, ap_runner) = embassy_net::new(
        interfaces.access_point,
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(store::AP_IP, 24),
            gateway: None,
            dns_servers: heapless::Vec::new(),
        }),
        mk_static!(StackResources<3>, StackResources::<3>::new()),
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
        interfaces.station,
        sta_net_config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed2,
    );

    spawner
        .spawn(net_task(ap_runner).expect("failed to spawn ap net task"));
    spawner
        .spawn(net_task(sta_runner).expect("failed to spawn sta net task"));
    spawner
        .spawn(ap_web_task(ap_stack).expect("failed to spawn ap web task"));
    spawner
        .spawn(sta_web_task(stack).expect("failed to spawn sta web task"));

    // 热点侧必须自带 DHCP + 假 DNS，否则手机连上会"无互联网"然后断开
    spawner
        .spawn(dhcp_task(ap_stack).expect("failed to spawn dhcp task"));
    spawner
        .spawn(dns_task(ap_stack).expect("failed to spawn dns task"));

    spawner
        .spawn(wifi_task(controller, creds.clone()).expect("failed to spawn wifi task"));
    spawner
        .spawn(status_task(stack).expect("failed to spawn status task"));
    spawner
        .spawn(blinker_task(stack).expect("failed to spawn blinker task"));

    println!("USB HID ready. Plug USB(GPIO19/20) into the PC.");
    println!(
        "热点已开: {} / {}  -> 配网请打开 http://{}",
        store::AP_SSID,
        store::AP_PASSWORD,
        store::AP_IP
    );
    println!("BOOT: 短按唤醒 / 长按 {BOOT_RESET_MS}ms 恢复出厂");

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
        match select(NEW_CREDS.wait(), controller.wait_for_disconnect_async()).await {
            Either::First(c) => {
                info!("[wifi] 收到新配置: {}", c.ssid);
                cur = Some(c);
                connected = false;
            }
            Either::Second(_) => {
                warn!("[wifi] STA 断线，准备重连");
                NET.lock().await.state = 0;
                connected = false;
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

    let cfg = WifiConfig::AccessPointStation(
        StationConfig::default()
            .with_ssid(c.ssid.as_str())
            .with_password(c.pass.as_str().into()),
        ap_config(),
    );

    if let Err(e) = controller.set_config(&cfg) {
        error!("[wifi] 设置 AP+STA 失败: {e:?}");
        NET.lock().await.state = 3;
        return false;
    }

    info!("[wifi] 连接 {}（最多等 {STA_CONNECT_TIMEOUT}s）", c.ssid);
    NET.lock().await.state = 1;

    match with_timeout(Duration::from_secs(STA_CONNECT_TIMEOUT), controller.connect_async()).await
    {
        Ok(Ok(_)) => {
            info!("[wifi] 已连上 {}", c.ssid);
            NET.lock().await.state = 2;
            true
        }
        Ok(Err(e)) => {
            warn!("[wifi] 连接失败: {e:?}");
            NET.lock().await.state = 3;
            false
        }
        Err(_) => {
            warn!("[wifi] 连接超时（{STA_CONNECT_TIMEOUT}s）");
            NET.lock().await.state = 3;
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

#[embassy_executor::task]
async fn ap_web_task(stack: Stack<'static>) {
    web_server(
        stack,
        "AP",
        mk_static!([u8; 3072], [0u8; 3072]),
        mk_static!([u8; 3072], [0u8; 3072]),
    )
    .await;
}

#[embassy_executor::task]
async fn sta_web_task(stack: Stack<'static>) {
    web_server(
        stack,
        "STA",
        mk_static!([u8; 3072], [0u8; 3072]),
        mk_static!([u8; 3072], [0u8; 3072]),
    )
    .await;
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
    socket.set_timeout(Some(Duration::from_secs(30)));

    info!("[web/{tag}] 监听 {}:{}", "0.0.0.0", web::HTTP_PORT);

    loop {
        if let Err(e) = socket.accept(web::HTTP_PORT).await {
            warn!("[web/{tag}] accept 失败: {e:?}");
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }

        web::serve_once(&mut socket).await;

        socket.close();
        let _ = socket.flush().await;
        socket.abort();
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}
