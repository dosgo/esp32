//! ESP32-S3 模拟 USB 键盘 + 鼠标，通过 Web 页面远程输入。
//!
//! 浏览器（键盘/鼠标状态都在这里维护） ──WebSocket──> ESP32 ──USB HID──> 电脑
//!
//! 关键设计：**ESP32 不做任何键鼠状态管理**。浏览器端已经维护好 6KRO 按键数组和
//! 修饰键位图，直接把完整的 8 字节键盘报告发过来，ESP32 只是转发。
//! 这样固件极简，也不会出现"按键卡住"的状态不一致问题。
//!
//! ## 网络：AP + STA 同时开，不用切模式
//!
//!   - **热点（AP）永远开着**：`ESP32-KVM` / 192.168.4.1，配网、救急都用它
//!   - **联网（STA）可选**：没配过就只走热点；配过开机自动连，连上后局域网里
//!     也能直接用它的 IP 打开控制页
//!   - **配网**：连热点 → 浏览器开 192.168.4.1 → 点"WiFi 设置" → 填账号密码 → 保存
//!   - **改 WiFi**：连热点重新配就行；或者长按 BOOT 3 秒清掉已存的配置

#![no_std]
#![no_main]

mod dns;
mod hid;
mod lcd;
mod netlog;
mod store;
mod web;

use core::str;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_net::{
    Config as NetConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
    StaticConfigV4, tcp::TcpSocket,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embassy_usb::{
    Builder, Handler, UsbDevice,
    class::hid::{
        Config as HidConfig, HidBootProtocol, HidSubclass, HidWriter, State as HidState,
    },
};
use esp_backtrace as _;
use esp_bootloader_esp_idf::esp_app_desc;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::otg_fs::asynch::{Config as UsbDrvConfig, Driver};
use esp_hal::otg_fs::Usb;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::system::software_reset;
use esp_hal_dhcp_server::{
    Ipv4Addr as DhcpIp, run_dhcp_server, simple_leaser::SimpleDhcpLeaser,
    structs::DhcpServerConfig,
};
use esp_radio::wifi::{
    AuthenticationMethod, Config as WifiConfig, ControllerConfig, Interface, WifiController,
    ap::AccessPointConfig, sta::StationConfig,
};
use esp_storage::FlashStorage;
use log::{error, info, warn};

esp_app_desc!();

// ===================== 需要你填写的配置 =====================

/// 想让 ESP32 在局域网里用固定地址就开这个（省得每次找 IP）。
/// 不开就是 DHCP，IP 得看屏幕 / 串口日志。
const USE_STATIC_IP: bool = false;
const STATIC_IP: Ipv4Address = Ipv4Address::new(192, 168, 1, 60);
const GATEWAY_IP: Ipv4Address = Ipv4Address::new(192, 168, 1, 1);

/// 接了 SPI TFT 屏幕就保持 true；没接（比如只有 USB 公头的 dongle）改 false，
/// 避免 SPI 初始化失败把整个固件拖住。
/// 屏幕 SPI 引脚在 src/lcd.rs 里：SCLK=12  MOSI=11  CS=10  DC=9  RST=8
const LCD_ENABLED: bool = true;

/// 热点名字 / 密码 / 地址在 src/store.rs（AP_SSID / AP_PASSWORD / AP_IP）。
/// ⚠️ 如果你家路由器网段正好是 192.168.4.x，把 AP_IP 改成别的（比如 192.168.71.1），
///    否则两个网段撞车，STA 那边会不通。

/// STA 单次连接最多等多久（秒）
const STA_CONNECT_TIMEOUT: u64 = 15;
/// 连不上时隔多久重试（秒）
const STA_RETRY_INTERVAL: u64 = 10;

/// 长按 BOOT 这么久 → 恢复出厂设置（清空整个配置区）并重启
const BOOT_RESET_MS: u64 = 3000;
// ===========================================================

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

struct UsbHandler {
    _priv: (),
}

/// USB 枚举状态。Handler 是同步上下文，用原子量避免在这里 await 锁。
static USB_READY: AtomicBool = AtomicBool::new(false);

/// BIOS 模式：为 true 时键盘走 8 字节 boot 报告（BIOS 只认这个）
static BIOS_MODE: AtomicBool = AtomicBool::new(false);

/// flash 句柄（保存配网要写）。esp-storage 的 `FlashStorage::new` 只能调一次，
/// 又不能随便 clone，所以丢进这个全局盒子给 web / 按键任务用。
static FLASH: Mutex<CriticalSectionRawMutex, Option<FlashStorage<'static>>> = Mutex::new(None);

/// 网页提交了新的 WiFi 账号密码 → 通知 wifi_task 去连
static NEW_CREDS: Signal<CriticalSectionRawMutex, store::Creds> = Signal::new();

// ---------------- HID 事件队列 ----------------
//
// 热点和局域网各有一个 web server，两边都能下发键鼠指令，
// 但 USB 的 HidWriter 只有一份、不能 clone，所以走队列：
// web 任务只管往里塞事件，hid_task 单独消费写 USB。

enum HidEvent {
    Nkro([u8; 20]),
    Boot([u8; 8]),
    Mouse([u8; 4]),
    Cc([u8; 2]),
}

static HID_Q: Channel<CriticalSectionRawMutex, HidEvent, 24> = Channel::new();

impl Handler for UsbHandler {
    fn enabled(&mut self, enabled: bool) {
        info!("USB enabled={enabled}");
    }
    fn reset(&mut self) {
        USB_READY.store(false, Ordering::Relaxed);
    }
    fn addressed(&mut self, _addr: u8) {}
    fn configured(&mut self, configured: bool) {
        info!("USB configured={configured}（电脑已识别设备）");
        USB_READY.store(configured, Ordering::Relaxed);
    }
}

/// 热点（AP）的固定配置。每次要改 WiFi 模式时都得带上它，否则热点会被关掉。
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

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    // device 模式下 GPIO19/20 归 OTG，USB-Serial-JTAG 失效；
    // 单口 dongle 没有 UART，所以日志走 UDP 广播（tools/../netlog_listen.py 收）
    netlog::init();

    esp_alloc::heap_allocator!(size: 100 * 1024);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("=== ESP32-S3 HID KVM ===");

    // ---------------- USB HID 设备 ----------------
    let usb = Usb::new(peripherals.USB0, peripherals.GPIO20, peripherals.GPIO19);
    let ep_out_buffer = mk_static!([u8; 1024], [0u8; 1024]);
    let driver = Driver::new(usb, ep_out_buffer, UsbDrvConfig::default());

    let mut usb_config = embassy_usb::Config::new(0x303A, 0x8002);
    usb_config.manufacturer = Some("dosgo");
    usb_config.product = Some("ESP32-S3 KVM");
    usb_config.serial_number = Some("0001");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    let config_desc = mk_static!([u8; 256], [0u8; 256]);
    let bos_desc = mk_static!([u8; 256], [0u8; 256]);
    let msos_desc = mk_static!([u8; 256], [0u8; 256]);
    let control_buf = mk_static!([u8; 128], [0u8; 128]);
    let handler = mk_static!(UsbHandler, UsbHandler { _priv: () });

    let mut builder = Builder::new(
        driver,
        usb_config,
        config_desc,
        bos_desc,
        msos_desc,
        control_buf,
    );
    builder.handler(handler);

    // ① NKRO 键盘：bitmap 报告，任意多键同按（日常走这个）
    let nkro_state = mk_static!(HidState, HidState::new());
    let nkro_writer = HidWriter::<Driver<'static>, 20>::new(
        &mut builder,
        nkro_state,
        HidConfig {
            report_descriptor: hid::NKRO_DESC,
            request_handler: None,
            poll_ms: 1,
            max_packet_size: 64,
            hid_subclass: HidSubclass::Boot,
            hid_boot_protocol: HidBootProtocol::Keyboard,
        },
    );

    // ② Boot 键盘：6KRO，8 字节标准报告（只在 BIOS 模式下发）
    let kbd_state = mk_static!(HidState, HidState::new());
    let kbd_writer = HidWriter::<Driver<'static>, 8>::new(
        &mut builder,
        kbd_state,
        HidConfig {
            report_descriptor: hid::KBD_DESC,
            request_handler: None,
            poll_ms: 1,
            max_packet_size: 8,
            hid_subclass: HidSubclass::Boot,
            hid_boot_protocol: HidBootProtocol::Keyboard,
        },
    );

    // ③ 鼠标：boot protocol，4 字节报告
    let mouse_state = mk_static!(HidState, HidState::new());
    let mouse_writer = HidWriter::<Driver<'static>, 4>::new(
        &mut builder,
        mouse_state,
        HidConfig {
            report_descriptor: hid::MOUSE_DESC,
            request_handler: None,
            poll_ms: 1,
            max_packet_size: 8,
            hid_subclass: HidSubclass::Boot,
            hid_boot_protocol: HidBootProtocol::Mouse,
        },
    );

    // ④ Consumer Control：音量 / 播放暂停等媒体键（2 字节，usage 16 位）
    let cc_state = mk_static!(HidState, HidState::new());
    let cc_writer = HidWriter::<Driver<'static>, 2>::new(
        &mut builder,
        cc_state,
        HidConfig {
            report_descriptor: hid::CONSUMER_DESC,
            request_handler: None,
            poll_ms: 1,
            max_packet_size: 8,
            // 不是 boot device，用 None
            hid_subclass: HidSubclass::No,
            hid_boot_protocol: HidBootProtocol::None,
        },
    );

    let device = builder.build();
    spawner
        .spawn(usb_task(device).expect("failed to spawn usb task"));
    spawner.spawn(
        hid_task(nkro_writer, kbd_writer, mouse_writer, cc_writer)
            .expect("failed to spawn hid task"),
    );

    // ---------------- 屏幕 ----------------
    if LCD_ENABLED {
        spawner
            .spawn(lcd::lcd_task(
                peripherals.SPI2,
                peripherals.GPIO12,
                peripherals.GPIO11,
                peripherals.GPIO10,
                peripherals.GPIO9,
                peripherals.GPIO8,
            )
            .expect("failed to spawn lcd task"));
    }

    // ---------------- flash：读上次存的 WiFi 账号密码 ----------------
    let boot = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );

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

    // ---------------- WiFi：AP + STA 一起开 ----------------
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

    // 联网侧：DHCP 或固定地址
    let sta_net_config = if USE_STATIC_IP {
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(STATIC_IP, 24),
            gateway: Some(GATEWAY_IP),
            dns_servers: heapless::Vec::new(),
        })
    } else {
        NetConfig::dhcpv4(Default::default())
    };
    let (sta_stack, sta_runner) = embassy_net::new(
        interfaces.station,
        sta_net_config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed2,
    );

    // 两个网各跑一个 web server：连热点能用，连家里 WiFi 也能用
    spawner.spawn(ap_web_task(ap_stack).expect("failed to spawn ap web task"));
    spawner.spawn(sta_web_task(sta_stack).expect("failed to spawn sta web task"));
    spawner.spawn(net_task(ap_runner).expect("failed to spawn ap net task"));
    spawner.spawn(net_task(sta_runner).expect("failed to spawn sta net task"));

    // 热点侧必须自带 DHCP + 假 DNS，否则手机连上会"无互联网"然后断开
    spawner.spawn(dhcp_task(ap_stack).expect("failed to spawn dhcp task"));
    spawner.spawn(dns_task(ap_stack).expect("failed to spawn dns task"));

    spawner.spawn(wifi_task(controller, creds.clone()).expect("failed to spawn wifi task"));
    spawner.spawn(status_task(ap_stack, sta_stack).expect("failed to spawn status task"));
    spawner.spawn(boot_task(boot).expect("failed to spawn boot task"));
    // 日志广播挂在热点侧：热点永远在，配网阶段最需要看日志
    spawner.spawn(netlog::netlog_task(ap_stack).expect("failed to spawn netlog task"));

    // 屏幕要显示的东西先填进去
    {
        let mut st = lcd::STATUS.lock().await;
        st.ap_ip = Some(store::AP_IP.octets());
        if let Some(c) = &creds {
            let _ = st.sta_ssid.push_str(&c.ssid);
        }
    }

    info!(
        "=== 热点已开: {} / {} === 配网请打开 http://{}",
        store::AP_SSID,
        store::AP_PASSWORD,
        store::AP_IP
    );

    loop {
        Timer::after(Duration::from_secs(30)).await;
        let sta = sta_stack.config_v4().map(|c| c.address);
        match sta {
            Some(a) => info!("STA IP = {a}  ->  http://{a}   （热点另有一个 192.168.4.1）"),
            None => info!("STA 未连接（热点 {} 一直可用）", store::AP_IP),
        }
    }
}

// ---------------- 任务 ----------------

/// 把两个网的 IP / 状态刷到屏幕。
#[embassy_executor::task]
async fn status_task(ap_stack: Stack<'static>, sta_stack: Stack<'static>) {
    loop {
        {
            let mut st = lcd::STATUS.lock().await;
            st.ap_ip = ap_stack.config_v4().map(|c| c.address.address().octets());
            st.sta_ip = sta_stack.config_v4().map(|c| c.address.address().octets());
            st.usb_ready = USB_READY.load(Ordering::Relaxed);
            st.stations = dns::device_count();
        }
        Timer::after(Duration::from_millis(1000)).await;
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

/// 往屏幕上打一行临时提示（长按 BOOT 时显示进度）
pub async fn set_hint(text: &str) {
    let mut st = lcd::STATUS.lock().await;
    st.hint.clear();
    let _ = st.hint.push_str(text);
}

/// BOOT 按键：**长按 3 秒 → 恢复出厂设置并重启**。
/// 网页 /setup 里的「恢复出厂设置」按钮走的是同一套逻辑。
#[embassy_executor::task]
async fn boot_task(boot: Input<'static>) {
    loop {
        // 等按下（BOOT 按下 = 低电平）
        while boot.is_high() {
            Timer::after(Duration::from_millis(50)).await;
        }

        let t0 = Instant::now();
        let mut armed = false;

        while boot.is_low() {
            let el = (Instant::now() - t0).as_millis();

            if el >= BOOT_RESET_MS {
                info!("[boot] 长按 {BOOT_RESET_MS}ms -> 恢复出厂设置并重启");
                set_hint("factory reset...").await;
                if !factory_reset_flash().await {
                    warn!("[boot] 恢复出厂失败（flash 里那块没擦掉）");
                }
                // 留点时间把日志/屏幕刷出去
                Timer::after(Duration::from_millis(400)).await;
                software_reset();
            }

            // 按下 1.5 秒后开始提示，别让用户以为没反应
            if el >= 1500 && !armed {
                armed = true;
                set_hint("hold: factory reset").await;
            }

            Timer::after(Duration::from_millis(50)).await;
        }

        // 松手了但没到 3 秒：什么都不做，清掉提示
        if armed {
            set_hint("").await;
            info!("[boot] 松手太早，取消");
        }
        Timer::after(Duration::from_millis(200)).await;
    }
}

/// STA 连接管理：开机自动连上次存的 WiFi；网页改了配置就立刻重连。
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>, creds: Option<store::Creds>) {
    let mut cur: Option<store::Creds> = creds;
    let mut connected = false;

    if let Some(c) = &cur {
        connected = connect_sta(&mut controller, c).await;
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
                lcd::STATUS.lock().await.sta_state = lcd::WifiState::Idle;
                connected = false;
            }
        }
    }
}

/// 连一次 STA。返回是否连上。
/// 注意：切配置必须带上 AP，否则 `set_config` 会把热点关掉。
async fn connect_sta(controller: &mut WifiController<'static>, c: &store::Creds) -> bool {
    if !c.is_set() {
        lcd::STATUS.lock().await.sta_state = lcd::WifiState::Idle;
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
        lcd::STATUS.lock().await.sta_state = lcd::WifiState::Failed;
        return false;
    }

    info!("[wifi] 连接 {}（最多等 {STA_CONNECT_TIMEOUT}s）", c.ssid);
    lcd::STATUS.lock().await.sta_state = lcd::WifiState::Connecting;

    match with_timeout(Duration::from_secs(STA_CONNECT_TIMEOUT), controller.connect_async()).await
    {
        Ok(Ok(_)) => {
            info!("[wifi] 已连上 {}", c.ssid);
            lcd::STATUS.lock().await.sta_state = lcd::WifiState::Connected;
            true
        }
        Ok(Err(e)) => {
            warn!("[wifi] 连接失败: {e:?}");
            lcd::STATUS.lock().await.sta_state = lcd::WifiState::Failed;
            false
        }
        Err(_) => {
            warn!("[wifi] 连接超时（{STA_CONNECT_TIMEOUT}s）");
            lcd::STATUS.lock().await.sta_state = lcd::WifiState::Failed;
            false
        }
    }
}

/// 热点模式的 DHCP 服务：没有它，手机连上热点会一直卡在"正在获取 IP 地址"。
#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    // 这几个必须是 'static 的切片，配好就不动了
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

/// 把所有域名都解析到 ESP32 自己，配合手机的俘虏门户探测。
#[embassy_executor::task]
async fn dns_task(stack: Stack<'static>) {
    dns::run(stack, store::AP_IP).await;
}

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, Driver<'static>>) {
    device.run().await;
}

/// 消费 HID 队列，真正往 USB 写报告。
///
/// USB 没枚举时 `write` 会一直挂着，所以每一帧都带超时，
/// 免得一条卡住把后面所有按键都堵死。
#[embassy_executor::task]
async fn hid_task(
    mut nkro: HidWriter<'static, Driver<'static>, 20>,
    mut kbd: HidWriter<'static, Driver<'static>, 8>,
    mut mouse: HidWriter<'static, Driver<'static>, 4>,
    mut cc: HidWriter<'static, Driver<'static>, 2>,
) {
    loop {
        match HID_Q.receive().await {
            HidEvent::Nkro(r) => {
                match with_timeout(Duration::from_millis(200), nkro.write(&r)).await {
                    Ok(Ok(())) => USB_READY.store(true, Ordering::Relaxed),
                    _ => warn!("[hid] NKRO 报告超时（电脑还没识别？）"),
                }
            }
            HidEvent::Boot(r) => {
                match with_timeout(Duration::from_millis(200), kbd.write(&r)).await {
                    Ok(Ok(())) => USB_READY.store(true, Ordering::Relaxed),
                    _ => warn!("[hid] 键盘报告超时（电脑还没识别？）"),
                }
            }
            HidEvent::Mouse(r) => {
                if with_timeout(Duration::from_millis(200), mouse.write(&r))
                    .await
                    .is_err()
                {
                    warn!("[hid] 鼠标报告超时");
                }
            }
            HidEvent::Cc(r) => {
                if with_timeout(Duration::from_millis(200), cc.write(&r))
                    .await
                    .is_err()
                {
                    warn!("[hid] 媒体键报告超时");
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn ap_web_task(stack: Stack<'static>) {
    web_server(stack, "AP").await;
}

#[embassy_executor::task]
async fn sta_web_task(stack: Stack<'static>) {
    web_server(stack, "STA").await;
}

/// HTTP + WebSocket。热点侧和局域网侧各跑一份。
async fn web_server(stack: Stack<'static>, tag: &'static str) {
    let rx = mk_static!([u8; 4096], [0u8; 4096]);
    let tx = mk_static!([u8; 4096], [0u8; 4096]);
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(120)));

    info!("[web/{tag}] 监听 {}:{}", "0.0.0.0", web::HTTP_PORT);

    loop {
        if let Err(e) = socket.accept(web::HTTP_PORT).await {
            warn!("[web/{tag}] accept 失败: {e:?}");
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }

        match web::serve_once(&mut socket).await {
            web::Served::WebSocket => {
                info!("[web/{tag}] WebSocket 已升级");
                let _ = web::ws_text(&mut socket, "_ok").await;

                {
                    let mut st = lcd::STATUS.lock().await;
                    st.ws_clients = st.ws_clients.saturating_add(1);
                }

                let mut buf = [0u8; 256];
                loop {
                    match web::ws_read(&mut socket, &mut buf).await {
                        Ok((web::OP_TEXT, n)) => {
                            dispatch(&buf[..n]);
                        }
                        Ok((web::OP_PING, _)) => {
                            let _ = web::ws_pong(&mut socket).await;
                        }
                        Ok((web::OP_CLOSE, _)) => {
                            info!("[web/{tag}] 客户端关闭");
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }

                // 断开时把所有键松开，免得目标机上卡键
                release_all();
                {
                    let mut st = lcd::STATUS.lock().await;
                    st.ws_clients = st.ws_clients.saturating_sub(1);
                }
            }
            web::Served::Page => {}
            web::Served::None => {}
        }

        socket.close();
        let _ = socket.flush().await;
        socket.abort();
    }
}

/// 解析一行指令，塞进 HID 队列。
///
/// 线协议（WebSocket 文本帧，空格分隔）：
///   `n <40位hex>`                NKRO 键盘报告（20 字节）
///   `k <mod> <k1..k6>`           boot 键盘报告（8 字节，BIOS 模式）
///   `m <btn> <dx> <dy> <wheel>`  鼠标
///   `c <usage>`                  Consumer Control 媒体键（0 = 松开）
///   `b <0|1>`                    切 BIOS 模式
///   `r`                          释放所有键
///   `_p`                         心跳
fn dispatch(cmd: &[u8]) {
    let text = match str::from_utf8(cmd) {
        Ok(s) => s.trim(),
        Err(_) => return,
    };
    let mut it = text.split(' ');

    match it.next() {
        // n <40 hex>  NKRO 报告（20 字节）
        Some("n") => {
            let hex = it.next().unwrap_or("");
            let mut rep = [0u8; 20];
            if decode_hex(hex, &mut rep) != 20 {
                warn!("[hid] NKRO 报告长度不对");
                return;
            }
            // BIOS 模式下主机只认 8 字节 boot 报告，发了也没用
            if BIOS_MODE.load(Ordering::Relaxed) {
                return;
            }
            push(HidEvent::Nkro(rep));
        }
        // k <mod> <k1..k6>  boot 键盘报告（BIOS 模式）
        Some("k") => {
            let mut rep = [0u8; 8];
            rep[0] = parse_u(it.next()) as u8;
            for i in 0..6 {
                rep[2 + i] = parse_u(it.next()) as u8;
            }
            if !BIOS_MODE.load(Ordering::Relaxed) {
                return;
            }
            push(HidEvent::Boot(rep));
        }
        // m <btn> <dx> <dy> <wheel>
        Some("m") => {
            let btn = parse_u(it.next()) as u8;
            let dx = parse_i(it.next());
            let dy = parse_i(it.next());
            let w = parse_i(it.next());
            push(HidEvent::Mouse([btn, dx as u8, dy as u8, w as u8]));
        }
        // c <usage>  媒体键，usage=0 表示松开
        Some("c") => {
            let u = parse_u(it.next());
            push(HidEvent::Cc([
                (u & 0xFF) as u8,
                ((u >> 8) & 0xFF) as u8,
            ]));
        }
        // b <0|1>  切 BIOS 模式
        Some("b") => {
            let on = parse_u(it.next()) != 0;
            info!("[hid] BIOS 模式 = {on}");
            BIOS_MODE.store(on, Ordering::Relaxed);
            // 切换时把两个键盘接口都松开
            push(HidEvent::Nkro([0u8; hid::NKRO_REPORT_LEN]));
            push(HidEvent::Boot([0u8; hid::BOOT_REPORT_LEN]));
        }
        // 释放所有键
        Some("r") => release_all(),
        Some("_p") => {}
        _ => {}
    }
}

/// 队列满就丢一帧（鼠标位移丢一帧无所谓，不能让整条队列堵住）
fn push(ev: HidEvent) {
    if HID_Q.try_send(ev).is_err() {
        warn!("[hid] 队列满，丢一帧");
    }
}

fn release_all() {
    push(HidEvent::Nkro([0u8; hid::NKRO_REPORT_LEN]));
    push(HidEvent::Boot([0u8; hid::BOOT_REPORT_LEN]));
    push(HidEvent::Cc([0u8; hid::CONSUMER_REPORT_LEN]));
}

/// 把 40 个 hex 字符解成 20 字节，返回解出的字节数
fn decode_hex(s: &str, out: &mut [u8]) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i + 1 < b.len() && n < out.len() {
        let hi = match hex_val(b[i]) {
            Some(v) => v,
            None => break,
        };
        let lo = match hex_val(b[i + 1]) {
            Some(v) => v,
            None => break,
        };
        out[n] = (hi << 4) | lo;
        n += 1;
        i += 2;
    }
    n
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
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

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await;
}
