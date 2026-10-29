//! ESP32-S3 片上温度传感器（esp-hal 1.2.2 仍未给 S3 实现，这里直接点寄存器）
//!
//! # 为什么自己写
//!
//! esp-hal 1.2.2 的 `tsens` 模块被 `#[cfg(soc_has_tsens)]` 挡着，S3 的元数据里
//! 没有这个标记，所以拿不到官方 API。但 **S3 的硬件是有的** —— 就在 `SENS` 外设的
//! `SAR_TSENS_CTRL`(0x50) / `SAR_TSENS_CTRL2`(0x54)。这里直接拿 PAC 点寄存器，
//! 逻辑和 ESP-IDF 的 `temperature_sensor_ll` 一致。
//!
//! # 寄存器位（esp32s3 PAC + TRM）
//!
//! | 位 | 名称 | 说明 |
//! |----|------|------|
//! | 0..8 | `sar_tsens_out` | 温度原始值 |
//! | 8 | `sar_tsens_ready` | 转换完成 |
//! | 14..22 | `sar_tsens_clk_div` | 时钟分频（复位值 6，不用动） |
//! | 22 | `sar_tsens_power_up` | 上电 |
//! | 23 | `sar_tsens_power_up_force` | 上/下电由软件控制（而非 FSM） |
//! | 24 | `sar_tsens_dump_out` | 强制输出 |
//!
//! # 换算公式的坑
//!
//! ESP-IDF（S3）：`raw * 0.4386 - 27.88 * offset - 20.52`，`offset` 来自校准；
//! 没烧校准值时按 **0** 算 → `raw * 0.4386 - 20.52`。
//! esp-hal 给 C3 用的是 `offset = -1`（相当于 +27.88），两者差 28℃。
//! 这里用 ESP-IDF 那套，并且**留了对照函数**：如果实测室温明显不对
//! （比如显示 50℃ 而不是 25℃），换 `celsius_alt` 即可。

use core::sync::atomic::{AtomicI16, Ordering};

use esp32s3::SENS;

/// 上一次成功的读数。`i16::MIN` 表示还没读到过。
static LAST: AtomicI16 = AtomicI16::new(i16::MIN);

/// 读原始值（0~255）。失败返回 None（等 ready 超时）。
pub fn read_raw() -> Option<u8> {
    let sens = unsafe { &*SENS::ptr() };

    // 软件强制上电 + 强制输出。FSM 控制时读出来的值不稳定。
    sens.sar_tsens_ctrl().modify(|_, w| {
        w.sar_tsens_power_up().set_bit();
        w.sar_tsens_power_up_force().set_bit();
        w.sar_tsens_dump_out().set_bit()
    });

    // 等转换完成。时钟分频是 6，几十微秒就绪；50k 次循环是充裕余量，
    // 正常情况下第一次或第二次就退出。
    for _ in 0..50_000u32 {
        let r = sens.sar_tsens_ctrl().read();
        if r.sar_tsens_ready().bit_is_set() {
            return Some(r.sar_tsens_out().bits());
        }
    }
    None
}

/// ESP-IDF 的换算（offset = 0，未烧校准值时的默认）
pub fn celsius_idf(raw: u8) -> f32 {
    raw as f32 * 0.4386 - 20.52
}

/// esp-hal 给 C3 用的那套（offset = -1）。只用来对照排错。
///
/// 实测如果室温明显不对（比如显示 50℃ 而不是 25℃），把 `refresh()` 里的
/// `celsius_idf` 换成这个。
#[allow(dead_code)]
pub fn celsius_alt(raw: u8) -> f32 {
    raw as f32 * 0.4386 + 7.36
}

/// 读一次温度并缓存，返回摄氏度（取整）。
///
/// 取整是因为：屏幕上只有 6x10 点阵字，一位小数没地方放；
/// 而且片上温度本身精度也就 ±1℃ 量级。
pub fn refresh() -> Option<i16> {
    let raw = read_raw()?;
    // 2026-10-27 实机对照：
    //   celsius_idf → 50℃，celsius_alt → 80℃
    // 50℃ 对「跑着 WiFi + 240MHz 的 S3」是正常芯片温度（不是室温），
    // 80℃ 明显过高 —— 所以改用 IDF 那套。两套公式只差 28℃ 常量。
    let t = celsius_idf(raw) as i16;
    LAST.store(t, Ordering::Relaxed);
    Some(t)
}

/// 取上次缓存的值（不触发新的转换）。还没读过就返回 None。
pub fn last() -> Option<i16> {
    let v = LAST.load(Ordering::Relaxed);
    if v == i16::MIN { None } else { Some(v) }
}
