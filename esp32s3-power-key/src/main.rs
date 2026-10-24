//! ESP32-S3 USB HID 电脑电源控制器 + Blinker 远程控制
//!
//! 把自己模拟成 USB 键盘 + 系统控制设备，插在电脑上。
//! 手机 Blinker App 点按钮 -> Blinker 云(MQTT) -> ESP32 -> USB HID -> 电脑睡眠/唤醒。
//!
//! 烧写：cargo run --release

#![no_std]
#![no_main]

mod mqtt;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_net::{
    Config as NetConfig, IpAddress, Ipv4Address, Runner, Stack, StackResources,
    dns::DnsQueryType,
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer, with_timeout};
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
use esp_println::println;
use esp_radio::wifi::{Config as WifiConfig, ControllerConfig, Interface, sta::StationConfig};
use heapless::String as HString;
use log::{info, warn};

esp_bootloader_esp_idf::esp_app_desc!();

// ===================== 需要你填写的配置 =====================
const WIFI_SSID: &str = "YOUR_WIFI_SSID";
const WIFI_PASSWORD: &str = "YOUR_WIFI_PASSWORD";

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

/// 本地 BOOT 键：按一下唤醒，方便没联网时验证 HID 是否生效
#[embassy_executor::task]
async fn button_task(mut button: Input<'static>) {
    info!("button task started, press BOOT(GPIO0) to wake the PC");
    loop {
        button.wait_for_low().await;
        info!("BOOT pressed -> wake");
        CMD_CHAN.send(PowerCmd::Wake).await;
        Timer::after(Duration::from_millis(400)).await;
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

    // ---------------- WiFi ----------------
    let station_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PASSWORD.into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .unwrap();

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        NetConfig::dhcpv4(Default::default()),
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner
        .spawn(wifi_task(controller).expect("failed to spawn wifi task"));
    spawner
        .spawn(net_task(runner).expect("failed to spawn net task"));
    spawner
        .spawn(blinker_task(stack).expect("failed to spawn blinker task"));

    println!("USB HID ready. Plug USB(GPIO19/20) into the PC.");
    println!("Press BOOT to send a wake signal.");

    loop {
        Timer::after(Duration::from_secs(30)).await;
        if let Some(cfg) = stack.config_v4() {
            info!("IP = {}", cfg.address);
        }
    }
}

type WifiController = esp_radio::wifi::WifiController<'static>;

#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController) {
    loop {
        info!("connecting to WiFi: {}", WIFI_SSID);
        match controller.connect_async().await {
            Ok(_) => {
                info!("WiFi connected");
                let _ = controller.wait_for_disconnect_async().await;
                info!("WiFi disconnected, retrying");
            }
            Err(e) => warn!("WiFi connect failed: {e:?}"),
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}
