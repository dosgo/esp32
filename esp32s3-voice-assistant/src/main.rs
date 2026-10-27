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

mod audio;
mod board;
mod config;
mod display;
mod es8311;
mod json;
mod sdcard;
mod sensors;
mod ws;

use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Runner, StackResources};
use embassy_time::{Duration, Timer};
use embedded_hal_bus::i2c::RefCellDevice;
use esp_hal::{
    clock::CpuClock,
    gpio::AnyPin,
    i2c::master::{Config as I2cConfig, I2c},
    i2s::master::{Channels, DataFormat, I2s, TdmConfig},
    rng::Rng,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{
    sta::StationConfig, AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig,
    Interface, Password, Ssid,
};

use crate::audio::{State, MIC_CH, SPK_CH};
use crate::config::{DEVICE_NAME, GATEWAY_HOST, GATEWAY_PORT, WIFI_PASSWORD, WIFI_SSID};
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
    esp_println::logger::init_logger_from_env();

    // Wi-Fi 协议栈会从内部堆 malloc，128KB 是实测够用的值
    esp_alloc::heap_allocator!(size: 128 * 1024);

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CPU_CLOCK));

    // PSRAM 挂进全局堆（esp-radio 内部走 malloc_internal，拿不到 PSRAM）
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

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
        I2cConfig::default().with_frequency(Rate::from_khz(100)),
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

    // ---------------- 3. Wi-Fi ----------------
    let ssid = Ssid::try_from(WIFI_SSID).unwrap_or_else(|_| Ssid::try_from("fallback").unwrap());
    let sta = StationConfig::default()
        .with_ssid(ssid)
        .with_authentication(match Password::try_from(WIFI_PASSWORD) {
            Ok(pw) => AuthenticationMethodConfig::Wpa2Personal(pw),
            Err(_) => AuthenticationMethodConfig::Open,
        });

    let mut controller = match esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(WifiConfig::Station(sta)),
    ) {
        Ok(c) => c,
        Err(e) => {
            println!("[wifi] 初始化失败: {e:?}");
            loop {
                Timer::after(Duration::from_secs(3600)).await;
            }
        }
    };

    let sta_iface = Interface::station();
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        sta_iface,
        NetConfig::dhcpv4(Default::default()),
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        seed,
    );
    spawner.spawn(net_task(runner).expect("net_task"));

    println!("[wifi] 连接 {WIFI_SSID} …");
    match controller.connect_async().await {
        Ok(_) => println!("[wifi] 已连上"),
        Err(e) => println!("[wifi] 连接失败: {e:?}"),
    }

    stack.wait_config_up().await;
    let mut ip_text: heapless::String<16> = heapless::String::new();
    if let Some(cfg) = stack.config_v4() {
        println!("[wifi] IP = {}", cfg.address);
        use core::fmt::Write;
        let _ = write!(ip_text, "{}", cfg.address);
    }

    // ---------------- 4. 屏幕 ----------------
    spawner.spawn(
        display::display_task(
            peripherals.SPI2,
            unsafe { AnyPin::steal(board::PIN_LCD_SCLK) },
            unsafe { AnyPin::steal(board::PIN_LCD_MOSI) },
            unsafe { AnyPin::steal(board::PIN_LCD_CS) },
            unsafe { AnyPin::steal(board::PIN_LCD_DC) },
            unsafe { AnyPin::steal(board::PIN_LCD_RST) },
            unsafe { AnyPin::steal(board::PIN_LCD_BL) },
            ip_text,
        )
        .expect("display_task"),
    );

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
    // IMU 每 8 拍（2 秒）打一行，避免刷屏；触摸有按下就报。
    let mut tick: u32 = 0;
    loop {
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

#[embassy_executor::task]
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
    let ip = ws::parse_ipv4(GATEWAY_HOST).ok_or("GATEWAY_HOST 不是合法的 IPv4（见 config.rs）")?;

    let mut host = heapless::String::<32>::new();
    {
        use core::fmt::Write;
        let _ = write!(host, "{GATEWAY_HOST}:{GATEWAY_PORT}");
    }

    log::info!("[ws] 连接 {}:{} …", GATEWAY_HOST, GATEWAY_PORT);
    let mut c = WsConn::connect(stack, ip, GATEWAY_PORT, "/", DEVICE_NAME, host.as_str()).await?;
    log::info!("[ws] 握手完成");

    // hello：告诉网关我们的音频格式
    c.send_text(r#"{"type":"hello","version":1,"format":"pcm","sample_rate":16000,"channels":1,"bits":16,"frame_ms":60}"#)
        .await
        .map_err(|_| "hello 发送失败")?;

    let mut down = [0u8; board::FRAME_BYTES];
    let mut text_buf = [0u8; 1024];

    loop {
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
