//! ESP32-S3 · Wi-Fi CSI 感知成像
//!
//! 不需要摄像头。Wi-Fi 每一帧的物理层前导码里都带着**信道频率响应**——
//! 56 个 OFDM 子载波上的复数增益（I/Q）。人体在链路附近移动时多径被扰动，
//! 这些子载波的幅度就会跟着抖。把 56 路子载波按时间铺成瀑布图，
//! 就得到一张「射频成像」图：无人时是平滑条纹，有人走动时出现明显扰动带。
//!
//! ## 网络：AP + STA 同时开
//!
//!   - **AP 永远开着**：`CSI-Vision` / 192.168.4.1。配网、救急、看数据都靠它
//!   - **STA**：连家里的 2.4G 路由，配过就开机自动连
//!   - **配网**：连热点 → 打开 192.168.4.1 → 配网 → 填账号密码 → 存 flash
//!
//! AP 模式同样能采 CSI：浏览器连热点后每 10ms 发的 WebSocket 心跳
//! 就是实打实的下行帧，一样触发 CSI 回调。所以没配网也能直接看画面。
//!
//! ## 为什么需要「自己 ping 网关」
//!
//! CSI 只在**收到帧**时上报。STA 模式下如果链路上没有下行流量，
//! 回调一次都不会触发。所以这里跑一个 ICMP 任务持续喂数据。
//!
//! 烧写：`cargo run --release`

#![no_std]
#![no_main]

mod config;
mod csi;
mod lcd;
mod soc_temp;
mod store;
mod web;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{
    Config as NetConfig, IpAddress, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
    StaticConfigV4,
    icmp::{IcmpEndpoint, IcmpSocket, PacketMetadata},
    tcp::TcpSocket,
    udp::{PacketMetadata as UdpMeta, UdpSocket},
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_hal_dhcp_server::{
    Ipv4Addr as DhcpIp, run_dhcp_server, simple_leaser::SimpleDhcpLeaser,
    structs::DhcpServerConfig,
};
use esp_radio::wifi::{
    AuthenticationMethod, Config as WifiConfig, ControllerConfig, Interface, PowerSaveMode,
    WifiController, ap::AccessPointConfig, csi::CsiConfig, sta::StationConfig,
};
use esp_storage::FlashStorage;
use log::{error, info, warn};
use static_cell::StaticCell;

use esp_backtrace as _;

esp_bootloader_esp_idf::esp_app_desc!();

// ============================================================
//  全局状态
// ============================================================
//
// 用原子量而不是 Mutex：这些值被 WebSocket 任务、状态页、串口日志
// 同时读，写入方只有一个（stat_task / CSI 回调），原子量最省事，
// 也不会在 CSI 回调里造成锁竞争。

/// STA 是否已连上路由器。
pub static STA_UP: AtomicBool = AtomicBool::new(false);
/// STA 拿到的 IPv4（大端 u32），0 表示还没拿到。
pub static STA_IP: AtomicU32 = AtomicU32::new(0);
/// 最近一次 RSSI（dBm，负值）。
pub static STA_RSSI: AtomicI32 = AtomicI32::new(0);
/// 芯片温度 ×10（423 = 42.3℃）。0 表示还没读到。
pub static TEMP_X10: AtomicI32 = AtomicI32::new(0);
/// flash 里有没有存过 Wi-Fi。用于决定「陌生 URL 该引导到哪里」：
/// 没配过 → 直接导去 /setup（安卓的联网探测弹窗就会弹出配网页）；
/// 配过 → 回主页。
pub static HAS_CREDS: AtomicBool = AtomicBool::new(false);

/// 网页提交了新 Wi-Fi → 唤醒 wifi_task。
pub static NEW_CREDS: Signal<CriticalSectionRawMutex, store::Creds> = Signal::new();

/// flash 句柄。`FlashStorage::new` 只能调一次，又不能 clone，
/// 所以 main 里拿出来之后放进这个锁里共用。
pub static FLASH: Mutex<CriticalSectionRawMutex, Option<FlashStorage<'static>>> = Mutex::new(None);

/// 保存 Wi-Fi 凭据（web 任务调用）。
pub async fn save_creds(c: store::Creds) -> bool {
    let ok = {
        let mut g = FLASH.lock().await;
        match g.as_mut() {
            Some(f) => store::save(f, &c),
            None => false,
        }
    };
    if ok {
        info!("[store] 已保存 Wi-Fi: {}", c.ssid);
    } else {
        error!("[store] 保存失败");
    }
    ok
}

/// 忘掉已存的 Wi-Fi。
pub async fn clear_creds() {
    let mut g = FLASH.lock().await;
    if let Some(f) = g.as_mut() {
        store::clear(f);
        info!("[store] Wi-Fi 配置已清除");
    }
}

/// 重启。配网写的是 flash，不重启不会重新读。
pub fn restart_now() -> ! {
    warn!("[sys] 重启以应用新配置");
    esp_hal::system::software_reset()
}

// ============================================================
//  静态缓冲
// ============================================================

const POOL: usize = config::WEB_BUF * config::WEB_SESSIONS * 2;

static STA_POOL: StaticCell<[u8; POOL]> = StaticCell::new();
static AP_POOL: StaticCell<[u8; POOL]> = StaticCell::new();

// Socket 槽位必须留够，否则 smoltcp 会 panic（adding a socket to a full SocketSet）。
// STA 侧的账：
//   WEB_SESSIONS(6) 个监听 TCP + 1 个 ICMP(ping) + 1 个 DHCP 客户端 = 8
// 再留 4 个余量，所以是 12。
// 之前写 6 的时候刚好卡在 3 个 web 能跑、5 个就爆 —— 排查了很久。
static STA_RES: StaticCell<StackResources<12>> = StaticCell::new();
static AP_RES: StaticCell<StackResources<12>> = StaticCell::new();

static DNS_RX_META: StaticCell<[UdpMeta; 2]> = StaticCell::new();
static DNS_RX_BUF: StaticCell<[u8; 384]> = StaticCell::new();
static DNS_TX_META: StaticCell<[UdpMeta; 2]> = StaticCell::new();
static DNS_TX_BUF: StaticCell<[u8; 128]> = StaticCell::new();

static PING_RX_META: StaticCell<[PacketMetadata; 2]> = StaticCell::new();
static PING_RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
static PING_TX_META: StaticCell<[PacketMetadata; 2]> = StaticCell::new();
static PING_TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();

// ============================================================

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    // 堆和主栈共用同一块 DRAM，**堆每加 1KB 主栈就少 1KB**。
    // 原来是 128KB，加了自激流量的两个任务后直接栈溢出
    // （stack pointer out of range）。96KB 对 esp-radio（约需 80KB）
    // + embassy-net 足够，把剩下的让给栈。
    esp_alloc::heap_allocator!(size: 96 * 1024);

    // 160MHz 而不是 240MHz：CSI 的瓶颈在射频不在 CPU
    // （64 路整数开方 × 100Hz，80MHz 都绰绰有余）。
    // 实测 240MHz 时芯片结温会爬到 72℃，降频能省 8~12℃ 且毫无体感差异。
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_160MHz));

    let timg0 = TimerGroup::new(p.TIMG0);
    let sw_int = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("");
    info!("=== ESP32-S3 Wi-Fi CSI 感知成像 ===");
    info!(
        "内部堆 {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into())
    );

    // ---- 读上次配网的 Wi-Fi ----
    let mut flash = FlashStorage::new(p.FLASH).multicore_auto_park();
    let creds = match store::load(&mut flash) {
        Some(c) => {
            info!("[store] 已存 Wi-Fi: {}", c.ssid);
            Some(c)
        }
        None => {
            info!("[store] 还没配网，连热点 {} 打开页面配", config::AP_SSID);
            None
        }
    };
    HAS_CREDS.store(creds.is_some(), Ordering::Relaxed);
    FLASH.lock().await.replace(flash);

    // ---- Wi-Fi：始终 APSTA，热点永远开着 ----
    //
    // 坑：StationConfig 默认 auth_method = Wpa2Personal，密码为空时
    // esp_wifi_set_config 返回 ESP_ERR_WIFI_PASSWORD，整个 wifi::new 失败。
    // 没配过网时用「占位 STA」（开放网络 + 不存在的 SSID），保证模式全程是 APSTA。
    let initial = match &creds {
        Some(c) if c.is_set() => WifiConfig::AccessPointStation(sta_config(c), ap_config()),
        _ => WifiConfig::AccessPointStation(placeholder_sta(), ap_config()),
    };

    let (mut controller, interfaces) = match esp_radio::wifi::new(
        p.WIFI,
        ControllerConfig::default().with_initial_config(initial),
    ) {
        Ok(v) => v,
        Err(e) => {
            error!("[wifi] 初始化失败: {e:?}");
            loop {
                Timer::after(Duration::from_secs(60)).await;
            }
        }
    };

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // STA：默认 DHCP
    let (stack, sta_runner) = embassy_net::new(
        interfaces.station,
        NetConfig::dhcpv4(Default::default()),
        STA_RES.init(StackResources::<12>::new()),
        seed,
    );
    // AP：自己就是网关
    let (ap_stack, ap_runner) = embassy_net::new(
        interfaces.access_point,
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(
                Ipv4Address::new(
                    config::AP_IP[0],
                    config::AP_IP[1],
                    config::AP_IP[2],
                    config::AP_IP[3],
                ),
                24,
            ),
            gateway: None,
            dns_servers: heapless::Vec::new(),
        }),
        AP_RES.init(StackResources::<12>::new()),
        seed ^ 0xA5A5_5A5A_1234_5678,
    );

    spawner.spawn(net_task(sta_runner).expect("spawn sta net"));
    spawner.spawn(net_task(ap_runner).expect("spawn ap net"));
    spawner.spawn(dhcp_task(ap_stack).expect("spawn dhcp"));

    spawn_web(spawner, stack, "STA", STA_POOL.init([0; POOL]));
    spawn_web(spawner, ap_stack, "AP", AP_POOL.init([0; POOL]));

    // 省电必须关：Minimum/Maximum 会让射频按 DTIM 休眠，
    // 收包节奏变得稀疏且不均匀，CSI 帧率会掉到个位数。
    match controller.set_power_saving(PowerSaveMode::None) {
        Ok(()) => info!("[wifi] 省电模式已关闭（CSI 需要持续收包）"),
        Err(e) => warn!("[wifi] 关闭省电失败: {e:?}"),
    }

    // 发射功率拉到 20dBm（原始值 80）。不设的话用的是芯片默认档，
    // 实测有些板子默认偏低，热点在同桌都扫不到。
    match controller.set_max_tx_power(80) {
        Ok(()) => info!("[wifi] 发射功率 = 20dBm"),
        Err(e) => warn!("[wifi] 设置发射功率失败: {e:?}"),
    }

    // 诊断：确认 AP 落在哪个信道（mode() 是 private，拿不到）
    match controller.channel() {
        Ok((p, _)) => info!("[wifi] 信道={p}"),
        Err(e) => warn!("[wifi] 读信道失败: {e:?}"),
    }

    // 先开一次 CSI —— **AP 模式下也能采数**。
    // 只要浏览器连上热点打开页面，每 10ms 一个的 WebSocket 心跳
    // 就是实打实的下行帧，一样触发 CSI 回调。
    match controller.set_csi(csi_config(), csi::on_csi) {
        Ok(()) => info!("[csi] 已启用（dump_ack 打开，无客户端也能采数）"),
        Err(e) => warn!("[csi] 预启用失败（等连上 STA 后再试）: {e:?}"),
    }

    spawner.spawn(ping_task(stack).expect("spawn ping"));
    spawner.spawn(dns_trigger_task(stack).expect("spawn dns trigger"));
    spawner.spawn(stat_task(stack).expect("spawn stat"));

    // 0.96" ST7735S：显示 AP 账号密码 / STA IP / 温度 / CSI 帧率。
    // 放在 WiFi 初始化之后 spawn：屏幕初始化那几十毫秒阻塞不影响射频校准。
    spawner.spawn(
        lcd::lcd_task(
            p.SPI2,
            p.GPIO10,
            p.GPIO11,
            p.GPIO12,
            p.GPIO13,
            p.GPIO14,
        )
        .expect("spawn lcd"),
    );

    info!(
        "热点 {}  ->  http://{}.{}.{}.{}   配网: /setup",
        config::AP_SSID, config::AP_IP[0], config::AP_IP[1], config::AP_IP[2], config::AP_IP[3]
    );

    // ---- STA 连接管理 ----
    spawner.spawn(wifi_task(controller, creds).expect("spawn wifi task"));

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

// ============================================================
//  Wi-Fi 配置
// ============================================================

/// 热点配置。每次改 WiFi 模式都必须带上它，否则热点会被关掉。
fn ap_config() -> AccessPointConfig {
    let base = AccessPointConfig::default()
        .with_ssid(config::AP_SSID)
        .with_max_connections(4);
    if config::AP_PASS.is_empty() {
        base
    } else {
        base.with_password(config::AP_PASS.into())
            .with_auth_method(AuthenticationMethod::Wpa2Personal)
    }
}

/// STA 配置：认证方式必须跟着密码走（空密码 = 开放网络）。
fn sta_config(c: &store::Creds) -> StationConfig {
    let sta = StationConfig::default().with_ssid(c.ssid.as_str());
    if c.pass.is_empty() {
        sta.with_auth_method(AuthenticationMethod::None)
    } else {
        sta.with_auth_method(AuthenticationMethod::Wpa2Personal)
            .with_password(c.pass.as_str().into())
    }
}

/// 还没配网时用的占位 STA：永远不会连上，只是为了让模式保持 APSTA。
fn placeholder_sta() -> StationConfig {
    StationConfig::default()
        .with_ssid("esp32-idle")
        .with_auth_method(AuthenticationMethod::None)
}

/// CSI 配置。
///
/// **必须打开 `dump_ack_en`**：这个 CPE 把 ICMP 全拦了（发出 700 多个 ping，
/// 回包 0 个），所以「ping 网关拿 echo reply 触发 CSI」这条路走不通。
/// 打开后，ESP32 自己发数据时收到的 MAC 层 ACK 帧也会上报 CSI ——
/// ACK 虽然是控制帧、只带 L-LTF，但前导码是完整的，振幅照样能用。
///
/// 这样即使没有任何客户端连进来，帧率也不会掉到 0。
fn csi_config() -> CsiConfig {
    let mut c = CsiConfig::default();
    c.dump_ack_en = true;
    c
}

// ============================================================
//  任务
// ============================================================

#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// 热点的 DHCP。没有它，手机连上会一直卡在「正在获取 IP 地址」。
#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    static GATEWAYS: [DhcpIp; 1] = [DhcpIp::new(
        config::AP_IP[0],
        config::AP_IP[1],
        config::AP_IP[2],
        config::AP_IP[3],
    )];
    static DNS: [DhcpIp; 1] = [DhcpIp::new(
        config::AP_IP[0],
        config::AP_IP[1],
        config::AP_IP[2],
        config::AP_IP[3],
    )];

    let cfg = DhcpServerConfig {
        ip: DhcpIp::new(
            config::AP_IP[0],
            config::AP_IP[1],
            config::AP_IP[2],
            config::AP_IP[3],
        ),
        lease_time: Duration::from_secs(3600),
        gateways: &GATEWAYS,
        subnet: Some(DhcpIp::new(255, 255, 255, 0)),
        dns: &DNS,
        use_captive_portal: true,
    };
    let mut leaser = SimpleDhcpLeaser {
        start: DhcpIp::new(config::AP_IP[0], config::AP_IP[1], config::AP_IP[2], 2),
        end: DhcpIp::new(config::AP_IP[0], config::AP_IP[1], config::AP_IP[2], 10),
        leases: heapless::Vec::new(),
    };

    info!("[dhcp] 启动");
    if let Err(e) = run_dhcp_server(stack, cfg, &mut leaser).await {
        error!("[dhcp] 退出: {e:?}");
    }
}

/// STA 连接管理：开机自动连上次存的 Wi-Fi；网页配网后重启再连。
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>, creds: Option<store::Creds>) {
    let mut creds = creds;

    loop {
        let c = match creds.clone() {
            Some(c) if c.is_set() => c,
            _ => {
                // 没配网：一直等网页提交
                STA_UP.store(false, Ordering::Relaxed);
                creds = Some(NEW_CREDS.wait().await);
                continue;
            }
        };

        info!("[wifi] 连接 {} …", c.ssid);
        let connected = match with_timeout(
            Duration::from_secs(config::STA_TIMEOUT_SECS),
            controller.connect_async(),
        )
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                warn!("[wifi] 连接失败: {e:?}（密码错 / 只支持 5G / 信号弱）");
                false
            }
            Err(_) => {
                warn!("[wifi] 连接超时 {}s", config::STA_TIMEOUT_SECS);
                false
            }
        };

        if connected {
            info!("[wifi] 已连上 {}", c.ssid);
            // 射频重配过，CSI 再设一次最保险
            let _ = controller.set_csi(csi_config(), csi::on_csi);

            // 等断线 / 等新配置
            match select(
                NEW_CREDS.wait(),
                controller.wait_for_disconnect_async(),
            )
            .await
            {
                Either::First(n) => {
                    info!("[wifi] 收到新配置: {}", n.ssid);
                    creds = Some(n);
                }
                Either::Second(_) => {
                    warn!("[wifi] STA 断开，准备重连");
                    STA_UP.store(false, Ordering::Relaxed);
                }
            }
        } else {
            STA_UP.store(false, Ordering::Relaxed);
            // 失败：等一会儿重试，期间用户可以在网页上改配置
            match select(
                NEW_CREDS.wait(),
                Timer::after(Duration::from_secs(config::STA_RETRY_SECS)),
            )
            .await
            {
                Either::First(n) => {
                    info!("[wifi] 收到新配置: {}", n.ssid);
                    creds = Some(n);
                }
                Either::Second(_) => {}
            }
        }
    }
}

/// 自发 ping 网关，制造稳定的下行流量来触发 CSI。
///
/// 为什么不用 UDP：UDP 发出去对端不一定回，而 ICMP echo 是必然回的，
/// 而且是「发一个收一个」的确定节奏，CSI 帧率稳定可预测。
/// 用 DNS 查询给 CSI 喂流量。
///
/// 背景：这个 CPE 把 ICMP 全拦了（发 700 多个 ping，回包 0），
/// `dump_ack_en` 也不上报 ACK 帧的 CSI，而客户端隔离又让「连自己」失败。
/// 三条自触发路全断，导致页面一关 CSI 帧率就掉到个位数。
///
/// DNS 是第四条路，而且这条一定通：
///   - DNS 服务器就是网关（DHCP 分配的），路由器**必须**应答 DNS，否则全网没法上网
///   - 请求路径是「客户端 → 网关」，属于基础转发，**不受客户端隔离影响**
///     （隔离只拦客户端之间的单播）
///   - 响应是标准 UDP 数据帧，带完整前导码，CSI 正常上报
///
/// 我们不需要解析响应内容，只要它回来触发射频收包就行。
#[embassy_executor::task]
async fn dns_trigger_task(stack: Stack<'static>) {
    let mut sock = UdpSocket::new(
        stack,
        DNS_RX_META.init([UdpMeta::EMPTY; 2]),
        DNS_RX_BUF.init([0; 384]),
        DNS_TX_META.init([UdpMeta::EMPTY; 2]),
        DNS_TX_BUF.init([0; 128]),
    );
    // 绑 0 = 让协议栈分配临时端口
    if sock.bind(0).is_err() {
        error!("[dns] bind 失败，CSI 没有触发流量");
        return;
    }

    // 最小 DNS 查询：查 "a.cn" 的 A 记录。查不存在/存在的域名都行，
    // 服务器总要回一个响应（答案或 NXDOMAIN），我们要的只是这个响应包。
    const QUERY: [u8; 22] = [
        0xAB, 0xCD, // ID
        0x01, 0x00, // Flags: RD
        0x00, 0x01, // QDCOUNT = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // AN / NS / AR
        0x01, b'a', 0x02, b'c', b'n', 0x00, // QNAME = a.cn
        0x00, 0x01, // QTYPE  = A
        0x00, 0x01, // QCLASS = IN
    ];

    let mut sent: u32 = 0;
    let mut recvd: u32 = 0;
    let mut reported = false;
    let mut report = Instant::now();
    let mut last_sent: u32 = 0;
    // 按「绝对节拍」调度，而不是每轮固定 sleep：
    // send + 等响应本身要花 1~4ms，固定 sleep 20ms 会让实际周期变成 22~24ms，
    // 速率就飘了。用 next 累加目标时刻，落后了才补齐。
    let mut next = Instant::now();

    loop {
        let dns = match stack.config_v4() {
            Some(c) => c.dns_servers.first().copied(),
            None => None,
        };
        let Some(dns) = dns else {
            Timer::after(Duration::from_secs(1)).await;
            next = Instant::now();
            continue;
        };

        if sock.send_to(&QUERY, (dns, 53)).await.is_ok() {
            sent += 1;
        }

        // 等响应。只要收到就说明射频收到了数据帧，CSI 已经触发，
        // 内容直接丢掉不做解析。
        if let Ok(true) = with_timeout(
            Duration::from_millis(4),
            sock.recv_from_with(|_d, _m| true),
        )
        .await
        {
            recvd += 1;
        }

        if report.elapsed() >= Duration::from_secs(4) {
            // 用真实窗口宽度算速率，而不是拿配置值充数
            let ms = report.elapsed().as_millis() as u32;
            let hz = if ms > 0 {
                sent.wrapping_sub(last_sent) * 1000 / ms
            } else {
                0
            };
            last_sent = sent;
            report = Instant::now();
            info!(
                "[dns] 发出 {} / 收到 {}（实测 {} Hz，目标 {} Hz）",
                sent,
                recvd,
                hz,
                1000 / config::DNS_INTERVAL_MS
            );
            if sent > 20 && recvd == 0 && !reported {
                reported = true;
                warn!("[dns] 发了很多查询但没响应：路由器可能不是 DNS 服务器");
            }
        }

        next += Duration::from_millis(config::DNS_INTERVAL_MS);
        let now = Instant::now();
        if next > now {
            Timer::after(next - now).await;
        } else {
            // 落后太多就重新对齐，不追债（否则会连发一串补上）
            next = now;
        }
    }
}

#[embassy_executor::task]
async fn ping_task(stack: Stack<'static>) {
    let mut sock = IcmpSocket::new(
        stack,
        PING_RX_META.init([PacketMetadata::EMPTY; 2]),
        PING_RX_BUF.init([0; 256]),
        PING_TX_META.init([PacketMetadata::EMPTY; 2]),
        PING_TX_BUF.init([0; 256]),
    );
    if sock.bind(IcmpEndpoint::Ident(0x4353)).is_err() {
        error!("[ping] bind 失败，CSI 将没有触发流量");
        return;
    }

    let payload = [0x5au8; 32];
    let mut next = Instant::now();
    let mut sent: u32 = 0;
    let mut failed: u32 = 0;
    let mut recvd: u32 = 0;
    let mut no_gw: u32 = 0;
    let mut report = Instant::now();

    loop {
        let gw = match stack.config_v4() {
            Some(c) => c.gateway,
            None => None,
        };

        let Some(gw) = gw else {
            no_gw += 1;
            if no_gw == 1 || no_gw % 20 == 0 {
                // 第一次和之后每 20 次打一行，别刷屏
                match stack.config_v4() {
                    Some(c) => warn!("[ping] 配置存在但没有网关: {:?}", c),
                    None => warn!("[ping] 还没拿到 DHCP 配置"),
                }
            }
            Timer::after(Duration::from_millis(500)).await;
            next = Instant::now();
            continue;
        };

        match sock.send_to(&payload, IpAddress::Ipv4(gw)).await {
            Ok(()) => sent += 1,
            Err(_) => failed += 1,
        }
        // 收 echo reply：这既是「网关到底回不回」的判据，
        // 也顺手清空 rx 队列（不读的话 smoltcp 之后会开始丢）。
        if let Ok(Ok(_)) = with_timeout(
            Duration::from_millis(2),
            sock.recv_from_with(|(_, _)| ()),
        )
        .await
        {
            recvd += 1;
        }

        if report.elapsed() >= Duration::from_secs(4) {
            report = Instant::now();
            info!(
                "[ping] gw={} 发出 {} / 发出失败 {} / 收到回包 {} / 无网关 {}",
                gw, sent, failed, recvd, no_gw
            );
            if sent > 20 && recvd == 0 {
                warn!("[ping] 包发出去了但一个回包都没收到：网关禁 ICMP，CSI 拿不到触发流量");
            }
            if failed > 0 && sent > 0 && failed * 2 > sent {
                warn!("[ping] 一半以上发不出去，路由器可能禁 ICMP 或 limit 了速率");
            }
        }

        next += Duration::from_millis(config::PING_INTERVAL_MS);
        let now = Instant::now();
        if next > now {
            Timer::after(next - now).await;
        } else {
            next = now;
        }
    }
}

/// 每 2 秒刷新一次全局状态（IP / RSSI / 温度）并打一行串口。
///
/// 上板调试时，这一行是判断「CSI 到底有没有数据」最直接的依据。
#[embassy_executor::task]
async fn stat_task(stack: Stack<'static>) {
    let mut last_frames = 0u32;
    let mut tick = 0u32;

    loop {
        Timer::after(Duration::from_secs(2)).await;

        // --- 网络状态 ---
        let cfg = stack.config_v4();
        let up = cfg.is_some();
        STA_UP.store(up, Ordering::Relaxed);
        if let Some(c) = cfg {
            STA_IP.store(
                u32::from_be_bytes(c.address.address().octets()),
                Ordering::Relaxed,
            );
        }
        STA_RSSI.store(csi::LAST_RSSI.load(Ordering::Relaxed), Ordering::Relaxed);

        // --- 芯片温度 ---
        let mut temp_note = heapless::String::<24>::new();
        if let Some(raw) = soc_temp::read_raw() {
            let a = soc_temp::celsius_x10(raw);
            TEMP_X10.store(a, Ordering::Relaxed);
            let _ = core::fmt::Write::write_fmt(
                &mut temp_note,
                format_args!("{}.{} ℃", a / 10, a % 10),
            );
            // 开机第一次把 raw 和两套换算都打出来备查（对照 power-key 用）
            if tick == 0 {
                info!(
                    "[temp] raw={} → IDF {}.{}℃ / alt {}.{}℃（alt 已证伪，仅参考）",
                    raw, a / 10, a % 10,
                    soc_temp::celsius_alt_x10(raw) / 10, soc_temp::celsius_alt_x10(raw) % 10
                );
            }
        }

        // --- CSI 帧率 ---
        let total = csi::FRAMES.load(Ordering::Relaxed);
        let fps = total.wrapping_sub(last_frames) / 2;
        last_frames = total;
        tick += 1;

        let ip = STA_IP.load(Ordering::Relaxed).to_be_bytes();
        info!(
            "[csi] {:>4} 帧/秒 | rssi {:>4} | {} 路子载波 | {} | IP {}.{}.{}.{}",
            fps,
            STA_RSSI.load(Ordering::Relaxed),
            csi::LAST_N.load(Ordering::Relaxed),
            temp_note.as_str(),
            ip[0],
            ip[1],
            ip[2],
            ip[3]
        );

        if tick % 15 == 0 && !up {
            info!(
                "[提示] 还没连上路由器，连热点 {} 打开 http://{}.{}.{}.{}/setup 配网",
                config::AP_SSID,
                config::AP_IP[0],
                config::AP_IP[1],
                config::AP_IP[2],
                config::AP_IP[3]
            );
        }
    }
}

/// 一个 stack 上起 `WEB_SESSIONS` 个会话任务（各自一个监听 socket）。
///
/// smoltcp 允许多个 socket 同时 listen 同一端口，新连接自然落到空闲那个，
/// 不需要额外分发器。好处是一个连接升级成 WebSocket 后，另一个还能接新请求。
fn spawn_web(spawner: Spawner, stack: Stack<'static>, tag: &'static str, buf: &'static mut [u8]) {
    let (mut rest_rx, mut rest_tx) = buf.split_at_mut(config::WEB_BUF * config::WEB_SESSIONS);
    for _ in 0..config::WEB_SESSIONS {
        let (r, r2) = rest_rx.split_at_mut(config::WEB_BUF);
        let (t, t2) = rest_tx.split_at_mut(config::WEB_BUF);
        spawner
            .spawn(web_task(stack, tag, r, t).expect("spawn web session"));
        rest_rx = r2;
        rest_tx = t2;
    }
}

// pool_size 必须 ≥ WEB_SESSIONS × 2（AP + STA 两个 stack 各 WEB_SESSIONS 个），
// 现在 6×2=12，正好留满。少一个 spawn 会返回 Busy 然后 .expect() panic。
#[embassy_executor::task(pool_size = 12)]
async fn web_task(
    stack: Stack<'static>,
    tag: &'static str,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
) {
    let mut socket = TcpSocket::new(stack, rx, tx);
    info!("[web/{tag}] 监听 :{}", config::HTTP_PORT);

    loop {
        socket.set_timeout(Some(Duration::from_secs(120)));
        if let Err(e) = socket.accept(config::HTTP_PORT).await {
            // 只给 50ms：accept 失败后这个槽处于非 listen 状态，
            // 等太久会让新连接直接被 RST（这就是「网页打不开」的元凶之一）。
            warn!("[web/{tag}] accept 失败: {e:?}");
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        // 收请求头只给 10 秒：手机息屏 / 标签页冻结时对端不发 FIN，
        // 这条连接会一直占着槽位。
        socket.set_timeout(Some(Duration::from_secs(10)));

        if let web::Served::WebSocket = web::serve_once(&mut socket).await {
            info!("[web/{tag}] WS 已连接");
            // 浏览器每 10ms 发一次心跳，20 秒无数据说明对方真的没了。
            // 不能太长：手机息屏 / 切网时对端不发 FIN，这条连接会一直占着槽位，
            // 槽位被僵尸连接吃满就是「网页打不开」。
            socket.set_timeout(Some(Duration::from_secs(20)));
            ws_session(&mut socket, tag == "AP").await;
            info!("[web/{tag}] WS 断开");
        }

        // 关闭顺序很重要：close() 只是进入 FIN-WAIT，数据靠 flush 发出去。
        // flush 之后立刻 abort() 会发 RST —— 对端（浏览器）正在收 20KB 的页面，
        // 收到 RST 就报「连接被重置」，页面白屏。
        // 所以先 flush，再留 120ms 让 smoltcp 把数据和 FIN 真正送出去。
        socket.close();
        let _ = socket.flush().await;
        Timer::after(Duration::from_millis(120)).await;
        socket.abort();
    }
}

/// WebSocket 会话：一边收浏览器心跳，一边推最新 CSI。
///
/// **为什么不用 `select(csi, ws_read)`**：`ws_read` 内部是多次 `read` 拼出来的，
/// 一旦被 select 取消（CSI 每 20ms 就绪一次，几乎必然先到），已经读进缓冲的
/// 半个帧头就丢了，之后整条流的帧边界全部错位，页面会一直解析失败。
///
/// 所以反过来：**由浏览器的 10ms 心跳驱动循环**，每次读完一帧就顺手把
/// 最新的 CSI 推出去。心跳既保活又当节拍器，还顺便充当第二路 CSI 触发流量。
async fn ws_session(socket: &mut TcpSocket<'_>, is_ap: bool) {
    // 拿不到就安静退出这个会话，绝不能 unwrap ——
    // 一个页面出问题不应该把整个固件打崩。
    let mut live = match csi::LIVE.receiver() {
        Some(r) => r,
        None => {
            warn!("[web] 订阅者已满（不应发生），拒绝本次会话");
            return;
        }
    };
    let mut buf = [0u8; 64];
    let mut frame = [0u8; 8 + csi::MAX_CARRIERS];

    let mut last_push = 0u32;
    let mut last_stat = Instant::now();
    let mut last_frames = 0u32;
    let mut stat = heapless::String::<160>::new();

    let _ = web::ws_text(socket, "{\"t\":\"hello\",\"c\":64}").await;

    loop {
        // 1) 有新 CSI 帧就推（Watch 只留最新，天然跳帧不会积压）
        if let Some(s) = live.try_get() {
            // 时间戳倒回（重启 / 32 位溢出）时强制放行，避免卡死不再推帧
            let due = s.ts_ms.wrapping_sub(last_push) >= config::PUSH_MS || s.ts_ms < last_push;
            if due && s.ts_ms != last_push {
                last_push = s.ts_ms;
                let n = s.encode(&mut frame);
                if web::ws_bin(socket, &frame[..n]).await.is_err() {
                    break;
                }
            }
        }

        // 2) 每 500ms 推一次状态（帧率 / RSSI / 温度 / STA 连接）
        if last_stat.elapsed() >= Duration::from_millis(500) {
            last_stat = Instant::now();
            let total = csi::FRAMES.load(Ordering::Relaxed);
            let fps = (total.wrapping_sub(last_frames)) * 2;
            last_frames = total;
            let ip = STA_IP.load(Ordering::Relaxed).to_be_bytes();
            stat.clear();
            let _ = write!(
                stat,
                "{{\"t\":\"st\",\"fps\":{},\"rssi\":{},\"n\":{},\"temp\":{},\"sta\":{},\"ip\":\"{}.{}.{}.{}\",\"mode\":\"{}\"}}",
                fps,
                STA_RSSI.load(Ordering::Relaxed),
                csi::LAST_N.load(Ordering::Relaxed),
                TEMP_X10.load(Ordering::Relaxed),
                STA_UP.load(Ordering::Relaxed),
                ip[0], ip[1], ip[2], ip[3],
                // 让页面知道自己是哪个接口在服务：
                // AP 侧没有网关，ping 任务跑不起来，帧率天然比 STA 侧低，
                // 不说明白用户会以为「网页坏了」
                if is_ap { "ap" } else { "sta" }
            );
            if web::ws_text(socket, &stat).await.is_err() {
                break;
            }
        }

        // 3) 读一帧（阻塞，由浏览器心跳驱动）
        match web::ws_read(socket, &mut buf).await {
            Ok((web::OP_PING, _)) => {
                if web::ws_pong(socket).await.is_err() {
                    break;
                }
            }
            Ok((web::OP_CLOSE, _)) | Err(_) => break,
            Ok(_) => {}
        }
    }
}
