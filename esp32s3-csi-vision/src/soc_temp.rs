//! ESP32-S3 片上温度传感器。
//!
//! esp-hal 1.1.2 的 `tsens` 模块只对 C2/C3/C6/H2 编译，S3 上没有 ——
//! 但 **S3 硬件其实有这个传感器**，就在 `SENS` 外设的
//! `SAR_TSENS_CTRL`(0x50) / `SAR_TSENS_CTRL2`(0x54) 里。
//! 所以这里直接点 PAC 寄存器，逻辑和 ESP-IDF 的 `temperature_sensor_ll` 一致。
//!
//! 寄存器位（来自 esp32s3 PAC 与 TRM）：
//!   - bit 0..8   `sar_tsens_out`           温度原始值
//!   - bit 8      `sar_tsens_ready`         转换完成
//!   - bit 14..22 `sar_tsens_clk_div`       时钟分频（复位值 6，不用动）
//!   - bit 22     `sar_tsens_power_up`      上电
//!   - bit 23     `sar_tsens_power_up_force` 上下电由软件控制（而不是 FSM）
//!   - bit 24     `sar_tsens_dump_out`      强制输出
//!
//! 换算公式的坑：ESP-IDF（S3）是 `raw * 0.4386 - 27.88 * offset - 20.52`，
//! 没烧校准值时 offset 按 0 算 → `raw * 0.4386 - 20.52`。
//! 而 esp-hal 给 C3 用的是 `offset = -1`（相当于 +27.88），两者差 28℃。
//! 这里用 IDF 那套，开机时会和另一种算法一起打日志方便对照。

use esp32s3::SENS;

/// 读一次原始值。实测：S3 跑 Wi-Fi AP 时 raw ≈ 197 → 66℃（芯片结温，
/// 比环境温度高 20~30℃，属正常）。
///
/// 会忙等最多 5 万次迭代（几十微秒到几毫秒），**只在低频任务里调用**。
pub fn read_raw() -> Option<u8> {
    let sens = unsafe { &*SENS::ptr() };

    // 软件强制上电 + 强制输出（FSM 控制时读不到稳定值）
    sens.sar_tsens_ctrl().modify(|_, w| {
        w.sar_tsens_power_up().set_bit();
        w.sar_tsens_power_up_force().set_bit();
        w.sar_tsens_dump_out().set_bit()
    });

    for _ in 0..50_000 {
        if sens.sar_tsens_ctrl().read().sar_tsens_ready().bit_is_set() {
            return Some(sens.sar_tsens_ctrl().read().sar_tsens_out().bits());
        }
    }
    None
}

/// 换算公式一：ESP-IDF 的 `raw * 0.4386 - 27.88 * offset - 20.52`，取 offset = 0。
///
/// 这是「没烧校准值」时的默认。实测算出来的温度**比功耗推算低约 28℃**，
/// 见下面的 `celsius_alt_x10`。
pub fn celsius_x10(raw: u8) -> i32 {
    ((raw as i32 * 4386) / 1000) - 205
}

/// 换算公式二：offset = -1，等价于 `raw * 0.4386 + 7.36`。
///
/// esp-hal 给 C3 用的就是这套。与公式一正好差 27.88℃。
///
/// 为什么倾向这套：功耗对比能对上。同样是这块板子，
/// power-key 项目 0.9W 时读 69℃，本项目 1.2W（功耗高 33%）按公式一只得 50℃，
/// 物理上说不通；按公式二得 78℃，和「0.9W→69℃ 按功耗比例外推」的 84℃ 接近。
pub fn celsius_alt_x10(raw: u8) -> i32 {
    ((raw as i32 * 4386) / 1000) + 74
}

/// 读一次温度，返回摄氏度 ×10（公式一）。
pub fn read_x10() -> Option<i32> {
    read_raw().map(celsius_x10)
}
