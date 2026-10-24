//! ESP32-S3 作为 USB Host，把安卓手机的 ADB 接口桥接到 TCP 5555
//!
//! 电脑上直接：`adb connect <esp32-ip>:5555` -> `adb shell`
//! ESP32 全程不理解 ADB 协议，只做 USB bulk <-> TCP 的字节流转发，
//! 所以 ADB 的 RSA 认证由电脑上的 adb 客户端完成，手机上弹窗也只弹一次。
//!
//! 烧写与日志：host 模式占用 GPIO19/20，USB-Serial-JTAG 不可用，
//! 必须走 UART0（板载 USB 转串口那个口）。

#![no_std]
#![no_main]

mod enumerate;
mod netlog;
mod usb_host;

use embassy_futures::select::select;
use embassy_net::{Config as NetConfig, Runner, Stack, StackResources, tcp::TcpSocket};
use embassy_time::{Duration, Timer};
use embassy_usb_driver::{
    Direction, EndpointAddress, EndpointInfo, EndpointType,
    host::{DeviceEvent, UsbHostAllocator, UsbHostController, UsbPipe, pipe},
};
use embassy_usb_synopsys_otg::host::OtgHost;
use embedded_io_async::Write as _;
use esp_backtrace as _;
use esp_bootloader_esp_idf::esp_app_desc;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{
    Config as WifiConfig, ControllerConfig, Interface, sta::StationConfig,
};
use log::{info, warn};

esp_app_desc!();

// ===================== 需要你填写的配置 =====================
const WIFI_SSID: &str = "YOUR_WIFI_SSID";
const WIFI_PASSWORD: &str = "YOUR_WIFI_PASSWORD";

/// adb 连接的 TCP 端口，标准 adb 网络调试就是 5555
const TCP_PORT: u16 = 5555;

/// VBUS 是否由 GPIO 控制。
/// - `true` ：用 GPIO4 驱动 P-MOS / 高电平使能的电源芯片（DevKitC 飞线方案）
/// - `false`：VBUS 由外部电源**常供**（转接头 / dongle 方案，推荐）
///   单口 dongle 引不出 GPIO，必须设 false。
const VBUS_SWITCHED: bool = false;
// ===========================================================

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    // host 模式会占用 GPIO19/20 -> USB-Serial-JTAG 失效 -> 串口看不到东西。
    // 所以日志改走 UDP 广播（LOG_PORT=4444），用 tools/netlog_listen.py 收。
    netlog::init();

    esp_alloc::heap_allocator!(size: 100 * 1024);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // ---------------- USB Host ----------------
    info!(
        "=== ESP32-S3 USB Host ADB proxy (vbus_switched={}) ===",
        VBUS_SWITCHED
    );
    let instance = if VBUS_SWITCHED {
        let mut vbus = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
        usb_host::init(Some(&mut vbus))
    } else {
        usb_host::init(None)
    };
    let host = OtgHost::new(instance);
    info!("USB OTG host 初始化完成");

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
    spawner.spawn(net_task(runner).expect("failed to spawn net task"));
    spawner.spawn(netlog::netlog_task(stack).expect("failed to spawn netlog task"));
    spawner.spawn(adb_host_task(host, stack).expect("failed to spawn adb host task"));

    loop {
        Timer::after(Duration::from_secs(30)).await;
        if let Some(cfg) = stack.config_v4() {
            info!("IP = {}  <- adb connect {}:{}", cfg.address, cfg.address, TCP_PORT);
        } else {
            info!("等待 DHCP...");
        }
    }
}

/// USB 主机主循环：等插入 -> 枚举 -> 桥接 -> 断开重来
#[embassy_executor::task]
async fn adb_host_task(mut host: OtgHost<'static>, stack: Stack<'static>) {
    // allocator 是 Copy 且不与 &mut host 冲突，先取出来一直用
    let alloc = host.allocator();

    loop {
        match host.wait_for_device_event().await {
            DeviceEvent::Connected(speed) => {
                info!("[usb] 设备插入，速率 {:?}", speed);
                match enumerate::enumerate_adb(&alloc).await {
                    Ok(iface) => {
                        info!(
                            "[usb] ADB 接口就绪：config={} iface={}",
                            iface.config_value, iface.interface_number
                        );
                        if let Err(e) = bridge_adb(&alloc, iface, stack).await {
                            warn!("[adb] 桥接结束：{}", e);
                        }
                    }
                    Err(e) => warn!("[usb] 枚举失败：{}", e),
                }
            }
            DeviceEvent::Disconnected => info!("[usb] 设备拔出"),
            DeviceEvent::Overcurrent => warn!("[usb] VBUS 过流！检查 5V 供电能力"),
            _ => {}
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

/// 把 ADB 的 bulk IN/OUT 端点原样桥接到一个 TCP 连接
async fn bridge_adb(
    alloc: &impl UsbHostAllocator<'static>,
    iface: enumerate::AdbInterface,
    stack: Stack<'static>,
) -> Result<(), &'static str> {
    use enumerate::DEV_ADDR;

    let (addr_in, mps_in) = iface.ep_in.ok_or("缺少 bulk IN 端点")?;
    let (addr_out, mps_out) = iface.ep_out.ok_or("缺少 bulk OUT 端点")?;

    let ep_in_info = EndpointInfo {
        addr: EndpointAddress::from_parts((addr_in & 0x7F) as usize, Direction::In),
        ep_type: EndpointType::Bulk,
        max_packet_size: mps_in,
        interval_ms: 0,
    };
    let ep_out_info = EndpointInfo {
        addr: EndpointAddress::from_parts((addr_out & 0x7F) as usize, Direction::Out),
        ep_type: EndpointType::Bulk,
        max_packet_size: mps_out,
        interval_ms: 0,
    };

    let mut in_pipe = alloc
        .alloc_pipe::<pipe::Bulk, pipe::In>(DEV_ADDR, &ep_in_info, None)
        .map_err(|_| "分配 bulk IN 管道失败")?;
    let mut out_pipe = alloc
        .alloc_pipe::<pipe::Bulk, pipe::Out>(DEV_ADDR, &ep_out_info, None)
        .map_err(|_| "分配 bulk OUT 管道失败")?;
    info!("[adb] bulk 管道已建立 IN={:#04x} OUT={:#04x}", addr_in, addr_out);

    // 等网络起来，然后监听 5555（同一时刻只服务一个 adb 连接）
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        info!("[adb] 监听 {}:{}  (adb connect {}:{})", cfg.address, TCP_PORT, cfg.address, TCP_PORT);
    }

    let rx = mk_static!([u8; 4096], [0u8; 4096]);
    let tx = mk_static!([u8; 4096], [0u8; 4096]);
    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(60)));

    loop {
        if let Err(e) = socket.accept(TCP_PORT).await {
            warn!("[adb] accept 失败: {:?}", e);
            Timer::after(Duration::from_secs(1)).await;
            continue;
        }
        info!("[adb] 客户端已连接");

        let (mut reader, mut writer) = socket.split();

        let usb_buf = mk_static!([u8; 512], [0u8; 512]);
        let tcp_buf = mk_static!([u8; 2048], [0u8; 2048]);

        // 手机 -> 电脑
        let phone_to_pc = async {
            loop {
                let n = in_pipe
                    .request_in(usb_buf)
                    .await
                    .map_err(|_| "USB IN 传输失败")?;
                if n == 0 {
                    continue;
                }
                writer
                    .write_all(&usb_buf[..n])
                    .await
                    .map_err(|_| "TCP 写失败")?;
            }
            #[allow(unreachable_code)]
            Ok::<(), &'static str>(())
        };

        // 电脑 -> 手机
        let pc_to_phone = async {
            loop {
                let n = reader.read(tcp_buf).await.map_err(|_| "TCP 读失败")?;
                if n == 0 {
                    return Err("对端关闭连接");
                }
                // ensure_transaction_end=true：最后一个包正好是 MPS 整数倍时补 ZLP，
                // 严格按 USB bulk 传输语义结束一次传输
                out_pipe
                    .request_out(&tcp_buf[..n], true)
                    .await
                    .map_err(|_| "USB OUT 传输失败")?;
            }
            #[allow(unreachable_code)]
            Ok::<(), &'static str>(())
        };

        let res = select(phone_to_pc, pc_to_phone).await;
        let err = match res {
            embassy_futures::select::Either::First(r) => r,
            embassy_futures::select::Either::Second(r) => r,
        };
        if let Err(e) = err {
            warn!("[adb] {}", e);
        }

        socket.close();
        drop(in_pipe);
        drop(out_pipe);
        return Err("连接断开");
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
                warn!("WiFi disconnected, retrying");
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
