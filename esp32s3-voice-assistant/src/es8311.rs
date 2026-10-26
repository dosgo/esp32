//! ES8311 音频编解码器驱动（纯 Rust，I2C 控制）
//!
//! 寄存器定义和上电顺序取自 Linux 主线 `sound/soc/codecs/es8311.c`，
//! 这里把 DAPM 的声明式路由展开成了过程式的「一次性全开」。
//!
//! 目标配置：**16kHz / 16bit / 单声道 / I2S 标准格式 / MCLK = 256×Fs = 4.096MHz**
//!
//! 三种模式的区别（决定 0x01 的 MCLK_SEL 位）：
//! - codec 作为 I2S slave，MCLK 从 MCLK 引脚进（本驱动，最常用）
//! - 无 MCLK 线时用 BCLK 当内部 MCLK（MCLK_SEL=1）
//!
//! 写寄存器一律用「读-改-写」，只动需要的位，其余保持芯片默认值。
//! 这比整字节硬写安全得多 —— 不同批次的 ES8311 默认值有细微差别。

// mute_dac / set_mic_gain_db 是预留的调参接口（实测麦克风电平不合适时用）
#![allow(dead_code)]

use esp_hal::i2c::master::I2c;

// ============ 寄存器地址 ============
const REG_RESET: u8 = 0x00;
const REG_CLKMGR1: u8 = 0x01;
const REG_CLKMGR2: u8 = 0x02;
const REG_CLKMGR5: u8 = 0x05;
const REG_SDP_IN: u8 = 0x09; // ADC → I2S（录音方向）
const REG_SDP_OUT: u8 = 0x0A; // I2S → DAC（播放方向）
const REG_SYS1: u8 = 0x0B;
const REG_SYS2: u8 = 0x0C;
const REG_SYS3: u8 = 0x0D; // 模拟电源总闸
const REG_SYS4: u8 = 0x0E; // PGA + ADC 调制器
const REG_SYS8: u8 = 0x12; // DAC 电源
const REG_SYS10: u8 = 0x14; // 麦克风 PGA 增益 / 输入选择
const REG_ADC2: u8 = 0x16; // ADC scale
const REG_ADC3: u8 = 0x17; // ADC 数字音量
const REG_ADC8: u8 = 0x1C; // ADC EQ / HPF
const REG_DAC1: u8 = 0x31;
const REG_DAC2: u8 = 0x32; // DAC 数字音量
const REG_DAC6: u8 = 0x37;

// ============ 位定义 ============
/// 0x00 bit7：CSM（Clock State Machine）总开关，关掉芯片不进工作态
const RESET_CSM_ON: u8 = 0x80;
const RESET_RST_ALL: u8 = 0x1F;

/// 0x01
const CLK_MCLK_ON: u8 = 1 << 5;
const CLK_BCLK_ON: u8 = 1 << 4;
const CLK_ADC_ON: u8 = 1 << 3;
const CLK_DAC_ON: u8 = 1 << 2;
const CLK_ANA_ADC_ON: u8 = 1 << 1;
const CLK_ANA_DAC_ON: u8 = 1 << 0;

/// 0x09 / 0x0A
const SDP_WL_16: u8 = 0x03 << 2; // 16bit 字长
const SDP_FMT_I2S: u8 = 0x00; // 标准 I2S（Philips）
const SDP_MUTE: u8 = 1 << 6;

/// 0x0D
const SYS3_PDN_ANA: u8 = 1 << 7;
const SYS3_PDN_IBIAS: u8 = 1 << 6;
const SYS3_PDN_ADCBIAS: u8 = 1 << 5;
const SYS3_PDN_ADCVREF: u8 = 1 << 4;
const SYS3_PDN_DACVREF: u8 = 1 << 3;
const SYS3_PDN_VREF: u8 = 1 << 2;
const SYS3_VMIDSEL_MASK: u8 = 0x03;
/// VMIDSEL = 2：正常操作档（1 是启动档，0 是掉电）
const SYS3_VMIDSEL_OP: u8 = 2;

/// 0x0E
const SYS4_PDN_PGA: u8 = 1 << 6;
const SYS4_PDN_MOD: u8 = 1 << 5;

/// 0x12
const SYS8_PDN_DAC: u8 = 1 << 1;

/// 0x14
const SYS10_PGA_GAIN_MASK: u8 = 0x1F;
const SYS10_LINESEL_SHIFT: u8 = 4;

/// 0dB 对应 0xBF（-95.5dB + 191×0.5dB = 0dB，见 TLV 表）
const VOL_0DB: u8 = 0xBF;

pub struct Es8311<'d> {
    i2c: I2c<'d, esp_hal::Blocking>,
    addr: u8,
}

impl<'d> Es8311<'d> {
    pub fn new(i2c: I2c<'d, esp_hal::Blocking>, addr: u8) -> Self {
        Self { i2c, addr }
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), ()> {
        self.i2c.write(self.addr, &[reg, val]).map_err(|_| ())
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, ()> {
        let mut b = [0u8; 1];
        self.i2c
            .write_read(self.addr, &[reg], &mut b)
            .map_err(|_| ())?;
        Ok(b[0])
    }

    /// 读-改-写：`clear` 里的位清零，`set` 里的位置一
    fn update(&mut self, reg: u8, clear: u8, set: u8) -> Result<(), ()> {
        let cur = self.read_reg(reg)?;
        let next = (cur & !clear) | set;
        if next != cur {
            self.write_reg(reg, next)?;
        }
        Ok(())
    }

    /// 芯片是否在线（读 CHIPID1，正常应是 0x83）
    pub fn probe(&mut self) -> Option<u8> {
        self.read_reg(0xFD).ok()
    }

    /// 完整上电 + 配置。调用前必须先让 ESP32 输出 MCLK/BCLK/WS。
    pub fn init(&mut self) -> Result<(), ()> {
        // ---- 1. 软复位 ----
        self.write_reg(REG_RESET, RESET_RST_ALL)?;
        esp_rom_delay_ms(20);
        self.write_reg(REG_RESET, 0x00)?;
        esp_rom_delay_ms(10);
        // CSM 打开，进入工作态
        self.write_reg(REG_RESET, RESET_CSM_ON)?;
        esp_rom_delay_ms(5);

        // ---- 2. 时钟 ----
        // MCLK 从 MCLK 引脚输入（bit7=0），并打开 BCLK/ADC/DAC 及各自的模拟时钟
        self.update(
            REG_CLKMGR1,
            0,
            CLK_MCLK_ON | CLK_BCLK_ON | CLK_ADC_ON | CLK_DAC_ON | CLK_ANA_ADC_ON | CLK_ANA_DAC_ON,
        )?;
        // 16k / 4.096MHz 对应系数表里的 (div=1, mult=1, div_adc_dac=1)，
        // 也就是三个字段全 0，不用算分频
        self.update(REG_CLKMGR2, 0xFF, 0x00)?;
        self.update(REG_CLKMGR5, 0xFF, 0x00)?;

        // ---- 3. 数字音频接口格式 ----
        // 录音方向和播放方向都要设：16bit + I2S，先不静音
        self.update(REG_SDP_IN, 0xFF, SDP_WL_16 | SDP_FMT_I2S)?;
        self.update(REG_SDP_OUT, 0xFF, SDP_WL_16 | SDP_FMT_I2S)?;

        // ---- 4. 模拟电源 ----
        // 上电等待时间给最小值，然后一次性把 bias / Vref / VMID 全打开
        self.write_reg(REG_SYS1, 0x00)?;
        self.write_reg(REG_SYS2, 0x00)?;
        self.update(
            REG_SYS3,
            SYS3_PDN_ANA
                | SYS3_PDN_IBIAS
                | SYS3_PDN_ADCBIAS
                | SYS3_PDN_ADCVREF
                | SYS3_PDN_DACVREF
                | SYS3_PDN_VREF
                | SYS3_VMIDSEL_MASK,
            SYS3_VMIDSEL_OP,
        )?;
        esp_rom_delay_ms(10);

        // ---- 5. 麦克风通路 ----
        // PGA 和 ADC 调制器上电
        self.update(REG_SYS4, SYS4_PDN_PGA | SYS4_PDN_MOD, 0)?;
        // 输入选择：0 = MIC1P/MIC1N 差分（板载模拟麦走这条）
        // PGA 增益 12dB（0x04，步进 3dB）。太大会削顶，太小 ASR 识别率掉
        self.update(REG_SYS10, SYS10_PGA_GAIN_MASK | (0x03 << SYS10_LINESEL_SHIFT), 0x04)?;
        // ADC 数字侧 0dB
        self.update(REG_ADC2, 0x07, 0x00)?;
        self.write_reg(REG_ADC3, VOL_0DB)?;
        // 关 ALC（自动增益在近场语音上会抽气），开高通滤掉直流
        self.update(REG_ADC8, 0, 0x60)?;

        // ---- 6. 播放通路 ----
        self.update(REG_SYS8, SYS8_PDN_DAC, 0)?;
        self.update(REG_DAC1, SDP_MUTE, 0)?;
        self.write_reg(REG_DAC2, VOL_0DB)?;
        self.update(REG_DAC6, 0, 0x08)?; // EQ bypass

        Ok(())
    }

    /// 设置播放音量。`vol` 0~100，100 = 0dB
    pub fn set_volume(&mut self, vol: u8) -> Result<(), ()> {
        let vol = vol.min(100);
        // 0dB 往下每 0.5dB 一档，100~0 映射到 0xBF~0x00
        let reg = ((vol as u16 * VOL_0DB as u16) / 100) as u8;
        self.write_reg(REG_DAC2, reg)
    }

    /// 播放静音（不影响录音）
    pub fn mute_dac(&mut self, mute: bool) -> Result<(), ()> {
        if mute {
            self.update(REG_DAC1, 0, SDP_MUTE)
        } else {
            self.update(REG_DAC1, SDP_MUTE, 0)
        }
    }

    /// 麦克风增益。`gain_db` 取 0/3/6/.../30
    pub fn set_mic_gain_db(&mut self, gain_db: u8) -> Result<(), ()> {
        let step = (gain_db / 3).min(10);
        self.update(REG_SYS10, SYS10_PGA_GAIN_MASK, step)
    }
}

/// ROM 里的忙等延时。ES8311 上电序列里有几处必须等芯片内部稳定，
/// 用 `embassy_time::Timer` 在同步上下文里不可用，这里直接空转。
#[inline]
fn esp_rom_delay_ms(ms: u32) {
    // 240MHz 下每秒约 240M 个周期，这里按「每毫秒 240k 次循环」估，
    // 只用于 5~20ms 的粗延时，精度不重要
    for _ in 0..(ms * 24_000) {
        core::hint::spin_loop();
    }
}
