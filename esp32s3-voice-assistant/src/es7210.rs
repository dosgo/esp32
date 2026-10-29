//! ES7210 麦克风 ADC 驱动（纯 Rust，I2C 控制）
//!
//! # 为什么需要它
//!
//! 微雪 ESP32-S3-Touch-LCD-1.54 的音频是**两颗 codec**：
//!
//! | 芯片 | 地址 | 负责 |
//! |---|---|---|
//! | ES8311 | 0x18 | 喇叭输出（DAC） |
//! | **ES7210** | **0x40** | **麦克风输入（4 路 ADC）** |
//!
//! 一开始只初始化了 ES8311，结果 I2C 扫描里那个 "0x40 未知设备" 就是 ES7210。
//! 它没被配置 → ADC 不工作 → I2S RX 收到的全是静音 → 唤醒词永远不命中、
//! ASR 也永远是空字符串。
//!
//! # 参数
//!
//! 16kHz / 16bit / 标准 I2S / MCLK = 256×Fs = 4.096MHz / MIC 增益 30dB。
//! 这些值直接取自微雪 BSP（`01_factory/components/esp_bsp/bsp_codec.c`）
//! 和 Arduino 版驱动（`10_esp_sr/es7210.cpp`），是这块板子验证过的配置。
//!
//! # 寄存器写入顺序不能乱
//!
//! 先软复位 → 上电时序 → HPF → I2S 格式 → 模拟电源 → MIC 偏置/增益/供电 →
//! 采样率时钟 → 最后 0x00 写 0x71 再写 0x41 使能。
//! 中间任何一步顺序错了都不会报错，只是没声音。

#![allow(dead_code)]

use embedded_hal::i2c::I2c;

/// ES7210 的固定 I2C 地址
pub const ADDR: u8 = 0x40;

// ============ 寄存器地址（es7210_reg.h）============
const REG_RESET: u8 = 0x00;
const REG_MAINCLK: u8 = 0x02;
const REG_LRCK_DIVH: u8 = 0x04;
const REG_LRCK_DIVL: u8 = 0x05;
const REG_POWER_DOWN: u8 = 0x06;
const REG_OSR: u8 = 0x07;
const REG_TIME_CONTROL0: u8 = 0x09;
const REG_TIME_CONTROL1: u8 = 0x0A;
const REG_SDP_INTERFACE1: u8 = 0x11;
const REG_SDP_INTERFACE2: u8 = 0x12;
const REG_ADC34_HPF2: u8 = 0x20;
const REG_ADC34_HPF1: u8 = 0x21;
const REG_ADC12_HPF2: u8 = 0x22;
const REG_ADC12_HPF1: u8 = 0x23;
const REG_ANALOG: u8 = 0x40;
const REG_MIC12_BIAS: u8 = 0x41;
const REG_MIC34_BIAS: u8 = 0x42;
const REG_MIC1_GAIN: u8 = 0x43;
const REG_MIC2_GAIN: u8 = 0x44;
const REG_MIC3_GAIN: u8 = 0x45;
const REG_MIC4_GAIN: u8 = 0x46;
const REG_MIC1_POWER: u8 = 0x47;
const REG_MIC2_POWER: u8 = 0x48;
const REG_MIC3_POWER: u8 = 0x49;
const REG_MIC4_POWER: u8 = 0x4A;
const REG_MIC12_POWER: u8 = 0x4B;
const REG_MIC34_POWER: u8 = 0x4C;

// ============ 参数取值 ============
/// 16bit → REG11 的高 3 位
const I2S_BITS_16B: u8 = 0x60;
/// 标准 I2S 格式（非左对齐/DSP）
const I2S_FMT_I2S: u8 = 0x00;
/// MIC 偏置 2.87V（厂商默认）
const MIC_BIAS_2V87: u8 = 0x70;
/// MIC 增益。
///
/// 【30dB → 18dB，2026-10-28】
///
/// 30dB 是厂商 BSP 的默认值（近距离录音室场景很合适），但这块板子的麦克风
/// 离嘴很近、房间又安静，实测录音峰值直接顶到 **32767 / -32768** —— 满幅削波。
/// 削顶之后的波形是一段段平顶方波，ASR 自然识别不出来（实测出来的都是
/// 「。」这种空结果），VAD 也会因为能量忽高忽低判不准断句。
///
/// 降 12dB（幅度约 1/4）之后峰值回到 8000 左右，留足余量不再削波。
/// 同时网关侧的 VAD 阈值要跟着从 250 降到 100（信号等比变小）。
const MIC_GAIN_DB_STEP: u8 = 6; // 6 × 3dB = 18dB

pub struct Es7210<'d, I> {
    i2c: I,
    addr: u8,
    _lifetime: core::marker::PhantomData<&'d ()>,
}

impl<'d, I> Es7210<'d, I>
where
    I: I2c,
{
    pub fn new(i2c: I, addr: u8) -> Self {
        Self {
            i2c,
            addr,
            _lifetime: core::marker::PhantomData,
        }
    }

    fn write(&mut self, reg: u8, val: u8) -> Result<(), I::Error> {
        self.i2c.write(self.addr, &[reg, val])
    }

    fn read(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut b = [0u8; 1];
        self.i2c.write_read(self.addr, &[reg], &mut b)?;
        Ok(b[0])
    }

    /// 探测芯片是否在线（读一次寄存器，能应答就算在）
    pub fn probe(&mut self) -> Option<u8> {
        self.read(REG_RESET).ok()
    }

    /// 完整初始化。写完最后两个 0x00 寄存器后 ADC 就开始出数据了。
    ///
    /// 失败时返回出错的那一步。这个方法**必须能自证成功**：实机上我见过两次
    /// 启动结果完全不同（一次成功拿到 L=570 的麦克风信号，一次直接失败），
    /// 所以调用方要按返回值重试，不能假定一次就成。
    pub fn init(&mut self) -> Result<(), &'static str> {
        // 1. 软复位。0xFF 是复位，0x32 是退出复位并进待机
        self.write(REG_RESET, 0xFF).map_err(|_| "复位: 0x00=0xFF")?;
        self.write(REG_RESET, 0x32).map_err(|_| "复位: 0x00=0x32")?;

        // 2. 上电建立时间
        self.write(REG_TIME_CONTROL0, 0x30)
            .map_err(|_| "建立时间: 0x09")?;
        self.write(REG_TIME_CONTROL1, 0x30)
            .map_err(|_| "建立时间: 0x0A")?;

        // 3. ADC1~4 的高通滤波器（去直流偏置）
        self.write(REG_ADC12_HPF1, 0x2A)
            .map_err(|_| "HPF: 0x23")?;
        self.write(REG_ADC12_HPF2, 0x0A)
            .map_err(|_| "HPF: 0x22")?;
        self.write(REG_ADC34_HPF1, 0x2A)
            .map_err(|_| "HPF: 0x21")?;
        self.write(REG_ADC34_HPF2, 0x0A)
            .map_err(|_| "HPF: 0x20")?;

        // 4. I2S 格式：16bit + 标准 I2S，关闭 TDM（走 2 声道，MIC1 左 / MIC2 右）
        self.write(REG_SDP_INTERFACE1, I2S_FMT_I2S | I2S_BITS_16B)
            .map_err(|_| "I2S 格式: 0x11")?;
        self.write(REG_SDP_INTERFACE2, 0x00)
            .map_err(|_| "I2S 格式: 0x12")?;

        // 5. 模拟电源 + VMID 电压
        self.write(REG_ANALOG, 0xC3)
            .map_err(|_| "模拟电源: 0x40")?;

        // 6. MIC1~4 偏置 2.87V
        self.write(REG_MIC12_BIAS, MIC_BIAS_2V87)
            .map_err(|_| "MIC 偏置: 0x41")?;
        self.write(REG_MIC34_BIAS, MIC_BIAS_2V87)
            .map_err(|_| "MIC 偏置: 0x42")?;

        // 7. MIC1~4 增益（bit4 必须置位，见厂商驱动 `mic_gain | 0x10`）
        let g = MIC_GAIN_DB_STEP | 0x10;
        self.write(REG_MIC1_GAIN, g)
            .map_err(|_| "MIC 增益: 0x43")?;
        self.write(REG_MIC2_GAIN, g)
            .map_err(|_| "MIC 增益: 0x44")?;
        self.write(REG_MIC3_GAIN, g)
            .map_err(|_| "MIC 增益: 0x45")?;
        self.write(REG_MIC4_GAIN, g)
            .map_err(|_| "MIC 增益: 0x46")?;

        // 8. 各 MIC 偏置供电
        self.write(REG_MIC1_POWER, 0x08)
            .map_err(|_| "MIC 供电: 0x47")?;
        self.write(REG_MIC2_POWER, 0x08)
            .map_err(|_| "MIC 供电: 0x48")?;
        self.write(REG_MIC3_POWER, 0x08)
            .map_err(|_| "MIC 供电: 0x49")?;
        self.write(REG_MIC4_POWER, 0x08)
            .map_err(|_| "MIC 供电: 0x4A")?;

        // 9. 采样率时钟：MCLK=4.096MHz / LRCK=16kHz
        //    系数取自厂商表 `{4096000, 16000, ss_ds=0, adc_div=1, dll=1,
        //    doubler=1, osr=0x20, mclk_src=0, lrck_h=1, lrck_l=0}`
        self.write(REG_OSR, 0x20).map_err(|_| "时钟: 0x07")?;
        // adc_div | doubler<<6 | dll<<7 = 0x01 | 0x40 | 0x80
        self.write(REG_MAINCLK, 0xC1)
            .map_err(|_| "时钟: 0x02")?;
        self.write(REG_LRCK_DIVH, 0x01)
            .map_err(|_| "时钟: 0x04")?;
        self.write(REG_LRCK_DIVL, 0x00)
            .map_err(|_| "时钟: 0x05")?;

        // 10. 关掉 DLL
        self.write(REG_POWER_DOWN, 0x04)
            .map_err(|_| "关 DLL: 0x06")?;

        // 11. MIC 偏置 + ADC + PGA 全部供电
        self.write(REG_MIC12_POWER, 0x0F)
            .map_err(|_| "ADC 供电: 0x4B")?;
        self.write(REG_MIC34_POWER, 0x0F)
            .map_err(|_| "ADC 供电: 0x4C")?;

        // 12. 使能器件
        self.write(REG_RESET, 0x71).map_err(|_| "使能: 0x00=0x71")?;
        self.write(REG_RESET, 0x41).map_err(|_| "使能: 0x00=0x41")?;

        // 13. 回读校验 —— 只写不读，I2C 上偶发的 NAK 会被当成成功，
        //     结果 ADC 其实没使能，整条麦克风链路静默而没有任何报错。
        match self.read(REG_RESET) {
            Ok(0x41) => Ok(()),
            Ok(v) => {
                let _ = v;
                Err("回读校验: 0x00 不是 0x41（芯片没进工作态）")
            }
            Err(_) => Err("回读校验: 0x00 读失败"),
        }
    }
}
