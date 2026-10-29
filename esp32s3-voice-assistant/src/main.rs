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
mod dhcp;
mod display;
mod es7210;
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
use embassy_time::{Duration, Instant, Timer};
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
    log::info!("\n[boot] firmware start");

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
    //
    // 【试过提到 64KB，已回退】
    // 前提是把 `ws::mk_static_buf` 的 16KB TCP 缓冲搬去 PSRAM 腾地方，
    // 结果设备变成「能连 WiFi、能 DHCP，但连网关 8765 一直超时」——
    // 而对照项目 esp32s3-wifi-min（缓冲在内部 RAM）一次就通。
    // 所以缓冲留在内部 RAM、堆保持 48KB，总内部堆 112KB。
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    // 48KB → 64KB：那 16KB 是 ws 的 TCP 缓冲搬去 PSRAM 腾出来的，
    // 加在堆上而不是留给栈（栈当前 53KB 够用，缺的是 WiFi 动态缓冲）。
    esp_alloc::heap_allocator!(size: 64 * 1024);
    log::info!("[boot] heap ok");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CPU_CLOCK));
    log::info!("[boot] hal ok");

    // PSRAM 挂进全局堆（esp-radio 内部走 malloc_internal，拿不到 PSRAM）
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);
    log::error!(
        "[boot] psram ok, {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::External.into())
    );

    // rtos 必须跑在任何 await 之前：Timer、任务调度、软件中断都靠它。
    // 原来这段在第 130 行（在 load_into_memory().await 之后），属于顺序错误。
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    log::info!("[boot] rtos ok");

    // ---------------- 0. 从 flash 读配置 ----------------
    // WiFi 账号、网关地址、各家 API key 全在这里。没配过就是空的，
    // 下面会进配网模式，只开热点等手机来填。
    web::load_into_memory(FlashStorage::new(peripherals.FLASH).multicore_auto_park()).await;
    let boot_cfg = web::snapshot().await;
    log::error!(
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

    log::info!("\n=== ESP32-S3 语音助手 ===");
    log::error!(
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

    // ========================================================================
    // 增量验证开关
    //
    // 排查方法：基线（全部 false）只跑「WiFi → DHCP → WebSocket」这一条链路，
    // 确认设备能连上网关之后，**每次只把一个开关改成 true**，重新烧写验证。
    // 一旦某个开关打开后就连不上了，那个模块就是干扰源。
    //
    // 别一次开多个 —— 之前就是到处同时改，改到最后完全不知道是谁的锅。
    // ========================================================================
    const EN_I2C: bool = true; // I2C 总线 + ES8311 / ES7210 初始化
    const EN_SENSORS: bool = false; // IMU + 触摸轮询（依赖 I2C）
    const EN_KEYS: bool = true; // 按键扫描
    const EN_DISPLAY: bool = true; // ST7789 屏幕
    const EN_AUDIO: bool = true; // I2S 全双工 + 录音/播放任务
    const EN_WEB: bool = true; // 网页配置页（监听 80）
    const EN_SDCARD: bool = true; // TF 卡挂载

    // ---------------- 1. I2C 总线：codec / IMU / 触摸 三家共用 ----------------
    // 这块板子的 ES8311、QMI8658、触摸芯片的 SDA/SCL 全是 GPIO42/GPIO41，
    // 物理上就是同一条总线。所以这里**只建一个 I2c 实例**，
    // 后面用 RefCellDevice 派生出多个设备句柄。
    let i2c_bus = if EN_I2C || EN_SENSORS {
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
        Some(RefCell::new(i2c))
    } else {
        None
    };

    // ---------------- 1.1 扫描总线 ----------------
    // 这行日志是换板子后**最有价值的一条**：0x18 出现说明 codec 线对了，
    // 0x6B/0x6A 出现说明 IMU 在，剩下的一个地址就是触摸芯片的型号。
    if let Some(i2c_bus) = i2c_bus.as_ref() {
        let mut dev = RefCellDevice::new(i2c_bus);
        let found = sensors::scan(&mut dev);
        log::info!("[i2c] 扫描到 {} 个设备 @ SDA={} SCL={}",
            found.len(), board::PIN_I2C_SDA, board::PIN_I2C_SCL);
        if found.is_empty() {
            log::info!("[i2c] 一个都没有！SDA/SCL 接反或没有上拉（board.rs 第 6 节）");
        }
        for &addr in found.iter() {
            log::info!("  [i2c] 0x{addr:02X}  {}", sensors::addr_name(addr));
        }
    }

    // ---------------- 1.2 ES8311 codec ----------------
    // 先配 codec 再启 I2S：寄存器写入走 I2C，和 MCLK 无关，
    // 反过来（先启 I2S 再配 codec）容易出现首帧丢字。
    //
    // 【必须重试 —— 这是「偶尔搭理你、大多数时候不理人」的根源之一】
    //
    // 2026-10-28 晚实测：同一份固件连烧两次，启动结果完全不同 ——
    //   第 1 次：ES8311 在线 CHIPID=0x83，ES7210 init 成功 → 麦克风出信号 L=570
    //   第 2 次：读不到 ES8311，ES7210 init 失败 → 麦克风彻底没声音
    // 而且两次都没有任何「错误」可看，只是静默地不工作。
    // 所以 probe 和 init 都改成重试 3 次，ES7210 的 init 内部还带回读校验。
    if let Some(bus) = i2c_bus.as_ref().filter(|_| EN_I2C) {
        let _ = &i2c_bus;
        let delay = esp_hal::delay::Delay::new();
        let mut es8311_ok = false;
        for attempt in 1..=3u8 {
            let mut codec = Es8311::new(RefCellDevice::new(bus), board::ES8311_ADDR);
            match codec.probe() {
                Some(id) => match codec.init() {
                    Ok(()) => {
                        log::info!("[codec] ES8311 就绪（第 {attempt} 次，CHIPID=0x{id:02X}）");
                        let _ = codec.set_volume(85);
                        es8311_ok = true;
                        break;
                    }
                    Err(step) => {
                        log::warn!("[codec] ES8311 初始化失败（第 {attempt} 次）：{step}")
                    }
                },
                None => log::info!("[codec] 读不到 ES8311（第 {attempt} 次，检查 I2C 引脚）"),
            }
            delay.delay_millis(100);
        }
        if !es8311_ok {
            log::error!("[codec] ES8311 最终失败 —— 喇叭不会有声音");
        }

        // ---------------- 1.3 ES7210 麦克风 ADC ----------------
        //
        // 【这块板子有两颗 codec】ES8311 只管喇叭，麦克风走 ES7210（I2C 0x40）。
        // 之前只初始化了 ES8311，所以 I2C 扫描里那个 "0x40 未知设备" 一直没被
        // 用起来 —— 后果是 ADC 不出数据，I2S RX 全是静音：
        // 唤醒词永不命中、ASR 永远收到空音频。
        let mut mic_ok = false;
        let mut last_err = "未尝试";
        for attempt in 1..=3u8 {
            let mut mic = es7210::Es7210::new(RefCellDevice::new(bus), es7210::ADDR);
            match mic.probe() {
                Some(v) => match mic.init() {
                    Ok(()) => {
                        log::error!(
                            "[codec] ES7210 就绪（第 {attempt} 次，reg0=0x{v:02X}）16kHz/16bit/I2S"
                        );
                        mic_ok = true;
                        break;
                    }
                    Err(step) => {
                        last_err = step;
                        log::warn!("[codec] ES7210 初始化失败（第 {attempt} 次）：{step}");
                    }
                },
                None => {
                    last_err = "I2C 探测不到 0x40";
                    log::info!("[codec] 读不到 ES7210（第 {attempt} 次）");
                }
            }
            delay.delay_millis(100);
        }
        if !mic_ok {
            log::error!("[codec] ES7210 最终失败：{last_err} —— 麦克风不会有声音，ASR 必然空结果");
        }
    }

    // ---------------- 1.3 IMU + 触摸（同一个任务轮询）----------------
    if EN_SENSORS {
        if let Some(bus) = i2c_bus {
            spawner.spawn(sensors_task(bus).expect("sensors_task"));
        }
    } else {
        let _ = i2c_bus;
    }

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

    // 【音频任务延后到 WiFi 连上之后再 spawn】
    //
    // 原因（今天实机实测出来的）：`mic_task` 首帧就跑 esp-sr 初始化，
    // AEC 要 ~122KiB 内部 RAM，VADNet/WakeNet 还要各自几十 KB。
    // 而内部堆总共只有 112KB（BSS 48KB + reclaimed 64KB），
    // WiFi 驱动又只能用内部 RAM（malloc_internal）。
    //
    // 结果：esp-sr 先把内部 RAM 吃干，WiFi 在四次握手阶段分配不出加密缓冲，
    // 稳定报 reason=15 FourWayHandshakeTimeout —— 而且**正确密码和错误密码
    // 报的是同一个错**（当时误判成驱动 bug，其实一个字节都没进到握手）。
    // 临时关掉 esp-sr 后 WiFi 立刻连上、拿到 192.168.88.149，根因确认。
    //
    // 所以顺序改成：先建 WiFi → 连上并拿到 IP → 再启动音频任务，
    // 让 WiFi 优先拿到它需要的内部 RAM。

    // 三个板载按键：PWR 打断 / PLUS/MINUS 音量
    if EN_KEYS {
        spawner.spawn(keys::keys_task().expect("keys_task"));
    }

    // ---------------- 3. 屏幕（Wi-Fi 之前就点上）----------------
    // 放在这里而不是 Wi-Fi 之后，是为了开机画面能尽早出现：
    // 连 Wi-Fi 通常要 2~5 秒，屏幕要是等连上再初始化，这段时间就是黑屏。
    if EN_DISPLAY {
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
    } else {
        // 屏不点也要把 PA 打开，否则喇叭没声（PA_EN 在前面已拉高）
        log::info!("[boot] 屏幕已跳过（EN_DISPLAY=false）");
    }

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
    log::error!(
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
    // 建 WiFi 之前先把内部堆剩多少打出来 —— WiFi 只能用内部 RAM
    log::error!(
        "[mem] 建 WiFi 前：内部 {} B / PSRAM {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into()),
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::External.into()),
    );

    let ap_cfg = ap_config(&boot_cfg);
    let sta_cfg = if boot_cfg.wifi_set() {
        sta_config(boot_cfg.wifi_ssid.as_str(), boot_cfg.wifi_pass.as_str())
    } else {
        placeholder_sta()
    };

    // 【有配置时只开 STA，不开热点】
    //
    // 实测：APSTA 并存时连家里 WiFi 一直报
    //   FourWayHandshakeTimeout（信号 -27dBm，密码确认无误）
    // 怀疑是射频被热点打断 —— 四次握手要求毫秒级及时性，而热点每 100ms 要发
    // beacon，beta 版 coex 调度可能挤掉握手窗口。
    // 配网完成后热点本来就没用了（配置在 flash 里，网页走局域网 IP 也能开），
    // 所以有配置就只开 STA。
    // 实验结论：APSTA 并存**不是**握手失败的原因（纯 STA 也一样失败）。
    // 所以热点保持常开 —— 它是连不上家里 WiFi 时唯一能进去改配置的通道。
    // 【2026-10-28 从硬编码 true 改为按配置判断】
    //
    // 原来这里是 `let ap_enabled = true;`，于是设备**永远**跑在 APSTA 模式：
    // 一边开热点「ESP32-VOICE」，一边连 CPE。而 ESP32 的 AP 和 STA 共用同一
    // 套射频前端，只能分时切换信道 —— 结果就是：
    //
    //   · 关联能成功（信标/认证帧很小，抢得到时隙）
    //   · DHCP 能成功（广播帧每个 DTIM 都发）
    //   · **但 ICMP echo / TCP SYN-ACK 这类单播包大量丢失**
    //     表现：ping 100% 丢包、网关停在 SynReceived、TCP 建不起来
    //
    // 配网只在「还没配过 WiFi」时才需要热点；一旦 WiFi 配置存在，就该
    // 纯 STA 跑，把整个射频让给联网。这也正是下面那段注释本来写明的意图
    // （「只有没配网时才建。有配置就纯 STA」），只是判断条件一直没接上。
    let ap_enabled = !boot_cfg.wifi_set();
    let initial = if ap_enabled {
        WifiConfig::AccessPointStation(sta_cfg, ap_cfg)
    } else {
        WifiConfig::Station(sta_cfg)
    };

    // 【WiFi 缓冲预算 —— 内部 RAM 只有 112KB，默认配置的峰值要 118KB】
    //
    // esp-radio 的 `ControllerConfig::default()` 是这么配的：
    //   static_rx_buf_num  = 10  → 10 × 1.6KB = 16KB，esp_wifi_init 时分配，常驻不释放
    //   dynamic_rx_buf_num = 32  → 峰值 51KB
    //   dynamic_tx_buf_num = 32  → 峰值 51KB
    //   ampdu_tx_enable    = true → 聚合还要额外 TX 缓冲
    // 合计峰值 118KB，而这块板的内部堆总共只有 112KB。
    //
    // 实测症状完全对得上：刚连上时动态缓冲还没撑开，能跑；一旦开始上传音频
    // （32KB/s），缓冲被吃满 → 分配失败 → 发不出包 → TCP 静默断（约 15 秒）→
    // 反复重连后连 connect_async 里要的 static_rx 都分不出来，直接卡死不返回。
    // 这就是「老不理人」在内存层的真相。
    //
    // 【2026-10-28 晚 定案：WiFi 缓冲**必须用驱动默认值，一个都不要改**】
    //
    // 我曾把 static_rx 10→6、dynamic_rx 32→16 来省内存，理由是算出来峰值要
    // 118KB > 112KB 堆。但后来做了个对照实验（新建 esp32s3-wifi-min，
    // 只有 WiFi + DHCP + 访问百度，其余全砍）：
    //
    //   默认配置 + 128KB 堆 → DNS 通、TCP 连百度 80 连续 9 次全通，
    //   内部堆稳定在 83KB，WiFi 实际只占 45KB。
    //
    // 也就是说「118KB 峰值」只是理论最大值，实际根本用不到，内存压根不缺。
    // 而收紧缓冲之后，设备反而变成「能关联、能 DHCP、但连 ARP 都发不出去」
    // （PC 邻居表里 MAC 恒为 00-00-00-00-00-00）—— 说明 esp-radio beta 的
    // TX 路径对这个参数敏感，动了就废。教训：这类底层缓冲别自作主张优化。
    let ctrl_cfg = ControllerConfig::default().with_initial_config(initial);

    let mut controller = match esp_radio::wifi::WifiController::new(peripherals.WIFI, ctrl_cfg) {
        Ok(c) => c,
        Err(e) => {
            log::error!("[wifi] 初始化失败: {e:?}");
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
        log::info!("[wifi] 设置发射功率失败: {e:?}");
    } else {
        log::info!("[wifi] 发射功率已提到 19.5dBm");
    }

    // 【2026-10-28 晚 定案：不要关 modem sleep】
    //
    // 上一轮关掉它，设备立刻变成「能关联、能 DHCP、但 ARP reply 发不出去」，
    // PC 邻居表里 MAC 恒为 00-00-00-00-00-00。默认（省电开着）反而是好的 ——
    // 对照实验里默认配置能连百度 80 端口连续 9 次全通。
    //
    // 原作者也在这里留过同样的结论（「关掉之后更糟，先恢复默认」），
    // 我不该在没做对照实验的情况下第二次踩进去。



    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let seed2 = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A;

    // ---- 热点侧：自己做网关，地址固定 192.168.4.1 ----
    // 只有没配网时才建。有配置就纯 STA，射频全给联网用。
    let mut ap_stack_opt: Option<embassy_net::Stack<'static>> = None;
    if ap_enabled {
        let (ap_stack, ap_runner) = embassy_net::new(
            Interface::access_point(),
            NetConfig::ipv4_static(StaticConfigV4 {
                address: Ipv4Cidr::new(store::AP_IP, 24),
                gateway: None,
                dns_servers: heapless::Vec::new(),
            }),
            mk_static!(StackResources<10>, StackResources::<10>::new()),
            seed,
        );
        spawner.spawn(net_task(ap_runner).expect("ap net task"));
        ap_stack_opt = Some(ap_stack);
    }

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
    if let Some(ap_stack) = ap_stack_opt {
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
        // 热点侧必须自带 DHCP，否则手机连上会"无互联网"然后自动断开
        spawner.spawn(dhcp::dhcp_task(ap_stack).expect("dhcp"));
    }
    // 【临时禁用：排查 DHCP 卡住】
    // 现象是卡在 `[trace] 等 DHCP 配置…`，而对照项目 esp32s3-wifi-min
    // （没有 web_task）同样的 wait_config_up() 一秒就过。
    // 先关掉配置页，确认它是不是元凶。
    if EN_WEB {
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
    }


    if !ap_enabled {
        log::info!("[wifi] 已配网，只开 STA（热点关闭）");
    } else {
    log::error!(
        "[wifi] 热点已开：{} / {}  →  http://{}",
        boot_cfg.ap_ssid_or_default(),
        if boot_cfg.ap_pass_or_default().is_empty() {
            "开放"
        } else {
            boot_cfg.ap_pass_or_default()
        },
        store::AP_IP,
    );
    }

    // ---- 连家里 WiFi ----
    //
    // 【为什么不用 select(connect_async(), Timer)】
    // 实测这块板子上 `select` + `Timer` 根本不会唤醒 —— 表现是日志停在
    // "连接 CPE（第 N 次）" 之后再无输出，超时分支永远不触发。
    // 显示页早先也踩过同一个坑（select(Signal.wait(), Timer) 永不返回）。
    //
    // 所以改成：**连接交给独立任务，主流程只用普通 Timer 轮询一个原子状态**。
    // 普通 Timer::after 在这块板子上是好的（配网页 30 秒等待就是用它）。
    let mut connected = false;
    if boot_cfg.wifi_set() {
        let ssid_for_task = boot_cfg.wifi_ssid.clone();
        spawner.spawn(
            wifi_connect_task(controller, ssid_for_task).expect("wifi_connect_task"),
        );

        // 最多等 45 秒（5 次 × 每次约 8 秒）
        for _ in 0..225 {
            match WIFI_STATE.load(core::sync::atomic::Ordering::Relaxed) {
                1 => {
                    connected = true;
                    break;
                }
                2 => break,
                _ => Timer::after(Duration::from_millis(200)).await,
            }
        }
        // 用 error 级别：这是启动成败的关键节点，必须无条件可见
        // （ESP_LOG=warn 时 info 不输出，之前的排查就卡在「看不到走到哪」上）
        log::error!(
            "[trace] WiFi 连接阶段结束：{}",
            if connected { "已连上" } else { "未连上" }
        );
    } else {
        // 没配网时热点必须常开，所以别 drop 控制器
        core::mem::forget(controller);
    }

    // 没配网 / 连不上：进配网模式。热点、网页、屏幕都照常工作，
    // 只是不去连网关 —— 用户改完配置保存后设备会自己重启。
    if !connected {
        log::info!("[wifi] 进入配网模式，屏幕显示热点地址");
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

    log::error!("[trace] 等 DHCP 配置…");
    // 【不用 stack.wait_config_up() —— 它的唤醒在这块板子上不可靠】
    //
    // 实测它在这里永远不返回（串口停在上一行），而对照项目 esp32s3-wifi-min
    // 用同一个 API 却一秒就过。与其依赖它的内部 waker，不如像主流程等
    // WIFI_STATE 那样**主动轮询** —— 这条路径本项目已经验证是可靠的。
    // 顺便每 2 秒打一次进度，能区分「DHCP 还没好」和「DHCP 彻底失败」。
    {
        let mut waited = 0u32;
        loop {
            if stack.config_v4().is_some() {
                break;
            }
            if waited >= 300 {
                // 30 秒还没拿到地址：打印诊断后继续往下走（不阻塞启动）
                log::error!(
                    "[trace] DHCP 超时 30s：link_up={} 仍无 IP",
                    stack.is_link_up()
                );
                break;
            }
            if waited % 20 == 0 {
                log::error!(
                    "[trace] 等 DHCP… {}s  link_up={} ip={:?}",
                    waited / 10,
                    stack.is_link_up(),
                    stack.config_v4().map(|c| c.address)
                );
            }
            Timer::after(Duration::from_millis(100)).await;
            waited += 1;
        }
    }
    log::error!("[trace] DHCP 阶段结束，IP={:?}", stack.config_v4().map(|c| c.address));
    let mut ip_text: heapless::String<16> = heapless::String::new();
    if let Some(cfg) = stack.config_v4() {
        log::info!("[wifi] IP = {}", cfg.address);
        use core::fmt::Write;
        let _ = write!(ip_text, "{}", cfg.address);
    }

    // WiFi 已拿到 IP。esp-sr 的大块缓冲现在会被 esp-alloc 的补丁
    // 直接分到 PSRAM（见 vendor/esp-alloc/src/malloc.rs），
    // 所以这里可以放心启动音频任务，内部 RAM 留给 WiFi/TCP。
    if EN_AUDIO {
        spawner.spawn(audio::speaker_task(i2s_tx).expect("speaker_task"));
        spawner.spawn(audio::mic_task(i2s_rx).expect("mic_task"));
    } else {
        let _ = i2s_tx;
        let _ = i2s_rx;
    }
    log::error!("[trace] 音频任务：{}", if EN_AUDIO { "已启动" } else { "已跳过" });

    // ---------------- 4.4 出站连通性自检 ----------------
    //
    // 【出站自检已移除】
    // 曾在这里做 DNS 解析百度 + 连 80 端口的测试，用来区分「TX 坏了」和
    // 「AP 客户端隔离」。结论已经拿到了（见 esp32s3-wifi-min 对照项目）：
    // 外网通、到 PC 的 8765 也通，所以射频和局域网都没问题。
    //
    // 而这段 `dns_query` 在本项目里会**卡住不返回**（串口停在「外网自检开始」），
    // 因为 DNS 依赖 embassy 的定时器来判超时，而这里的定时器在 audio 任务
    // 起来之后就不太灵。它只是诊断代码，不该挡住主流程，删掉。
    // 需要再验证网络时，用独立的 esp32s3-wifi-min。

    // 通知屏幕：IP 到手了，可以从开机画面切到状态页
    display::IP_READY.signal(ip_text);

    // ---------------- 4.5 TF 卡 ----------------
    // SD 卡用 SPI 兼容模式（CLK16/CMD15/D0_17/D3_14），原因见 sdcard.rs 顶部。
    // 挂卡是阻塞的，没插卡时约 500ms 后放弃 —— 属于可选外设，失败不影响语音链路。
    if EN_SDCARD {
        match sdcard::SdStorage::mount(
            peripherals.SPI3,
            peripherals.GPIO16,
            peripherals.GPIO15,
            peripherals.GPIO17,
            peripherals.GPIO14,
        ) {
            Ok(card) if card.blocks() > 0 => {
                log::info!("[sd] 就绪 {} MB（{} 块）", card.bytes() / 1024 / 1024, card.blocks())
            }
            Ok(_) => log::info!("[sd] 没插卡，或卡不认（格式化成 FAT32 再试）"),
            Err(e) => log::info!("[sd] 挂载失败: {e}"),
        }
    }

    // ---------------- 5. WebSocket 主循环 ----------------
    log::error!("[trace] 进入 ws_loop");
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
            log::info!("[imu] @0x{:02X} WHO_AM_I=0x{id:02X}", board::QMI8658_ADDR);
            if imu.whoami_ok() {
                // 初始化前先把寄存器打出来，万一配置没生效能对照着看
                imu.dump_regs();
                match imu.init() {
                    Ok(()) => {
                        log::info!("[imu] 初始化完成 250Hz ±4g / ±1024dps");
                        imu_ok = true;
                    }
                    Err(_) => log::info!("[imu] 初始化失败（寄存器写不进去）"),
                }
            } else {
                log::info!("[imu] WHO_AM_I 不是 0x05，可能不是 QMI8658（跳过）");
            }
        }
        None => log::info!("[imu] 0x{:02X} 无应答", board::QMI8658_ADDR),
    }

    // ---- 触摸 ----
    if touch.is_present() {
        log::info!("[touch] {:?} @0x{:02X}", touch.kind(), touch.addr());
    } else {
        log::info!("[touch] 没探测到触摸芯片（RST=47 可能需要先拉低复位，见 touch.rs）");
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
                    log::info!("[temp] 芯片 {}°C", t);
                }
            }
        }

        // 音量变了就写进 codec（按键任务只改原子变量，不直接碰 I2C）
        let vol = keys::VOLUME.load(core::sync::atomic::Ordering::Relaxed);
        if vol != last_vol {
            if codec.set_volume(vol).is_ok() {
                last_vol = vol;
                log::info!("[vol] 已设为 {vol}");
            }
        }

        if imu_ok {
            if let Ok(m) = imu.read() {
                if tick % 8 == 0 {
                    // 【必须是 info，不能是 error】
                    // 这是**周期性**日志（每 2 秒一条）。一旦提到 error 级，
                    // ESP_LOG=error 也挡不住它，电脑不读串口时 FIFO 几秒就满，
                    // 整个设备被自己的日志堵死（ping 不通、TCP 全挂）。
                    // 只有一次性/致命的日志才配用 error。
                    log::info!(
                        "[imu] a=({:+.2},{:+.2},{:+.2})g  g=({:+.0},{:+.0},{:+.0})°/s",
                        m.accel_g[0], m.accel_g[1], m.accel_g[2],
                        m.gyro_dps[0], m.gyro_dps[1], m.gyro_dps[2],
                    );
                }
            }
        }
        if let Some((x, y)) = touch.read_point() {
            log::info!("[touch] 按下 ({x}, {y})");
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
    // 【配网热点用开放网络】
    //
    // 实测：带 WPA2 密码时，手机能搜到热点但一律提示"密码错误"——
    // 密码本身经日志核对是完全正确的（9 字节 ASCII），
    // 且同一个 esp-radio 版本在另一块板子上是能连的。
    // 所以问题在 esp-radio 1.0.0-beta.1 的 SoftAP 加密实现。
    //
    // 配网阶段用 Open 是常见做法：热点只活到配网完成，配完就切正常模式。
    // 真要加密，等 esp-radio 出正式版再说。
    AccessPointConfig::default()
        .with_ssid(to_ssid(
            c.ap_ssid_or_default(),
            store::DEFAULT_AP_SSID,
        ))
        .with_max_connections(4)
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

// pool_size = 2：AP 和 STA 各有一个 embassy-net runner，各要跑一份
/// 0 = 连接中，1 = 已连上，2 = 失败
static WIFI_STATE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// STA 连接任务。
///
/// `connect_async()` 内部等「连上 / 断开」事件，事件不来就永久挂住，所以必须有
/// 人在外面管着。这里由任务自己重试，主流程只读 `WIFI_STATE`。
///
/// 注意：`WifiController` 一旦 drop 就会停掉射频，所以任务结尾必须常驻，
/// 不能直接返回。
#[embassy_executor::task]
async fn wifi_connect_task(
    mut controller: esp_radio::wifi::WifiController<'static>,
    ssid: heapless::String<32>,
) {
    // 【扫描整段删掉了 —— 这是「设备卡在启动、ping 不通」的头号元凶】
    //
    // 原来这里先 `scan_async()` 扫一遍 AP，只为把目标 AP 的安全类型打出来
    // （排查 FourWayHandshakeTimeout 用）。但它有个致命问题：
    //
    //   2026-10-28 晚实测，烧写后设备停在「扫描周边 AP…」这一行，
    //   75 秒没有任何后续输出、ping 也不通、web 配置页也连不上，
    //   但 USB-JTAG 还能烧写（芯片根本没死）。**外面套 with_timeout 也无效** ——
    //   说明 scan_async 内部是阻塞实现（不交还执行器），定时器压根没机会触发。
    //
    // 而扫描的价值仅仅是「打印调试信息」，不该拿整个启动流程去赌。
    // 需要确认 AP 的安全类型时，用手机看，或者临时把这段加回来。
    //
    // 下面这个连接循环是 5 次重试，失败会打 reason，
    // 足够定位「密码不对 / WPA3 / 信号弱」这几类问题。

    for attempt in 1..=5u8 {
        log::error!("[wifi] 连接 {}（第 {attempt} 次）…", ssid.as_str());
        // 【绝对不要给 connect_async 套 with_timeout】
        //
        // 2026-10-28 晚踩过：`with_timeout` 内部是 `select(future, Timer)`，
        // 而**这块板子上 select + Timer 根本不会唤醒**（见上面主流程那段注释，
        // 显示页和这里都验证过同一件事）。套上去之后表现是连接过程永远不返回，
        // 设备停在「连接 CPE（第 1 次）」再无下文 —— 比不套还糟。
        //
        // 兜底不在这里：主流程只轮询 WIFI_STATE（最多 45 秒），
        // 连不上就进配网模式，不会把设备憋死。
        match controller.connect_async().await {
            Ok(info) => {
                log::error!(
                    "[wifi] 已连上，信道 {}，认证 {:?}，AID {}",
                    info.channel, info.authmode, info.aid
                );
                WIFI_STATE.store(1, core::sync::atomic::Ordering::Relaxed);
                break;
            }
            Err(e) => {
                // 常见 reason：
                //   FourWayHandshakeTimeout → 密码不对，或 AP 要求 PMF/WPA3
                //   NoApFound / AuthenticationExpired → 信号/距离问题
                log::error!("[wifi] 第 {attempt} 次失败: {e:?}");
            }
        }
        Timer::after(Duration::from_secs(2)).await;
    }

    if WIFI_STATE.load(core::sync::atomic::Ordering::Relaxed) != 1 {
        WIFI_STATE.store(2, core::sync::atomic::Ordering::Relaxed);
        log::info!("[wifi] 5 次都失败，放弃重试");
    }

    // 保住控制器：drop 会把 WiFi 停掉
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}

#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

/// 断线重连的外壳。真正的会话在 `run_session` 里。
async fn ws_loop(stack: embassy_net::Stack<'static>) -> ! {
    // 【重连日志必须节流 —— 这是第二个「设备自己把自己憋死」的坑】
    //
    // 网关没开的时候，这个循环每 3 秒跑一轮，原本每轮打 2 条日志
    // （[net] 状态 + [ws] 断开）。esp-println 走 USB-Serial-JTAG，主机端不读
    // 就会阻塞，实测两三分钟设备就 ping 不通了 —— 看起来像「WiFi 掉线」，
    // 其实射频好好的，只是所有打印的任务都堵在 write 上。
    //
    // 现在：首次失败前 3 次照常打（方便看失败原因），之后每 20 次（1 分钟）
    // 才打一条。最坏情况一分钟一条，绝不会把 FIFO 憋爆。
    let mut fails: u32 = 0;
    loop {
        let quiet = fails > 3 && fails % 20 != 0;
        if !quiet {
            // 排查「WiFi 明明连上了却连不到网关」时，这三项能直接区分：
            //   link_up=false        → 射频掉线（很可能被 esp-sr 抢走内部 RAM）
            //   link_up=true 有 IP   → 网是通的，问题在对端/防火墙/隔离
            //   内部堆接近 0          → 后续分配都会失败
            log::error!(
                "[net] link_up={} ip={:?} 内部堆={}B",
                stack.is_link_up(),
                stack.config_v4().map(|c| c.address),
                esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into()),
            );
        }
        match run_session(stack).await {
            Ok(()) => {
                log::info!("[ws] 会话结束");
                fails = 0;
            }
            Err(e) => {
                fails = fails.saturating_add(1);
                if fails <= 3 || fails % 20 == 0 {
                    log::error!("[ws] 断开第 {fails} 次: {e}");
                }
                // 【自愈兜底：连续 20 次（约 1 分钟）连不上就软件复位】
                //
                // 实测反复重连之后，设备会出现「ping 通、但任何 TCP 都连不上」
                // 的状态（web 配置页 80 端口、网关 8765 全部超时），只有断电
                // 重启才恢复。看起来是 embassy-net 的 TCP socket 在异常断开后
                // 没有完全释放，池子里的 8 个位置被慢慢耗光 —— ICMP 不走 socket，
                // 所以 ping 照样通，特别有迷惑性。
                //
                // 与其让它一直哑着，不如自己重启。1 分钟才触发一次，不会抖。
                if fails >= 60 {
                    log::error!("[ws] 连续 {fails} 次连不上网关，软件复位重来");
                    Timer::after(Duration::from_millis(200)).await;
                    esp_hal::system::software_reset();
                }
            }
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

    log::error!("[ws] 连接 {gw_host}:{gw_port} …");
    let mut c = WsConn::connect(stack, ip, gw_port, "/", DEVICE_NAME, host.as_str()).await?;
    // 降到 debug：重连时每轮都会打一条，info 级别会在网关没开时刷爆 USB-JTAG
    log::debug!("[ws] 握手完成");

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
                r#""tts":{{"chain":"{tts_chain}","voice":"{tts_voice}"}}"#,
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
    // 应用层心跳的最后发送时刻
    let mut last_hb = Instant::now();
    // 最后一次**收到**下行数据的时刻。
    //
    // 【为什么必须有】网关被强杀（Stop-Process）时不会发 TCP FIN，设备这边
    // 的 socket 毫不知情，往发送缓冲里继续写还以为一切正常 —— 实测要等
    // smoltcp 的 socket timeout（现在 120 秒）才会报错重连。用户看到的就是
    // 「网关重启了一下，设备哑了两分钟」。
    //
    // 网关现在每 10 秒发一个 {"type":"ping"}，只要 30 秒什么都没收到，
    // 就认定这条连接死了，主动断开重连（3 秒后重连成功）。
    let mut last_rx = Instant::now();

    loop {
        // ---- 0. 应用层心跳 ----
        //
        // 设备在待机（Idle）状态下**一个字节都不往外发**，整条 TCP 连接是
        // 纯空闲的，很容易被 NAT / socket timeout 回收，所以要有心跳。
        //
        // 【关键】只在**不上传音频**的时候发。正在 Listening 上传时本来就
        // 有持续流量，不需要心跳；这时候插一条心跳进去反而有害 ——
        // TCP 发送缓冲只有几 KB，音频一挤就满，心跳的 write_all 就会阻塞，
        // 把整个发送任务卡死。实测现象：设备传了整整 8 秒（一个心跳周期）
        // 之后突然一条数据都不再发，也不报错。
        if audio::state() != audio::State::Listening
            && last_hb.elapsed() >= Duration::from_secs(8)
        {
            c.send_text(r#"{"type":"hb"}"#)
                .await
                .map_err(|_| "心跳发送失败")?;
            last_hb = Instant::now();
        }

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
        //
        // 【不要再做「多帧合并成一批」的优化】
        //
        // 试过在栈上开 `[0u8; FRAME_BYTES * 2]` 把两帧合成一个 WS 消息发，
        // 结果设备启动跑一阵就彻底卡死、串口零输出 —— ws_task 的栈装不下
        // 这 2KB 的数组，直接栈溢出。embassy 任务的栈是静态分配的，
        // 想加大得改 task 宏的 pool_size，不值得为一个发送次数的小优化去冒这个险。
        //
        // 只有 Listening 状态才上传；其它状态把队列清空，免得重连后发旧声音。
        //
        // 【每轮最多发 8 帧，且塞不进就丢】
        //
        // 原来是无上限的 `while let Ok(f)` + 无条件 `send_binary().await`。
        // 网关慢一下（重建后端、模型冷启动）发送缓冲就满，write_all 死等，
        // 整个循环卡住 —— 心跳发不出去、下行也读不了，设备表现成
        // 「刚连上 1 秒就掉线，然后疯狂重连」。实测网关收到 hello 后会重建
        // ASR/TTS（SenseVoice 1.5s + Piper 4.5s），这 6 秒正好踩中。
        // 现在：发之前先问 socket 还能不能写，不能写就丢帧；每轮最多 8 帧，
        // 保证每 256ms 一定回到读 socket 的分支，心跳和 ping 都不会饿死。
        let mut sent_this_round = 0u8;
        while let Ok(f) = MIC_CH.try_receive() {
            if audio::state() == State::Listening {
                if !c.can_send() {
                    continue; // 发送缓冲满：丢这帧，别死等
                }
                c.send_binary(&f).await.map_err(|_| "上行发送失败")?;
                sent_this_round += 1;
                if sent_this_round >= 8 {
                    break;
                }
            }
        }

        // ---- 2. 读 20ms ----
        // 超时是正常路径（没有下行数据），不是错误；只有断开才重连
        // 【下行背压：播放队列快满时，先别读 socket】
        //
        // 这是治「啵啵」的关键。原来是不管三七二十一，每 20ms 读一次 socket，
        // 读到音频就往 SPK_CH 里塞，塞不下就丢 —— 而网关是按自己的节奏推的
        // （Windows 的 asyncio 定时器颗粒度 15.6ms，sleep(32ms) 实际落在
        // 31~47ms 之间抖），于是设备一会儿被灌爆（丢帧）、一会儿被抽干（补静音）。
        // 实测一轮 112 帧里：丢帧 14、空帧 54 —— 一半的帧都是补的静音。
        // 每个「有声→静音→有声」的接缝就是一声「啵」。
        //
        // 现在队列有 24 帧（768ms），快满时干脆**不读**：数据留在 TCP 接收缓冲里，
        // 窗口缩到 0，网关的 ws.send 自然就慢下来 —— 完全跟随设备的消费速度。
        // 既不丢也不饿，两边靠 TCP 自己同步，不需要任何时钟估算。
        let backpressure = audio::state() == State::Speaking && SPK_CH.len() >= 18;
        if backpressure {
            Timer::after(Duration::from_millis(6)).await;
        } else {
        match c.fill(Duration::from_millis(20)).await {
            Ok(()) => last_rx = Instant::now(),
            Err(ws::ReadErr::Timeout) => {
                // 超时是正常节拍（网关没话说时就是没数据）。但安静太久
                // 就得怀疑这条连接已经死了，见 last_rx 的说明。
                if last_rx.elapsed() > Duration::from_secs(30) {
                    return Err("30 秒没收到任何下行数据，判为断线");
                }
            }
            Err(ws::ReadErr::Closed) => return Err("连接已关闭"),
        }
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
                        // 播放队列满说明消费不过来，丢帧保证实时性。
                        // 丢一帧就是 32ms 的音频凭空消失，接缝处一声「啵」——
                        // 所以这个计数必须能看见，否则「破音」永远只能靠猜。
                        audio::SPK_DROP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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

        // tts start 后先攒够一整批 8 帧再真正开始播放，避免开头
        // 「空队列 → 补静音 → 硬切语音」那声「啵」。
        if audio::SPK_PRIMING.load(core::sync::atomic::Ordering::Acquire)
            && SPK_CH.len() >= audio::SPK_PRIME_FRAMES
        {
            audio::SPK_PRIMING.store(false, core::sync::atomic::Ordering::Release);
            audio::set_state(State::Speaking);
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
            log::debug!("[ws] 网关就绪");
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
                    // 【开始新一句前必须清空播放队列】
                    //
                    // 设备消费音频的速度比网关推送慢 1~3%（每圈 DMA 32ms 之外
                    // 还有 copy/提交的开销），漂移会在队列里一点点攒下来。
                    // 上一轮残留几帧不清，下一轮再从这几帧起步，几轮之后就顶满
                    // → 之后每帧都得丢，整句都是「啵啵啵」。
                    // 每轮开头归零，漂移就永远只在单轮内累积，装得下。
                    while SPK_CH.try_receive().is_ok() {}
                    audio::SPK_PLAYED.store(0, core::sync::atomic::Ordering::Relaxed);
                    audio::SPK_EMPTY.store(0, core::sync::atomic::Ordering::Relaxed);
                    audio::SPK_PARTIAL.store(0, core::sync::atomic::Ordering::Relaxed);
                    audio::SPK_DROP.store(0, core::sync::atomic::Ordering::Relaxed);
                    audio::SPK_USED_MS.store(0, core::sync::atomic::Ordering::Relaxed);
                    audio::SPK_PEAK.store(0, core::sync::atomic::Ordering::Relaxed);
                    audio::SPK_WAIT_MS.store(0, core::sync::atomic::Ordering::Relaxed);
                    // 【不要立刻切 Speaking】先停在 Thinking，让 ws_task 把
                    // 前 8 帧音频收进 SPK_CH；main 循环看到攒够一批后再切。
                    // 否则 speaker_task 第一圈队列为空，会先播 8 帧静音，
                    // 再从静音硬切到语音 —— 就是开头那声「啵」。
                    audio::SPK_PRIMING.store(true, core::sync::atomic::Ordering::Release);
                    audio::set_state(State::Thinking);
                }
                Some("stop") => {
                    audio::SPK_PRIMING.store(false, core::sync::atomic::Ordering::Release);
                    // 等播放队列放完再回待机，否则最后一句会被截断
                    while !SPK_CH.is_empty()
                        || audio::SPK_INFLIGHT.load(core::sync::atomic::Ordering::Acquire)
                    {
                        Timer::after(Duration::from_millis(20)).await;
                    }
                    // 一句播完打一行统计。三个数直接区分「破音」的不同来源：
                    //   drop>0  → 设备消费比网关慢，丢帧处啵一声（队列加深/清空已缓解）
                    //   empty>0 → 网关推得慢/网络抖，补静音的接缝处咔一声
                    //   avg>34ms→ DMA 一圈超过一帧时长，播放整体偏慢、队列持续涨
                    let played = audio::SPK_PLAYED.load(core::sync::atomic::Ordering::Relaxed);
                    let used = audio::SPK_USED_MS.load(core::sync::atomic::Ordering::Relaxed);
                    log::error!(
                        "[spk] 帧={played} 丢帧={} 空帧={} 半批={} 平均周期={}ms 其中等DMA={}ms 峰值={}",
                        audio::SPK_DROP.load(core::sync::atomic::Ordering::Relaxed),
                        audio::SPK_EMPTY.load(core::sync::atomic::Ordering::Relaxed),
                        audio::SPK_PARTIAL.load(core::sync::atomic::Ordering::Relaxed),
                        if played > 0 { used / played } else { 0 },
                        if played > 0 {
                            audio::SPK_WAIT_MS.load(core::sync::atomic::Ordering::Relaxed) / played
                        } else { 0 },
                        audio::SPK_PEAK.load(core::sync::atomic::Ordering::Relaxed),
                    );
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
