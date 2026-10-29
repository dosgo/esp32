//! I2C 总线上的设备：codec(ES8311) / IMU(QMI8658) / 电容触摸
//!
//! 这块板子的 codec、IMU、触摸三者的 SDA/SCL **都是 GPIO42/GPIO41**，
//! 物理上就是同一条总线，共用一组上拉电阻。所以：
//!
//! - 不能给每个设备各建一个 `I2c` 实例（引脚是独占的）
//! - 用 `embedded_hal_bus::i2c::RefCellDevice` 从同一个总线派生多个设备句柄
//! - 三个设备都留在**同一个任务**里顺序访问，所以 `RefCell` 足够，
//!   不需要上 `critical_section` 那种关中断的互斥
//!
//! 预期能在总线上扫到的地址：
//!
//! | 地址 | 设备 |
//! |------|------|
//! | 0x18 | ES8311（固定） |
//! | 0x6B / 0x6A | QMI8658（看 SA0 接法） |
//! | 0x15 / 0x38 / 0x5D / 0x63 | 触摸：CST816 / FT6336 / GT911 / AXS5106 |

#![allow(dead_code)]

use embedded_hal::i2c::I2c;
use heapless::Vec;

// ============================================================================
// I2C 扫描
// ============================================================================

/// 探测整条总线，返回所有 ACK 的 7 位地址（最多 32 个）。
///
/// 保留地址段（0x00~0x07 广播/CB 用、0x78~0x7F 10 位寻址）按规范跳过，
/// 不然会扫出一堆假地址。
pub fn scan(bus: &mut impl I2c) -> Vec<u8, 32> {
    let mut found = Vec::new();
    for addr in 0x08..=0x77u8 {
        // 空写一探 ACK。有些芯片被读会推进内部指针，所以这里用纯写
        if bus.write(addr, &[]).is_ok() {
            let _ = found.push(addr);
        }
    }
    found
}

/// 给扫描结果配上人话的设备名，方便对着串口日志判断
pub fn addr_name(addr: u8) -> &'static str {
    match addr {
        0x18 => "ES8311 (codec)",
        0x6b | 0x6a => "QMI8658 (IMU)",
        0x15 => "CST816S (touch)",
        0x38 => "FT6336U (touch)",
        0x5d | 0x14 => "GT911 (touch)",
        0x63 => "AXS5106 (touch)",
        0x2e => "CST820 (touch)",
        _ => "未知设备",
    }
}

// ============================================================================
// QMI8658 IMU
// ============================================================================

// ---- 寄存器地址 ----
const REG_WHO_AM_I: u8 = 0x00;
const REG_CTRL1: u8 = 0x02;
const REG_CTRL2: u8 = 0x03; // 加速度计：ODR + 量程
const REG_CTRL3: u8 = 0x04; // 陀螺仪：ODR + 量程
const REG_CTRL7: u8 = 0x08; // 使能位
const REG_RESET: u8 = 0x2d;

const REG_AX_L: u8 = 0x35; // AX/AY/AZ 各 16 位，低字节在前
const REG_GX_L: u8 = 0x3b; // GX/GY/GZ 同上

/// QMI8658A 的固定 ID
const WHO_AM_I_QMI8658: u8 = 0x05;

/// 软复位命令
const RESET_CMD: u8 = 0xb0;

// CTRL2：ODR = 250Hz（高 4 位 0x4），量程 ±4g（低 4 位 0x1）
const CTRL2_250HZ_4G: u8 = 0x41;
// CTRL3：ODR = 250Hz，量程 ±1024dps（低 4 位 0x6）
const CTRL3_250HZ_1024DPS: u8 = 0x46;
// CTRL7：加速度 + 陀螺同时使能
const CTRL7_ENABLE_BOTH: u8 = 0x03;

/// ±4g 量程下的灵敏度（16 位 ADC，2g 对应 16384 LSB，4g 对应 8192）
const ACCEL_LSB_PER_G: f32 = 8192.0;
/// ±1024dps 量程下的灵敏度（16 位 ADC，满量程 32768 对应 1024dps）
const GYRO_LSB_PER_DPS: f32 = 32.0;

/// 一次采样结果
#[derive(Debug, Clone, Copy, Default)]
pub struct Motion {
    /// 三轴加速度，单位 g
    pub accel_g: [f32; 3],
    /// 三轴角速度，单位 °/s
    pub gyro_dps: [f32; 3],
}

pub struct Qmi8658<I2C> {
    i2c: I2C,
    addr: u8,
}

impl<I2C: I2c> Qmi8658<I2C> {
    pub fn new(i2c: I2C, addr: u8) -> Self {
        Self { i2c, addr }
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, ()> {
        let mut b = [0u8; 1];
        self.i2c
            .write_read(self.addr, &[reg], &mut b)
            .map_err(|_| ())?;
        Ok(b[0])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), ()> {
        self.i2c.write(self.addr, &[reg, val]).map_err(|_| ())
    }

    /// 读 WHO_AM_I。正常 QMI8658A 是 0x05；QMI8658C 也兼容这个值。
    pub fn probe(&mut self) -> Option<u8> {
        self.read_reg(REG_WHO_AM_I).ok()
    }

    pub fn whoami_ok(&mut self) -> bool {
        self.probe() == Some(WHO_AM_I_QMI8658)
    }

    /// 把 0x00~0x10 的寄存器原始值打印出来。
    ///
    /// 这一步是给**对不上时排查**用的：不同批次的 QMI8658 默认值略有差别，
    /// 万一初始化后读不到数据，对比这十几个值就能看出是配置没写进去
    /// 还是寄存器地址本身就理解错了。
    pub fn dump_regs(&mut self) {
        for reg in 0x00..=0x10u8 {
            match self.read_reg(reg) {
                Ok(v) => log::info!("  [imu] reg 0x{reg:02X} = 0x{v:02X}"),
                Err(_) => log::info!("  [imu] reg 0x{reg:02X} = <读失败>"),
            }
        }
    }

    /// 初始化。失败返回 Err，调用方降级成「没有 IMU」继续跑。
    pub fn init(&mut self) -> Result<(), ()> {
        // 1. 软复位
        self.write_reg(REG_RESET, RESET_CMD)?;
        // 复位后芯片需要几毫秒重新就绪。这里用忙等而不是 async 延时，
        // 因为本驱动是同步的，而且只在启动时跑一次，十几微秒的差异无所谓。
        for _ in 0..20_000u32 {
            core::hint::spin_loop();
        }

        // 2. CTRL1 = 0x40：
        //    bit6 ADDR_AI = 1  连读时寄存器地址自动递增
        //    bit5 BE      = 0  小端（低字节在前）
        //
        //    【ADDR_AI 必须开】默认值是 0，不开的话 `read_xyz` 连读 6 字节
        //    读到的全是同一个寄存器的重复值，三轴数据会一模一样：
        //      a=(-3.64,-3.80,-3.80)g   g=(-835,-835,-835)°/s
        //    这不是芯片坏，是 burst read 没有递增。
        self.write_reg(REG_CTRL1, 0x40)?;

        // 3. 配置量程和输出速率
        self.write_reg(REG_CTRL2, CTRL2_250HZ_4G)?;
        self.write_reg(REG_CTRL3, CTRL3_250HZ_1024DPS)?;

        // 4. 使能加速度 + 陀螺
        self.write_reg(REG_CTRL7, CTRL7_ENABLE_BOTH)?;

        // 5. 让第一次采样完成
        for _ in 0..200_000u32 {
            core::hint::spin_loop();
        }

        // 6. 回头确认配置真的落盘了
        if self.read_reg(REG_CTRL7)? != CTRL7_ENABLE_BOTH {
            return Err(());
        }
        Ok(())
    }

    /// 从 `reg` 起连读 6 个字节，解成三轴 i16
    fn read_xyz(&mut self, reg: u8) -> Result<[i16; 3], ()> {
        let mut b = [0u8; 6];
        self.i2c
            .write_read(self.addr, &[reg], &mut b)
            .map_err(|_| ())?;
        // 小端：低字节在前
        Ok([
            i16::from_le_bytes([b[0], b[1]]),
            i16::from_le_bytes([b[2], b[3]]),
            i16::from_le_bytes([b[4], b[5]]),
        ])
    }

    /// 读一次加速度 + 角速度，已换算成物理单位
    pub fn read(&mut self) -> Result<Motion, ()> {
        let a = self.read_xyz(REG_AX_L)?;
        let g = self.read_xyz(REG_GX_L)?;
        Ok(Motion {
            accel_g: [
                a[0] as f32 / ACCEL_LSB_PER_G,
                a[1] as f32 / ACCEL_LSB_PER_G,
                a[2] as f32 / ACCEL_LSB_PER_G,
            ],
            gyro_dps: [
                g[0] as f32 / GYRO_LSB_PER_DPS,
                g[1] as f32 / GYRO_LSB_PER_DPS,
                g[2] as f32 / GYRO_LSB_PER_DPS,
            ],
        })
    }
}

// ============================================================================
// 电容触摸
// ============================================================================

/// 触摸芯片型号。启动时靠 I2C 探测确定，之后按型号走不同寄存器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchKind {
    /// 海栎创 CST816S，地址 0x15
    Cst816,
    /// 敦泰 FT6336U，地址 0x38
    Ft6336,
    /// 汇顶 GT911，地址 0x5D / 0x14
    Gt911,
    /// 未知 / 没焊
    None,
}

pub struct Touch<I2C> {
    i2c: I2C,
    addr: u8,
    kind: TouchKind,
}

impl<I2C: I2c> Touch<I2C> {
    /// 探测触摸芯片。整条总线扫一遍，按已知地址匹配型号。
    /// 找不到就返回 `TouchKind::None`，不报错 —— 触摸属于可选外设。
    pub fn probe(i2c: I2C) -> Self {
        let mut me = Self {
            i2c,
            addr: 0,
            kind: TouchKind::None,
        };
        for (addr, kind) in [
            (0x15u8, TouchKind::Cst816),
            (0x38, TouchKind::Ft6336),
            (0x5d, TouchKind::Gt911),
            (0x14, TouchKind::Gt911),
        ] {
            if me.i2c.write(addr, &[]).is_ok() {
                me.addr = addr;
                me.kind = kind;
                break;
            }
        }
        me
    }

    pub fn kind(&self) -> TouchKind {
        self.kind
    }

    /// 探测到的 I2C 地址（没探测到时是 0）
    pub fn addr(&self) -> u8 {
        self.addr
    }

    pub fn is_present(&self) -> bool {
        self.kind != TouchKind::None
    }

    fn read_regs(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), ()> {
        self.i2c
            .write_read(self.addr, &[reg], buf)
            .map_err(|_| ())
    }

    /// 读第一个触摸点。返回 `Some((x, y))` 表示有手指按下。
    pub fn read_point(&mut self) -> Option<(u16, u16)> {
        match self.kind {
            TouchKind::Cst816 => {
                // 0x02 = 手指数量；0x03~0x06 = X/Y 的 12 位坐标
                let mut b = [0u8; 5];
                self.read_regs(0x02, &mut b).ok()?;
                if b[0] == 0 {
                    return None;
                }
                let x = (((b[1] & 0x0f) as u16) << 8) | b[2] as u16;
                let y = (((b[3] & 0x0f) as u16) << 8) | b[4] as u16;
                Some((x, y))
            }
            TouchKind::Ft6336 => {
                // 0x02 = TD_STATUS（低 4 位是触摸点数）；0x03~0x06 = 第一个点
                let mut b = [0u8; 5];
                self.read_regs(0x02, &mut b).ok()?;
                if b[0] & 0x0f == 0 {
                    return None;
                }
                let x = (((b[1] & 0x0f) as u16) << 8) | b[2] as u16;
                let y = (((b[3] & 0x0f) as u16) << 8) | b[4] as u16;
                Some((x, y))
            }
            TouchKind::Gt911 => {
                // GT911 用 16 位寄存器地址，读法和前两个不一样。
                // 0x814E = 状态（bit7 有数据），0x8150 起是第一个点。
                let mut st = [0u8; 1];
                self.i2c
                    .write_read(self.addr, &[0x81, 0x4e], &mut st)
                    .ok()?;
                if st[0] & 0x80 == 0 {
                    return None;
                }
                let mut p = [0u8; 4];
                self.i2c
                    .write_read(self.addr, &[0x81, 0x50], &mut p)
                    .ok()?;
                // 读完要把状态位清掉，否则下一次读到的一直是「有数据」
                let _ = self.i2c.write(self.addr, &[0x81, 0x4e, 0x00]);
                let x = u16::from_le_bytes([p[0], p[1]]);
                let y = u16::from_le_bytes([p[2], p[3]]);
                Some((x, y))
            }
            TouchKind::None => None,
        }
    }
}
