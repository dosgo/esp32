//! ESP32-S3 片上温度传感器（esp-hal 1.2.2 仍未给 S3 实现，这里直接点寄存器）
//!
//! ## 为什么自己写
//!
//! esp-hal 1.2.2 的 `tsens` 模块只对元数据里带 `soc_has_tsens` 的芯片编译（S3 至今没有，
//! 所以 S3 拿不到官方 API），但 **S3 的硬件是有的** —— 就在 `SENS` 外设的
//! `SAR_TSENS_CTRL`(0x50) / `SAR_TSENS_CTRL2`(0x54) 两个寄存器里。
//! 所以这里直接拿 PAC 点寄存器，逻辑和 ESP-IDF 的 `temperature_sensor_ll` 一致。
//!
//! ## 寄存器位（来自 esp32s3 PAC 与 TRM）
//!
//! - bit 0..8  `sar_tsens_out`           ：温度原始值
//! - bit 8     `sar_tsens_ready`         ：转换完成
//! - bit 14..22 `sar_tsens_clk_div`      ：时钟分频（复位值 = 6，不用动）
//! - bit 22    `sar_tsens_power_up`      ：上电
//! - bit 23    `sar_tsens_power_up_force`：上下电由软件控制（而不是 FSM）
//! - bit 24    `sar_tsens_dump_out`      ：强制输出
//!
//! ## 换算公式的坑
//!
//! ESP-IDF（S3）：`raw * 0.4386 - 27.88 * offset - 20.52`，offset 来自校准，
//! 没烧校准值时按 **0** 算 → `raw * 0.4386 - 20.52`。
//! esp-hal 给 C3 用的是 `offset = -1`（相当于 +27.88），两个差 28℃，
//! 所以这里把两种都打日志，方便对照实物判断哪个对。

use esp32s3::SENS;

/// 室温下 raw 大概 90~100（对应 20~25℃）
pub fn read_raw() -> Option<u8> {
    let sens = unsafe { &*SENS::ptr() };

    // 软件强制上电 + 强制输出（FSM 控制时读不到稳定值）
    sens.sar_tsens_ctrl().modify(|_, w| {
        w.sar_tsens_power_up().set_bit();
        w.sar_tsens_power_up_force().set_bit();
        w.sar_tsens_dump_out().set_bit()
    });

    // 等转换完成。传感器时钟分频是 6，几十微秒就绪；给足余量不阻塞太久。
    for _ in 0..50_000 {
        if sens.sar_tsens_ctrl().read().sar_tsens_ready().bit_is_set() {
            let raw = sens.sar_tsens_ctrl().read().sar_tsens_out().bits();
            return Some(raw);
        }
    }
    None
}

/// ESP-IDF 的官方换算（offset = 0，未烧校准值时的默认）
pub fn celsius_idf(raw: u8) -> f32 {
    raw as f32 * 0.4386 - 20.52
}

/// esp-hal 给 C3 用的那套（offset = -1），只用来打日志对照
pub fn celsius_alt(raw: u8) -> f32 {
    raw as f32 * 0.4386 + 7.36
}
