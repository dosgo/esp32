//! CSI（Channel State Information）采集与整形。
//!
//! 数据流：
//!
//! ```text
//!   射频收到帧
//!        ↓  （esp-radio 内部，Wi-Fi 任务上下文）
//!   on_csi()  ── 算 64 路振幅 ──→  LIVE: Watch<1>
//!                                      ↓
//!                              WebSocket（浏览器心跳驱动取最新帧）
//! ```
//!
//! 为什么只用一个 `Watch`：
//!   - CSI 回调可能 200~500Hz（每个收到的帧都触发），用队列把每一帧都推给
//!     浏览器，WebSocket 会被打满，而人眼 50fps 以上毫无差别。
//!   - Watch 只保留「最新一帧」，浏览器慢了就自然跳帧，永远不会积压出延迟。

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::watch::Watch;
use esp_radio::wifi::csi::WifiCsiInfo;

/// 子载波上限。HT20（20MHz）标准是 56 路，但 ESP32-S3 实测上报 64 路
/// （走的是 L-LTF，64 个采样点）。留 64 以兼容两种。
pub const MAX_CARRIERS: usize = 64;

/// 一帧 CSI 的紧凑表示。
///
/// 只留振幅不留相位：相位要做载波频偏（CFO）和采样频偏（SFO）校准，
/// 不校准的话相位每个包随机跳，看着全是噪声。振幅抖动的物理意义
/// 已经足够做人体存在 / 动作识别。
#[derive(Clone)]
pub struct Sample {
    /// 收到这一帧时的信号强度（dBm），负值，-30 很强 / -80 很弱。
    pub rssi: i8,
    /// 本帧有效的子载波对数。
    pub n: u8,
    /// bit0: 前 4 字节被硬件标记无效；bit1: rx_state != 0（接收有错）。
    pub flags: u8,
    /// 每路子载波的振幅（整数开方，0~255）。
    pub amp: [u8; MAX_CARRIERS],
    /// 时间戳（毫秒，从开机算起）。
    pub ts_ms: u32,
}

impl Sample {
    /// 序列化成 WebSocket 二进制帧：
    ///
    /// ```text
    ///   [0]     0x01            帧类型 = CSI
    ///   [1]     rssi  (i8)
    ///   [2]     flags (u8)
    ///   [3]     n     (u8)
    ///   [4..]   amp[0..n]       每字节一路子载波
    ///   [4+n..] ts_ms  (u32 LE)
    /// ```
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let n = self.n as usize;
        out[0] = 0x01;
        out[1] = self.rssi as u8;
        out[2] = self.flags;
        out[3] = self.n;
        out[4..4 + n].copy_from_slice(&self.amp[..n]);
        out[4 + n..8 + n].copy_from_slice(&self.ts_ms.to_le_bytes());
        8 + n
    }
}

/// 最新一帧。回调写、WebSocket 读。
///
/// **第三个泛型参数是「最大订阅者数」，不是缓冲深度！**
/// 写 1 意味着整个系统同时只能有一个 WebSocket 会话拿 receiver，
/// 第二个会话 `receiver()` 会返回 None。之前这里写成 1，
/// 结果第一个页面异常断开后 receiver 没释放，之后新连接就 panic，
/// 表现就是「网页突然打不开、板子像死了一样」。
///
/// 给到 8：远大于 WEB_SESSIONS，任何一个会话出问题都不会卡死别人。
pub static LIVE: Watch<CriticalSectionRawMutex, Sample, 8> = Watch::new();

/// 累计收到的 CSI 帧数（用来算实际帧率）。
pub static FRAMES: AtomicU32 = AtomicU32::new(0);
/// 最近一次的 RSSI，给状态栏用。
pub static LAST_RSSI: AtomicI32 = AtomicI32::new(0);
/// 最近一次的包长（子载波对数），0 表示还没收到过。
pub static LAST_N: AtomicU32 = AtomicU32::new(0);

/// 整数平方根（牛顿迭代）。
///
/// 不用 `f32::sqrt`：no_std 下它要链 `sqrtf`，某些目标还得额外引 libm；
/// 整数迭代 5 次搞定，56 路 × 100Hz 对 240MHz 的 S3 来说可以忽略。
#[inline]
fn isqrt_u8(v: u32) -> u8 {
    if v == 0 {
        return 0;
    }
    if v >= 255 * 255 {
        return 255;
    }
    let mut x = v;
    let mut y = (x + 1) >> 1;
    while y < x {
        x = y;
        y = (x + v / x) >> 1;
    }
    x as u8
}

/// CSI 回调。**运行在 Wi-Fi 任务上下文**，三条铁律：
///
///   - 不能 `.await`
///   - 不能长时间持锁（会卡住射频任务，直接掉包）
///   - 不能分配内存（堆锁可能正被别处持有，会死锁）
///
/// 所以这里只做纯计算 + 一次 Watch 覆盖写。
pub fn on_csi(info: WifiCsiInfo<'_>) {
    FRAMES.fetch_add(1, Ordering::Relaxed);

    let rssi = info.rssi();
    LAST_RSSI.store(rssi as i32, Ordering::Relaxed);

    let buf = info.buf();
    // buf 是 [imag0, real0, imag1, real1, ...]，每路 2 字节
    let mut n = buf.len() / 2;
    if n > MAX_CARRIERS {
        n = MAX_CARRIERS;
    }
    LAST_N.store(n as u32, Ordering::Relaxed);

    let mut s = Sample {
        rssi,
        n: n as u8,
        flags: 0,
        amp: [0; MAX_CARRIERS],
        ts_ms: embassy_time::Instant::now().as_millis() as u32,
    };
    if info.first_word_invalid() {
        s.flags |= 0b01;
    }
    if info.rx_state() != 0 {
        s.flags |= 0b10;
    }

    for k in 0..n {
        let im = buf[2 * k] as i32;
        let re = buf[2 * k + 1] as i32;
        s.amp[k] = isqrt_u8((re * re + im * im) as u32);
    }

    LIVE.sender().send(s);
}
