//! ESP32-S3 语音助手 —— 主流程
//!
//! # 任务划分
//!
//! ```text
//! net_task      smoltcp 轮询，必须一直在跑
//! display_task  240x240 状态表情
//! mic_task      I2S RX → 能量 VAD → MIC_CH
//! speaker_task  SPK_CH → I2S TX
//! ws_task       WebSocket 长连接：上传 MIC_CH，下行 JSON + PCM
//! ```
//!
//! # 数据流
//!
//! ```text
//!  麦克风 ──I2S RX──> mic_task ──VAD──> MIC_CH ──> ws_task ──ws──> 网关
//!                                                                  │
//!                                                          ASR/LLM/TTS
//!                                                                  │
//!  喇叭  <──I2S TX── speaker_task <── SPK_CH <── ws_task <──ws──────┘
//! ```
//!
//! # 唤醒设计
//!
//! 没有本地唤醒词模型（乐鑫的 WakeNet 是闭源 C 库，会破坏纯 Rust 架构）。
//! 这里用「能量门控 + 服务端关键词」：设备端只有在检测到人声（连续 3 帧
//! 超过自适应噪声阈值）时才开始上传，网关识别出「你好小智」就进入会话。
//! 体感上就是喊一声就醒，但只有几 KB 的代码量。

#![no_std]
#![no_main]

extern crate alloc;

mod aec;
mod audio;
mod board;
mod config;
mod display;
mod es8311;
mod esp_sr;
mod json;
mod keys;
mod sdcard;
mod sensors;
mod soc_temp;
mod store;
mod vad;
mod wakenet;
mod web;
mod ws;

use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Ipv4Cidr, Runner, StackResources, StaticConfigV4};
use embassy_time::{Duration, Timer};
use embedded_hal_bus::i2c::RefCellDevice;
use esp_hal::{
    clock::CpuClock,
    gpio::AnyPin,
    i2c::master::{Config as I2cConfig, I2c},
    i2s::master::{Channels, DataFormat, I2s, TdmConfig},
    ram,
    rng::Rng,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_hal_dhcp_server::{
    run_dhcp_server, simple_leaser::SimpleDhcpLeaser, structs::DhcpServerConfig,
    Ipv4Addr as DhcpIp,
};
use esp_println::println;
use esp_radio::wifi::{
    ap::AccessPointConfig, sta::StationConfig, AuthenticationMethodConfig, Config as WifiConfig,
    ControllerConfig, Interface, Password, Ssid,
};
use esp_storage::FlashStorage;

use crate::audio::{State, MIC_CH, SPK_CH};
use crate::config::DEVICE_NAME;
use crate::es8311::Es8311;
use crate::ws::WsConn;

// panic handler 由 esp-backtrace 的 panic-handler feature 提供
use esp_backtrace as _;

// 应用描述符：espflash 生成完整镜像（含 OTA 分区）时必须有这一段，
// 少了它 `espflash save-image` 会直接报错退出
esp_bootloader_esp_idf::esp_app_desc!();

const CPU_CLOCK: CpuClock = CpuClock::_240MHz;

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit().write($val)
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // 最早的一行。放在 esp_hal::init 之前 —— esp-println 直接写 USB-Serial-JTAG
    // 寄存器，不依赖 esp-hal。有这一行就说明「固件跑起来了 + USB 日志通道通了」，
    // 后面任何一步卡住都能靠它区分"没启动"和"启动了但挂在某处"。
    println!("\n[boot] firmware start");

    esp_println::logger::init_logger_from_env();

    // Wi-Fi 协议栈会从内部堆 malloc。
    //
    // 【关键：堆不能全放 .bss】
    // `heap_allocator!(size: N)` 是个 `static mut HEAP: [u8; N]`，N 字节全进 .bss。
    // 而 esp-hal 的 stack.x 里栈大小 = RWDATA 区域剩下的空间：
    //     _stack_end   = BSS 结束处
    //     _stack_start = ORIGIN(RWDATA) + LENGTH(RWDATA)
    // 最初 128KB 全放 .bss，把栈从 160KB 挤到只剩 32KB，启动直接 panic：
    //     stack pointer out of range: sp=0x3fcd2ea0, bottom=0x3fcd37a8
    //
    // 现在分两块：
    //   1. bootloader 归还的 IRAM（0x403B9000~0x403E0000，约 72KB 可用）——
    //      这段本来闲置，走 `#[ram(reclaimed)]`，完全不占 BSS
    //   2. 剩下 48KB 才放 .bss
    // 总 112KB，栈保住 110KB+。
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 48 * 1024);
    println!("[boot] heap ok");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CPU_CLOCK));
    println!("[boot] hal ok");

    // PSRAM 挂进全局堆（esp-radio 内部走 malloc_internal，拿不到 PSRAM）
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);
    println!(
        "[boot] psram ok, {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::External.into())
    );

    // rtos 必须跑在任何 await 之前：Timer、任务调度、软件中断都靠它。
    // 原来这段在第 130 行（在 load_into_memory().await 之后），属于顺序错误。
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    println!("[boot] rtos ok");

    // ---------------- 0. 从 flash 读配置 ----------------
    // WiFi 账号、网关地址、各家 API key 全在这里。没配过就是空的，
    // 下面会进配网模式，只开热点等手机来填。
    web::load_into_memory(FlashStorage::new(peripherals.FLASH).multicore_auto_park()).await;
    let boot_cfg = web::snapshot().await;
    println!(
        "[cfg] 网关={}:{}  LLM={}/{}  WiFi={}",
        boot_cfg.gw_host_or_default(),
        boot_cfg.gw_port,
        boot_cfg.llm_backend_or_default(),
        boot_cfg.llm_model_or_default(),
        if boot_cfg.wifi_set() {
            boot_cfg.wifi_ssid.as_str()
        } else {
            "<未配置，进配网模式>"
        },
    );

    println!("\n=== ESP32-S3 语音助手 ===");
    println!(
        "[boot] PSRAM {} B / 内部堆 {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::External.into()),
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into()),
    );

    // 功放常开。NS4150B 静态电流只有几 mA，调试阶段常开可以省掉
    // 「什么时候该开 PA」这个状态同步问题。想省电就改成
    // 只在 Speaking 状态拉高（需要把 Output 放进共享的 Mutex，跨任务同步）。
    let _pa_en = esp_hal::gpio::Output::new(
        unsafe { AnyPin::steal(board::PIN_PA_EN) },
        esp_hal::gpio::Level::High,
        esp_hal::gpio::OutputConfig::default(),
    );

    // ---------------- 1. I2C 总线：codec / IMU / 触摸 三家共用 ----------------
    // 这块板子的 ES8311、QMI8658、触摸芯片的 SDA/SCL 全是 GPIO42/GPIO41，
    // 物理上就是同一条总线。所以这里**只建一个 I2c 实例**，
    // 后面用 RefCellDevice 派生出多个设备句柄。
    let i2c = I2c::new(
        peripherals.I2C0,
        // 实测 100kHz 下这条总线不稳定：多次上电分别扫到 3 / 4 / 6 个设备，
        // ES8311 的 CHIPID 时而 0x83 时而 0x00。总线上挂着 codec + IMU + 触摸，
        // 0x5E/0x66/0x6F 那几个幽灵地址说明有反射/上拉不足导致的误应答。
        // 降到 50kHz 先求稳 —— 配置和轮询都是低频操作，降速没有副作用。
        I2cConfig::default().with_frequency(Rate::from_khz(50)),
    )
    .expect("I2C 初始化失败")
    .with_sda(unsafe { AnyPin::steal(board::PIN_I2C_SDA) })
    .with_scl(unsafe { AnyPin::steal(board::PIN_I2C_SCL) });

    let i2c_bus = RefCell::new(i2c);

    // ---------------- 1.1 扫描总线 ----------------
    // 这行日志是换板子后**最有价值的一条**：0x18 出现说明 codec 线对了，
    // 0x6B/0x6A 出现说明 IMU 在，剩下的一个地址就是触摸芯片的型号。
    {
        let mut dev = RefCellDevice::new(&i2c_bus);
        let found = sensors::scan(&mut dev);
        println!("[i2c] 扫描到 {} 个设备 @ SDA={} SCL={}",
            found.len(), board::PIN_I2C_SDA, board::PIN_I2C_SCL);
        if found.is_empty() {
            println!("[i2c] 一个都没有！SDA/SCL 接反或没有上拉（board.rs 第 6 节）");
        }
        for &addr in found.iter() {
            println!("  [i2c] 0x{addr:02X}  {}", sensors::addr_name(addr));
        }
    }

    // ---------------- 1.2 ES8311 codec ----------------
    // 先配 codec 再启 I2S：寄存器写入走 I2C，和 MCLK 无关，
    // 反过来（先启 I2S 再配 codec）容易出现首帧丢字。
    {
        let mut codec = Es8311::new(RefCellDevice::new(&i2c_bus), board::ES8311_ADDR);

        match codec.probe() {
            Some(id) => println!("[codec] ES8311 在线，CHIPID=0x{id:02X}"),
            None => {
                println!("[codec] 读不到 ES8311！检查 I2C 引脚（board.rs 第 1 节）");
                println!("[codec] 跳过音频初始化，继续启动网络部分");
            }
        }

        if codec.probe().is_some() {
            match codec.init() {
                Ok(()) => println!("[codec] 初始化完成 16kHz/16bit/mono"),
                Err(_) => println!("[codec] 初始化失败"),
            }
            let _ = codec.set_volume(85);
        }
    }

    // ---------------- 1.3 IMU + 触摸（同一个任务轮询）----------------
    spawner.spawn(sensors_task(i2c_bus).expect("sensors_task"));

    // ---------------- 2. I2S 全双工 ----------------
    let i2s = I2s::new(
        peripherals.I2S0,
        peripherals.DMA_CH0,
        TdmConfig::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(board::SAMPLE_RATE))
            .with_channels(Channels::STEREO)
            .with_data_format(DataFormat::Data16Channel16)
            // TX/RX 共用同一组 BCLK/WS，这是全双工的关键
            .with_signal_loopback(true),
    )
    .expect("I2S 配置失败（采样率/位宽组合不被支持）");

    let i2s = i2s
        .with_mclk(unsafe { AnyPin::steal(board::PIN_I2S_MCLK) })
        .into_async();

    let i2s_tx = i2s
        .i2s_tx
        .with_bclk(unsafe { AnyPin::steal(board::PIN_I2S_BCLK) })
        .with_ws(unsafe { AnyPin::steal(board::PIN_I2S_WS) })
        .with_dout(unsafe { AnyPin::steal(board::PIN_I2S_DOUT) })
        .build();
    let i2s_rx = i2s
        .i2s_rx
        .with_din(unsafe { AnyPin::steal(board::PIN_I2S_DIN) })
        .build();

    spawner.spawn(audio::speaker_task(i2s_tx).expect("speaker_task"));
    spawner.spawn(audio::mic_task(i2s_rx).expect("mic_task"));

    // 三个板载按键：PWR 打断 / PLUS/MINUS 音量
    spawner.spawn(keys::keys_task().expect("keys_task"));

    // ---------------- 3. 屏幕（Wi-Fi 之前就点上）----------------
    // 放在这里而不是 Wi-Fi 之后，是为了开机画面能尽早出现：
    // 连 Wi-Fi 通常要 2~5 秒，屏幕要是等连上再初始化，这段时间就是黑屏。
    spawner.spawn(
        display::display_task(
            peripherals.SPI2,
            unsafe { AnyPin::steal(board::PIN_LCD_SCLK) },
            unsafe { AnyPin::steal(board::PIN_LCD_MOSI) },
            unsafe { AnyPin::steal(board::PIN_LCD_CS) },
            unsafe { AnyPin::steal(board::PIN_LCD_DC) },
            unsafe { AnyPin::steal(board::PIN_LCD_RST) },
            unsafe { AnyPin::steal(board::PIN_LCD_BL) },
        )
        .expect("display_task"),
    );

    // ---------------- 4. Wi-Fi：AP + STA 并存 ----------------
    //
    // 两个坑，都是实测撞出来的（见 power-key-next 的注释）：
    //
    // 坑1：`StationConfig::default()` 的认证方式是 Wpa2Personal。没配过网时
    //      SSID 和密码都是空，`esp_wifi_set_config(WIFI_IF_STA)` 会返回
    //      ESP_ERR_WIFI_PASSWORD，init 直接失败。所以没配网时要用**占位 STA**
    //      （假 SSID + Open），而不是留空。
    //
    // 把实际要用的 AP 凭据打出来：手机提示"密码错误"时，先确认发出去的
    // 到底是不是预期值（flash 里残留脏数据会表现为密码对不上）。
    println!(
        "[wifi] AP 凭据：ssid='{}'({}B) pass='{}'({}B) auth={:?}",
        boot_cfg.ap_ssid_or_default(),
        boot_cfg.ap_ssid_or_default().len(),
        boot_cfg.ap_pass_or_default(),
        boot_cfg.ap_pass_or_default().len(),
        if boot_cfg.ap_pass_or_default().is_empty() {
            "Open"
        } else {
            "WPA/WPA2"
        },
    );
    //
    // 坑2：模式从 AP 变 APSTA 会让射频 stop/start，热点瞬断，正在提交配网表单的
    //      手机就被掐了。所以**开机就是 APSTA**，模式全程不变。
    let ap_cfg = ap_config(&boot_cfg);
    let sta_cfg = if boot_cfg.wifi_set() {
        sta_config(boot_cfg.wifi_ssid.as_str(), boot_cfg.wifi_pass.as_str())
    } else {
        placeholder_sta()
    };

    let mut controller = match esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default()
            .with_initial_config(WifiConfig::AccessPointStation(sta_cfg, ap_cfg)),
    ) {
        Ok(c) => c,
        Err(e) => {
            println!("[wifi] 初始化失败: {e:?}");
            loop {
                Timer::after(Duration::from_secs(3600)).await;
            }
        }
    };

    // 【发射功率必须手动提高】
    // esp-radio 的 WifiController::new() 内部写死了 esp_wifi_set_max_tx_power(20)，
    // 单位是 0.25dBm，也就是只有 5dBm —— 正常值是 78~84（19.5~21dBm）。
    // 5dBm 下手机能扫到信标，但握手/DHCP 交互会失败，表现就是「搜到但连不上」。
    // 78 = 19.5dBm，是 ESP32-S3 的常用满功率档。
    if let Err(e) = controller.set_max_tx_power(78) {
        println!("[wifi] 设置发射功率失败: {e:?}");
    } else {
        println!("[wifi] 发射功率已提到 19.5dBm");
    }

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let seed2 = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A;

    // ---- 热点侧：自己做网关，地址固定 192.168.4.1 ----
    let (ap_stack, ap_runner) = embassy_net::new(
        Interface::access_point(),
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(store::AP_IP, 24),
            gateway: None,
            dns_servers: heapless::Vec::new(),
        }),
        mk_static!(StackResources<8>, StackResources::<8>::new()),
        seed,
    );
    spawner.spawn(net_task(ap_runner).expect("ap net task"));

    // ---- 联网侧：DHCP 拿地址，语音流量走这条 ----
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        NetConfig::dhcpv4(Default::default()),
        mk_static!(StackResources<8>, StackResources::<8>::new()),
        seed2,
    );
    spawner.spawn(net_task(runner).expect("sta net task"));

    // ---- 配网服务：两个网各起一份，热点侧和局域网侧都能打开配置页 ----
    //
    // 缓冲放 PSRAM，不放 BSS：4 条 TcpSocket 缓冲 + 请求/页面缓冲加起来
    // 二十多 KB，直接 `mk_static!` 会把内部 RAM 的 BSS 撑爆，链接器报
    // `stack.x:13 cannot move location counter backwards`（栈没地方放了）。
    spawner.spawn(
        web::web_task(
            ap_stack,
            "AP",
            alloc_psram_buf(2048),
            alloc_psram_buf(2048),
            alloc_psram_buf(web::MAX_REQ),
            alloc_psram_buf(web::PAGE_BUF),
        )
        .expect("web ap"),
    );
    spawner.spawn(
        web::web_task(
            stack,
            "STA",
            alloc_psram_buf(2048),
            alloc_psram_buf(2048),
            alloc_psram_buf(web::MAX_REQ),
            alloc_psram_buf(web::PAGE_BUF),
        )
        .expect("web sta"),
    );

    // 热点侧必须自带 DHCP，否则手机连上会"无互联网"然后自动断开
    spawner.spawn(dhcp_task(ap_stack).expect("dhcp"));

    println!(
        "[wifi] 热点已开：{} / {}  →  http://{}",
        boot_cfg.ap_ssid_or_default(),
        if boot_cfg.ap_pass_or_default().is_empty() {
            "开放"
        } else {
            boot_cfg.ap_pass_or_default()
        },
        store::AP_IP,
    );

    // ---- 连家里 WiFi ----
    let mut connected = false;
    if boot_cfg.wifi_set() {
        for attempt in 1..=3u8 {
            println!(
                "[wifi] 连接 {}（第 {attempt} 次）…",
                boot_cfg.wifi_ssid.as_str()
            );
            if controller.connect_async().await.is_ok() {
                connected = true;
                break;
            }
            Timer::after(Duration::from_secs(5)).await;
        }
    }

    // 没配网 / 连不上：进配网模式。热点、网页、屏幕都照常工作，
    // 只是不去连网关 —— 用户改完配置保存后设备会自己重启。
    if !connected {
        println!("[wifi] 进入配网模式，屏幕显示热点地址");
        let mut ap_ip_text: heapless::String<16> = heapless::String::new();
        {
            use core::fmt::Write;
            let _ = write!(ap_ip_text, "{}", store::AP_IP);
        }
        // 先置标志再发信号：屏幕收到信号后会立刻读这个标志决定画哪页，
        // 顺序反了会先闪一下状态页再跳引导页。
        display::SETUP_MODE.store(true, core::sync::atomic::Ordering::Relaxed);
        display::IP_READY.signal(ap_ip_text);
        loop {
            Timer::after(Duration::from_secs(3600)).await;
        }
    }

    stack.wait_config_up().await;
    let mut ip_text: heapless::String<16> = heapless::String::new();
    if let Some(cfg) = stack.config_v4() {
        println!("[wifi] IP = {}", cfg.address);
        use core::fmt::Write;
        let _ = write!(ip_text, "{}", cfg.address);
    }

    // 通知屏幕：IP 到手了，可以从开机画面切到状态页
    display::IP_READY.signal(ip_text);

    // ---------------- 4.5 TF 卡 ----------------
    // SD 卡用 SPI 兼容模式（CLK16/CMD15/D0_17/D3_14），原因见 sdcard.rs 顶部。
    // 挂卡是阻塞的，没插卡时约 500ms 后放弃 —— 属于可选外设，失败不影响语音链路。
    match sdcard::SdStorage::mount(
        peripherals.SPI3,
        peripherals.GPIO16,
        peripherals.GPIO15,
        peripherals.GPIO17,
        peripherals.GPIO14,
    ) {
        Ok(card) if card.blocks() > 0 => {
            println!("[sd] 就绪 {} MB（{} 块）", card.bytes() / 1024 / 1024, card.blocks())
        }
        Ok(_) => println!("[sd] 没插卡，或卡不认（格式化成 FAT32 再试）"),
        Err(e) => println!("[sd] 挂载失败: {e}"),
    }

    // ---------------- 5. WebSocket 主循环 ----------------
    ws_loop(stack).await
}

/// IMU + 触摸轮询。
///
/// 两者和 codec 共用 GPIO42/41 这条 I2C，所以必须**顺序访问**。
/// 放在同一个任务里是最省事的做法，也天然避免了总线竞争。
/// 任一设备探测失败都不影响另一台 —— 都失败就空转。
#[embassy_executor::task]
async fn sensors_task(bus: RefCell<I2c<'static, esp_hal::Blocking>>) {
    use crate::sensors::{Qmi8658, Touch};

    let mut imu = Qmi8658::new(RefCellDevice::new(&bus), board::QMI8658_ADDR);
    let mut touch = Touch::probe(RefCellDevice::new(&bus));
    // 音量也要经常量写进 codec，所以这里再拿一个 codec 句柄。
    // 和 main 里初始化时用的是同一条 I2C，靠 RefCell 顺序访问，不会冲突。
    let mut codec = Es8311::new(RefCellDevice::new(&bus), board::ES8311_ADDR);
    let mut last_vol = keys::VOLUME.load(core::sync::atomic::Ordering::Relaxed);

    // ---- IMU ----
    let mut imu_ok = false;
    match imu.probe() {
        Some(id) => {
            println!("[imu] @0x{:02X} WHO_AM_I=0x{id:02X}", board::QMI8658_ADDR);
            if imu.whoami_ok() {
                // 初始化前先把寄存器打出来，万一配置没生效能对照着看
                imu.dump_regs();
                match imu.init() {
                    Ok(()) => {
                        println!("[imu] 初始化完成 250Hz ±4g / ±1024dps");
                        imu_ok = true;
                    }
                    Err(_) => println!("[imu] 初始化失败（寄存器写不进去）"),
                }
            } else {
                println!("[imu] WHO_AM_I 不是 0x05，可能不是 QMI8658（跳过）");
            }
        }
        None => println!("[imu] 0x{:02X} 无应答", board::QMI8658_ADDR),
    }

    // ---- 触摸 ----
    if touch.is_present() {
        println!("[touch] {:?} @0x{:02X}", touch.kind(), touch.addr());
    } else {
        println!("[touch] 没探测到触摸芯片（RST=47 可能需要先拉低复位，见 touch.rs）");
    }

    // ---- 轮询 ----
    // IMU 每 8 拍（2 秒）打一行，避免刷屏；触摸有按下就报；温度每 8 拍读一次。
    let mut tick: u32 = 0;
    loop {
        if tick % 8 == 0 {
            // 片上温度。读一次要等 ADC 转换完成，放在这里而不是屏幕任务里，
            // 是因为屏幕任务的重绘节奏不该被传感器时序绑住。
            if let Some(t) = soc_temp::refresh() {
                if tick % 16 == 0 {
                    println!("[temp] 芯片 {}°C", t);
                }
            }
        }

        // 音量变了就写进 codec（按键任务只改原子变量，不直接碰 I2C）
        let vol = keys::VOLUME.load(core::sync::atomic::Ordering::Relaxed);
        if vol != last_vol {
            if codec.set_volume(vol).is_ok() {
                last_vol = vol;
                println!("[vol] 已设为 {vol}");
            }
        }

        if imu_ok {
            if let Ok(m) = imu.read() {
                if tick % 8 == 0 {
                    println!(
                        "[imu] a=({:+.2},{:+.2},{:+.2})g  g=({:+.0},{:+.0},{:+.0})°/s",
                        m.accel_g[0], m.accel_g[1], m.accel_g[2],
                        m.gyro_dps[0], m.gyro_dps[1], m.gyro_dps[2],
                    );
                }
            }
        }
        if let Some((x, y)) = touch.read_point() {
            println!("[touch] 按下 ({x}, {y})");
        }
        tick = tick.wrapping_add(1);
        Timer::after(Duration::from_millis(250)).await;
    }
}

/// 从 PSRAM 分配一块缓冲，失败退回内部堆。
///
/// 网页任务的 socket 缓冲和页面缓冲都走这里。直接放 BSS 的话，
/// 内部 RAM 会被撑爆 —— 链接器报 `cannot move location counter backwards`，
/// 意思是栈的位置被 BSS 顶穿了。S3R8 有 8MB PSRAM，不用白不用。
pub(crate) fn alloc_psram_buf(len: usize) -> &'static mut [u8] {
    unsafe {
        let layout = core::alloc::Layout::from_size_align_unchecked(len, 4);
        let mut p = esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout);
        if p.is_null() {
            p = esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::Internal.into(), layout);
        }
        assert!(!p.is_null(), "网页缓冲分配失败");
        let s = core::slice::from_raw_parts_mut(p, len);
        s.fill(0);
        s
    }
}

/// 转 SSID。esp-radio 1.0 把 SSID 换成了强类型，只能 TryFrom；/// 超 32 字节会失败。这里绝不 unwrap —— no_std 里 panic 就是整机死机，
/// 而输入来自网页表单。
fn to_ssid(s: &str, fallback: &'static str) -> Ssid {
    Ssid::try_from(s).unwrap_or_else(|_| Ssid::try_from(fallback).unwrap())
}

/// 热点配置。名字/密码来自 flash，没配过用出厂值。
fn ap_config(c: &store::Config) -> AccessPointConfig {
    let base = AccessPointConfig::default()
        .with_ssid(to_ssid(
            c.ap_ssid_or_default(),
            store::DEFAULT_AP_SSID,
        ))
        .with_max_connections(4);
    let pass = c.ap_pass_or_default();
    if pass.is_empty() {
        // 开放网络不能带认证，否则 esp_wifi 报参数错误
        base
    } else {
        match Password::try_from(pass) {
            // 用 WPA/WPA2 兼容模式，不要只给 Wpa2Personal：
            // 部分手机/系统对「纯 WPA2」的 AP 兼容差，会直接提示"密码错误"。
            Ok(pw) => base.with_authentication(AuthenticationMethodConfig::WpaWpa2Personal(pw)),
            // 长度不在 8~63：降级成开放网络，总比起不来强
            Err(_) => base,
        }
    }
}

/// STA 配置。空密码必须按 Open 处理，否则 init 直接失败。
fn sta_config(ssid: &str, pass: &str) -> StationConfig {
    let sta = StationConfig::default().with_ssid(to_ssid(ssid, "esp32-idle"));
    if pass.is_empty() {
        sta.with_authentication(AuthenticationMethodConfig::Open)
    } else {
        match Password::try_from(pass) {
            Ok(pw) => sta.with_authentication(AuthenticationMethodConfig::Wpa2Personal(pw)),
            Err(_) => sta.with_authentication(AuthenticationMethodConfig::Open),
        }
    }
}

/// 没配网时用的假 STA：APSTA 模式下 STA 必须有个合法配置，否则 init 失败。
/// 别改成留空，也别指望它会连上。
fn placeholder_sta() -> StationConfig {
    StationConfig::default()
        .with_ssid(to_ssid("esp32-idle", "esp32-idle"))
        .with_authentication(AuthenticationMethodConfig::Open)
}

/// 热点侧 DHCP + 假 DNS，否则手机连上会"无互联网"然后自动断开
#[embassy_executor::task]
async fn dhcp_task(stack: embassy_net::Stack<'static>) {
    static GATEWAYS: [DhcpIp; 1] = [DhcpIp::new(192, 168, 4, 1)];
    static DNS: [DhcpIp; 1] = [DhcpIp::new(192, 168, 4, 1)];

    let config = DhcpServerConfig {
        ip: DhcpIp::new(192, 168, 4, 1),
        lease_time: Duration::from_secs(3600),
        gateways: &GATEWAYS,
        subnet: Some(DhcpIp::new(255, 255, 255, 0)),
        dns: &DNS,
        // 关掉 captive portal：部分手机会因此反复重连/直接放弃。
        // 配网页本来就要用户手动打开 192.168.4.1，不需要自动弹窗。
        use_captive_portal: false,
    };
    let mut leaser = SimpleDhcpLeaser {
        start: DhcpIp::new(192, 168, 4, 2),
        end: DhcpIp::new(192, 168, 4, 10),
        leases: heapless::Vec::new(),
    };

    log::info!("[dhcp] 分配 192.168.4.2 ~ 192.168.4.10");
    if let Err(e) = run_dhcp_server(stack, config, &mut leaser).await {
        log::error!("[dhcp] 退出: {e:?}");
    }
}

// pool_size = 2：AP 和 STA 各有一个 embassy-net runner，各要跑一份
#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

/// 断线重连的外壳。真正的会话在 `run_session` 里。
async fn ws_loop(stack: embassy_net::Stack<'static>) -> ! {
    loop {
        match run_session(stack).await {
            Ok(()) => log::info!("[ws] 会话结束"),
            Err(e) => log::warn!("[ws] 断开: {e}"),
        }
        audio::set_state(State::Idle);
        // 把队列里残留的音频清掉，避免重连后把旧声音发出去
        while MIC_CH.try_receive().is_ok() {}
        while SPK_CH.try_receive().is_ok() {}
        Timer::after(Duration::from_secs(3)).await;
    }
}

async fn run_session(stack: embassy_net::Stack<'static>) -> Result<(), &'static str> {
    // 网关地址和 AI 配置都是运行时从 flash 读的，改配置不用重烧固件
    let cfg = web::snapshot().await;
    let gw_host = cfg.gw_host_or_default();
    let gw_port = cfg.gw_port;

    let ip = ws::parse_ipv4(gw_host).ok_or("网关地址不是合法 IPv4（去网页配置里填）")?;

    let mut host = heapless::String::<32>::new();
    {
        use core::fmt::Write;
        let _ = write!(host, "{gw_host}:{gw_port}");
    }

    log::info!("[ws] 连接 {gw_host}:{gw_port} …");
    let mut c = WsConn::connect(stack, ip, gw_port, "/", DEVICE_NAME, host.as_str()).await?;
    log::info!("[ws] 握手完成");

    // hello：音频格式 + 全部 AI / 语音配置。
    //
    // 把 key 也下发给网关，是为了让「网页改完 key 就能用」——
    // 否则还得去改网关的 .env 再重启。网关那边以这条消息为准，
    // .env 只作兜底（老固件不带 config 字段时用）。
    let mut hello = heapless::String::<1024>::new();
    {
        use core::fmt::Write;
        let _ = write!(
            hello,
            concat!(
                r#"{{"type":"hello","version":2,"format":"pcm","sample_rate":16000,"#,
                r#""channels":1,"bits":16,"frame_ms":{frame_ms},"config":{{"#,
                r#""llm":{{"backend":"{llm_backend}","key":"{llm_key}","model":"{llm_model}","base":"{llm_base}"}},"#,
                r#""asr":{{"chain":"{asr_chain}","key":"{asr_key}","model":"{asr_model}"}},"#,
                r#""tts":{{"chain":"{tts_chain}","voice":"{tts_voice}"}}#,
                r#"}}}}"#
            ),
            llm_backend = cfg.llm_backend_or_default(),
            llm_key = cfg.llm_key.as_str(),
            llm_model = cfg.llm_model_or_default(),
            llm_base = cfg.llm_base_or_default(),
            asr_chain = cfg.asr_chain_or_default(),
            asr_key = cfg.asr_key.as_str(),
            asr_model = cfg.asr_model_or_default(),
            tts_chain = cfg.tts_chain_or_default(),
            tts_voice = cfg.tts_voice_or_default(),
            frame_ms = board::FRAME_MS,
        );
    }
    c.send_text(hello.as_str())
        .await
        .map_err(|_| "hello 发送失败")?;

    let mut down = [0u8; board::FRAME_BYTES];
    let mut text_buf = [0u8; 1024];

    loop {
        // ---- 0. PWR 键请求打断 ----
        // 按键任务已经把本地播放队列清空、状态打回 Idle 了，
        // 但网关那边还在往下推 TTS 数据，不告诉它就会立刻把队列重新灌满。
        if keys::take_abort() {
            c.send_text(r#"{"type":"abort"}"#)
                .await
                .map_err(|_| "abort 发送失败")?;
            // 网关收到 abort 后可能已经又发了几帧，再清一次
            while SPK_CH.try_receive().is_ok() {}
            log::info!("[key] 已通知网关打断");
        }

        // ---- 1. 把录音队列发出去 ----
        // 只有 Listening 状态才上传。语音帧本身带时间顺序，
        // 网关靠连续帧的到达判断"说完了没有"。
        while let Ok(f) = MIC_CH.try_receive() {
            if audio::state() == State::Listening {
                c.send_binary(&f).await.map_err(|_| "上行发送失败")?;
            }
        }

        // ---- 2. 读 20ms ----
        // 超时是正常路径（没有下行数据），不是错误；只有断开才重连
        match c.fill(Duration::from_millis(20)).await {
            Ok(()) | Err(ws::ReadErr::Timeout) => {}
            Err(ws::ReadErr::Closed) => return Err("连接已关闭"),
        }

        // ---- 3. 处理所有完整帧 ----
        loop {
            let head = c.peek_frame();
            let Some((op, start, len, total)) = head else {
                break;
            };

            match op {
                ws::OP_TEXT => {
                    let n = c.read_frame_data(start, len, &mut text_buf);
                    if let Ok(s) = core::str::from_utf8(&text_buf[..n]) {
                        handle_text(&mut c, s).await?;
                    }
                }
                ws::OP_BINARY => {
                    // 下行是单声道 PCM。一帧可能不是 60ms 的整数倍，
                    // 简单起见按收到的块直接排队播放（I2S 会自动补静音）。
                    let n = c.read_frame_data(start, len, &mut down);
                    let mut f = audio::AudioFrame::new();
                    let _ = f.extend_from_slice(&down[..n]);
                    if SPK_CH.try_send(f).is_err() {
                        // 播放队列满说明消费不过来，丢帧保证实时性
                        let _ = SPK_CH.try_receive();
                        let mut f2 = audio::AudioFrame::new();
                        let _ = f2.extend_from_slice(&down[..n]);
                        let _ = SPK_CH.try_send(f2);
                    }
                }
                ws::OP_PING => {
                    let _ = c.send(ws::OP_PONG, &[]).await;
                }
                ws::OP_CLOSE => {
                    c.consume(total);
                    return Err("服务端主动关闭");
                }
                _ => {}
            }
            c.consume(total);
        }
    }
}

/// 处理网关下发的 JSON 控制消息
async fn handle_text(c: &mut WsConn, s: &str) -> Result<(), &'static str> {
    let mut kind = [0u8; 32];
    let Some(ty) = json::get_str(s, "type", &mut kind) else {
        return Ok(());
    };

    match ty {
        "hello" => {
            log::info!("[ws] 网关就绪");
        }
        "ack" => {
            // 网关确认收到唤醒，可以开始正式会话
            audio::set_state(State::Listening);
        }
        "stt" => {
            let mut t = [0u8; 256];
            if let Some(text) = json::get_str(s, "text", &mut t) {
                log::info!("[stt] {text}");
                audio::set_state(State::Thinking);
            }
        }
        "llm" => {
            let mut t = [0u8; 256];
            if let Some(text) = json::get_str(s, "text", &mut t) {
                log::info!("[llm] {text}");
            }
        }
        "tts" => {
            let mut st = [0u8; 16];
            match json::get_str(s, "state", &mut st) {
                Some("start") => {
                    audio::set_state(State::Speaking);
                }
                Some("stop") => {
                    // 等播放队列放完再回待机，否则最后一句会被截断
                    while !SPK_CH.is_empty() {
                        Timer::after(Duration::from_millis(20)).await;
                    }
                    Timer::after(Duration::from_millis(200)).await;
                    audio::set_state(State::Idle);
                }
                _ => {}
            }
        }
        "error" => {
            let mut m = [0u8; 192];
            let msg = json::get_str(s, "msg", &mut m).unwrap_or("未知错误");
            log::error!("[网关] {msg}");
            audio::set_state(State::Idle);
        }
        _ => {}
    }
    let _ = c;
    Ok(())
}
