//! TF 卡挂载（SPI 模式）
//!
//! # 为什么用 SPI 而不是 SDIO 4 位
//!
//! 资料给的接法是 SDIO：CLK=16 / CMD=15 / D0=17 / D1=18 / D2=13 / D3=14。
//! `esp-hal` 的 SDIO 驱动要走 `sdio` crate 的 async 栈（`Slot::into_async` +
//! `sdio::BlockDevice` + async delay），仓库里没有可参考的完整示例，
//! 盲写不容易一次对。
//!
//! 而 SD 卡规范允许用这同一组引脚跑 **SPI 兼容模式**：
//!
//! | SD 卡脚 | SPI 里的角色 | GPIO |
//! |---------|--------------|------|
//! | CLK     | SCLK         | 16 |
//! | CMD     | MOSI         | 15 |
//! | D0      | MISO         | 17 |
//! | D3      | CS           | 14 |
//!
//! 卡内部的协议栈会自动识别 SPI 模式（CS 低 + CMD 上发 CMD0），
//! 板上那几颗 SDIO 上拉电阻对推挽输出没有影响。代价是带宽降到单线，
//! 20MHz 下约 2MB/s —— 存语音片段完全够。
//!
//! 将来要跑满 4 位宽，把本文件换成 SDIO 实现即可，`main` 里的调用面不变。
//!
//! # 代码来源
//!
//! SPI 时序和 CS 常低的处理借自 `esp32s3-power-key-next/src/sdcard.rs`。
//!
//! ⚠️ 注意**那两块板子不是同一块**：power-key 的 TF 卡是 SPI 直连
//! （SCLK/MOSI/MISO/CS 四根独立脚），本板是 SDIO 六线卡座。
//! 本文件把 SDIO 引脚**重新映射**成 SPI 兼容模式，属于新写的东西，
//! 不是原样复用，**在本板上尚未实机验证过**。
//!
//! 已知的两个风险点：
//!
//! 1. 卡座上 D1/D2 悬空（SPI 模式不用），可能引入干扰
//! 2. 卡座自带的 SDIO 上拉挂在 CMD/D0 上。推挽输出不受影响，
//!    D3 作 CS 时上拉会让空闲态被拉高 —— 理论没问题，实测为准
//!
//! 如果挂不上卡，第一选择是**放弃 SD 卡**（语音主链路不依赖它），
//! 第二选择才是换成真正的 SDIO 4 位实现。

use core::cell::Cell;

use embedded_hal::spi::{ErrorType, Operation, SpiBus, SpiDevice};
use embedded_sdmmc::{SdCard, sdcard::spi::AcquireOpts};
use esp_hal::{
    Blocking,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
};

/// SD 卡扇区大小
pub const BLOCK_SIZE: usize = 512;

/// 初始化频率：SD 规范上限 400kHz
const FREQ_INIT_HZ: u32 = 400_000;
/// 工作频率。SPI 模式下 20MHz 是稳妥值
const FREQ_FAST_HZ: u32 = 20_000_000;

/// 卡初始化重试次数。默认 50 次时没插卡要卡 5 秒，压到 5 次约 500ms
const ACQUIRE_RETRIES: u32 = 5;

/// 包了一层 CS 的 SPI 设备。
///
/// `embedded-sdmmc` 是逐字节调用 `SpiDevice` 的：命令帧一次、每个响应字节一次、
/// 数据块再一次。如果每次 transaction 都抬落 CS，一条 CMD17 会被拆成十几段，
/// 卡在中途看到 CS 上升沿就把命令作废了。所以进 SPI 模式后 **CS 常低**，
/// 和 Arduino SD 库、esp-idf 的 sdspi 驱动做法一致。
pub struct SdSpi<'d> {
    spi: Spi<'d, Blocking>,
    cs: Output<'d>,
    cfg: SpiConfig,
}

impl<'d> SdSpi<'d> {
    fn new(spi: Spi<'d, Blocking>, cs: Output<'d>, cfg: SpiConfig) -> Self {
        Self { spi, cs, cfg }
    }

    /// 上电时序：CS 高，发 80 个时钟（0xFF），让卡从原生 SD 模式切到 SPI 模式。
    /// 发完立刻把 CS 拉低并一直保持。
    fn enter_spi_mode(&mut self) {
        self.cs.set_high();
        let buf = [0xFFu8; 10];
        let _ = SpiBus::write(&mut self.spi, &buf);
        self.cs.set_low();
    }

    /// 改总线频率（初始化后提速用）
    fn set_freq(&mut self, hz: u32) {
        self.cfg = self.cfg.with_frequency(Rate::from_hz(hz));
        let _ = self.spi.apply_config(&self.cfg);
    }
}

impl ErrorType for SdSpi<'_> {
    type Error = esp_hal::spi::Error;
}

impl SpiDevice<u8> for SdSpi<'_> {
    /// 注意：**不动 CS**，原因见结构体注释
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
        for op in operations {
            match op {
                Operation::Read(buf) => SpiBus::read(&mut self.spi, buf)?,
                Operation::Write(buf) => SpiBus::write(&mut self.spi, buf)?,
                Operation::Transfer(rx, tx) => SpiBus::transfer(&mut self.spi, rx, tx)?,
                Operation::TransferInPlace(buf) => SpiBus::transfer_in_place(&mut self.spi, buf)?,
                Operation::DelayNs(ns) => {
                    let mut d = Delay::new();
                    embedded_hal::delay::DelayNs::delay_ns(&mut d, *ns);
                }
            }
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.transaction(&mut [Operation::Read(buf)])
    }

    fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
        self.transaction(&mut [Operation::Write(buf)])
    }

    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error> {
        self.transaction(&mut [Operation::Transfer(read, write)])
    }

    fn transfer_in_place(&mut self, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.transaction(&mut [Operation::TransferInPlace(buf)])
    }
}

/// 挂载好的 TF 卡内部 `SdCard` 自带 RefCell 互斥，所有方法都是 `&self`
pub struct SdStorage {
    card: SdCard<SdSpi<'static>, Delay>,
    /// 容量（块数）。0 表示"没插卡 / 卡不认"
    blocks: Cell<u32>,
}

impl SdStorage {
    /// 挂卡。**只有 SPI 外设配置失败才算错误**；卡没插/不认都返回 Ok，
    /// 靠 `blocks() == 0` 判断无介质，让上层降级而不是整个设备起不来。
    pub fn mount(
        spi: esp_hal::peripherals::SPI3<'static>,
        sclk: esp_hal::peripherals::GPIO16<'static>,
        mosi: esp_hal::peripherals::GPIO15<'static>,
        miso: esp_hal::peripherals::GPIO17<'static>,
        cs: esp_hal::peripherals::GPIO14<'static>,
    ) -> Result<Self, MountError> {
        let cfg = SpiConfig::default()
            .with_frequency(Rate::from_hz(FREQ_INIT_HZ))
            .with_mode(Mode::_0);

        let spi = Spi::new(spi, cfg)
            .map_err(|_| MountError::SpiConfig)?
            .with_sck(sclk)
            .with_mosi(mosi)
            .with_miso(miso);
        let cs = Output::new(cs, Level::High, OutputConfig::default());
        let mut dev = SdSpi::new(spi, cs, cfg);

        dev.enter_spi_mode();
        let card = SdCard::new_with_options(
            dev,
            Delay::new(),
            AcquireOpts {
                use_crc: true,
                acquire_retries: ACQUIRE_RETRIES,
            },
        );

        let storage = Self {
            card,
            blocks: Cell::new(0),
        };

        // 第一次访问才真正初始化卡（CMD0/CMD8/ACMD41/CMD58），全程 400kHz
        if let Ok(bytes) = storage.card.num_bytes() {
            let b = (bytes / BLOCK_SIZE as u64) as u32;
            if b > 0 {
                storage.blocks.set(b);
                // 初始化完了才提速
                storage.card.spi(|d| d.set_freq(FREQ_FAST_HZ));
            }
        }

        Ok(storage)
    }

    /// 卡上一共多少个 512 字节块（0 = 无介质）
    pub fn blocks(&self) -> u32 {
        self.blocks.get()
    }

    /// 容量（字节）
    pub fn bytes(&self) -> u64 {
        self.blocks.get() as u64 * BLOCK_SIZE as u64
    }
}

#[derive(Debug)]
pub enum MountError {
    SpiConfig,
}

impl core::fmt::Display for MountError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SpiConfig => write!(f, "SPI 配置失败"),
        }
    }
}
