//! 网络日志：dongle / 单口板在 host 模式下看不到串口，日志只能走网络。
//!
//! 原因：ESP32-S3 的 USB-Serial-JTAG 和原生 USB OTG **共用 GPIO19/20**。
//! 一旦进 host 模式，19/20 归 OTG 控制器，烧写和串口日志通道同时消失。
//! 所以这里实现一个 `log::Log`，把日志塞进 channel，再由 `netlog_task`
//! 通过 UDP 广播发到局域网，电脑上用 `tools/netlog_listen.py` 收。
//!
//! 早期日志（WiFi 还没起来时）会堆在 channel 里，联网后一次性吐出来。

use core::fmt::Write;

use embassy_net::{
    IpAddress, Ipv4Address, Stack,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, TrySendError};
use heapless::String;

/// 日志广播端口。电脑上：`python tools/netlog_listen.py`
pub const LOG_PORT: u16 = 4444;

const MSG_CAP: usize = 200;
const QUEUE: usize = 24;

pub type Msg = String<MSG_CAP>;

/// 日志缓冲。容量 24 条，满了丢最旧的一条（保证最新的总能发出去）。
pub static LOG_CHAN: Channel<CriticalSectionRawMutex, Msg, QUEUE> = Channel::new();

struct NetLogger;

impl NetLogger {
    fn push(&self, record: &log::Record) {
        let tag = match record.level() {
            log::Level::Error => "E",
            log::Level::Warn => "W",
            log::Level::Info => "I",
            log::Level::Debug => "D",
            log::Level::Trace => "T",
        };

        let mut s: Msg = String::new();
        if write!(s, "[{}] {}", tag, record.args()).is_err() {
            return;
        }

        // 队列满：丢掉最旧的一条，给新的腾位置
        // embassy-sync 的 TrySendError 只有 Full（channel 不会关闭）
        if let Err(TrySendError::Full(s)) = LOG_CHAN.try_send(s) {
            let _ = LOG_CHAN.try_receive(); // 丢掉最旧的一条
            let _ = LOG_CHAN.try_send(s);
        }
    }
}

impl log::Log for NetLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.push(record)
    }

    fn flush(&self) {}
}

static LOGGER: NetLogger = NetLogger;

/// 注册全局 logger。必须在任何 `info!()` 之前调用，且只能调用一次。
pub fn init() {
    // 已经注册过就忽略（set_logger 只能成功一次）
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
}

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

/// 把缓冲里的日志 UDP 广播出去。断网时暂停，恢复后继续。
#[embassy_executor::task]
pub async fn netlog_task(stack: Stack<'static>) {
    let rx_meta = mk_static!([PacketMetadata; 4], [PacketMetadata::EMPTY; 4]);
    let rx_buf = mk_static!([u8; 512], [0u8; 512]);
    let tx_meta = mk_static!([PacketMetadata; 4], [PacketMetadata::EMPTY; 4]);
    let tx_buf = mk_static!([u8; 1024], [0u8; 1024]);

    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    if socket.bind(0).is_err() {
        return;
    }

    // 广播地址：电脑不用配固定 IP，同网段监听即可
    let dst: IpAddress = IpAddress::Ipv4(Ipv4Address::new(255, 255, 255, 255));

    loop {
        stack.wait_config_up().await;
        while stack.config_v4().is_some() {
            let msg = LOG_CHAN.receive().await;
            if socket.send_to(msg.as_bytes(), (dst, LOG_PORT)).await.is_err() {
                break;
            }
        }
    }
}
