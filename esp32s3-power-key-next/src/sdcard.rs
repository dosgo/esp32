//! TF 卡（microSD）挂载 —— 给 USB 大容量存储（MSC）当后端。
//!
//! 接线来自 GNPE Pocket-Dongle-S3-0.96 原理图（见 lcd.rs 顶部注释），
//! 这块板的 TF 卡走**普通 SPI**，不是 SDMMC：
//!
//!   SCLK = GPIO17
//!   MOSI = GPIO18
//!   MISO = GPIO16
//!   CS   = GPIO47
//!
//! 用 SPI3（SPI2 已经被 LCD 占了）。
//!
//! ## 为什么要自己写 SpiDevice
//!
//! SD 卡规范要求**初始化阶段时钟 ≤400kHz**，初始化完成后才能提速到 20MHz+。
//! `embedded-hal-bus` 的 `ExclusiveDevice` 把 Spi 包进去以后没法再改频率，
//! 所以这里手写一个 `SpiDevice`，额外暴露 `set_freq()`，
//! 初始化走 400kHz、mount 成功后立刻切到 20MHz。
//!
//! ## 上电时序
//!
//! SD 卡 SPI 模式要求：CS 拉高的前提下先发至少 74 个时钟（这里发 80 个），
//! 让卡从原生 SD 模式切到 SPI 模式。少这一步，有些卡会一直不响应 CMD0。
//! 这个动作必须绕过 CS 直接打在 SpiBus 上，所以放在 `dummy_clocks()` 里。

use core::cell::Cell;

use embedded_hal::spi::{ErrorType, Operation, SpiBus, SpiDevice};
use embedded_sdmmc::sdcard::spi::AcquireOpts;
use embedded_sdmmc::{Block, BlockDevice, BlockIdx, SdCard};
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

/// SD 卡扇区大小，也是 MSC 跟主机对话的块大小
pub const BLOCK_SIZE: usize = 512;

/// 初始化频率：SD 规范上限 400kHz
const FREQ_INIT_HZ: u32 = 400_000;
/// 工作频率。这块板实测 20MHz 稳定；上不去就降到 10MHz
const FREQ_FAST_HZ: u32 = 20_000_000;

/// 包了一层 CS 的 SPI 设备。
///
/// ## CS 为什么要常低
///
/// `embedded-sdmmc` 是**逐字节**调用 `SpiDevice` 的：命令帧一次 transaction、
/// 每个响应字节一次、数据块再一次。如果按常规做法在每次 transaction 前后
/// 拉低/抬高 CS，一条 CMD17 会被拆成十几段，SD 卡在中途看到 CS 上升沿
/// 就会把这条命令作废（规范里 CS 抬高 = 终止当前操作）。
///
/// 这块板的 TF 卡独占 SPI3（没有第二个从设备），所以进 SPI 模式后
/// **CS 一直保持低**，和 Arduino SD 库、esp-idf 的 sdspi 驱动做法一致。
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
    /// 发完立刻把 CS 拉低并一直保持 —— 后面所有传输都在这条"选中"状态里完成。
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
    /// 注意：**不动 CS**。CS 由 `enter_spi_mode()` 拉低后常驻，
    /// 原因见上面结构体注释。
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
        for op in operations {
            match op {
                Operation::Read(buf) => SpiBus::read(&mut self.spi, buf)?,
                Operation::Write(buf) => SpiBus::write(&mut self.spi, buf)?,
                Operation::Transfer(rx, tx) => SpiBus::transfer(&mut self.spi, rx, tx)?,
                Operation::TransferInPlace(buf) => {
                    SpiBus::transfer_in_place(&mut self.spi, buf)?
                }
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

/// 挂载好的 TF 卡。
///
/// 内部 `SdCard` 自己用 RefCell 做互斥，所有方法都是 `&self`，
/// 所以 MSC 任务里直接持有它就行，不需要再包一层 Mutex。
pub struct SdStorage {
    card: SdCard<SdSpi<'static>, Delay>,
    /// 容量（块数）。0 表示"没插卡 / 卡不认" —— 不在这里报错，
    /// 让上层降级成"空读卡器"而不是整个设备起不来。
    blocks: Cell<u32>,
}

/// 卡初始化重试次数。
///
/// embedded-sdmmc 默认 50 次，而每次 CMD0 超时要烧 ~100ms —— 没插卡时
/// 光挂卡就要 5 秒，USB 那边会晚 5 秒才枚举出键鼠。这里压到 5 次（~500ms），
/// 正常卡上电到就绪远小于这个时间。
const ACQUIRE_RETRIES: u32 = 5;

impl SdStorage {
    /// 挂卡。**只有 SPI 外设配置失败才算错误**；卡没插/不认都返回 Ok，
    /// 靠 `blocks() == 0` 判断"无介质"。
    pub fn mount(
        spi: esp_hal::peripherals::SPI3<'static>,
        sclk: esp_hal::peripherals::GPIO17<'static>,
        mosi: esp_hal::peripherals::GPIO18<'static>,
        miso: esp_hal::peripherals::GPIO16<'static>,
        cs: esp_hal::peripherals::GPIO47<'static>,
    ) -> Result<Self, MountError> {
        use log::{info, warn};

        // 注意：这一段跑在 USB 初始化之前，日志还走 USB-Serial-JTAG。
        // 启动阶段日志必须极简，否则 FIFO 灌满会死等（详见 Cargo.toml 里的说明）。
        // 所以这里只在**结束时**打一条，中途一句都不打。
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

        // 74+ 时钟让卡进 SPI 模式（CS 高电平下先打 80 个），随后 CS 常低
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

        // 第一次访问触发真正的卡初始化（CMD0/CMD8/ACMD41/CMD58），全程仍是 400kHz。
        // 失败就当没插卡 —— 注意 card_type 仍是 None，之后每次读写都会再试一次，
        // 所以插着卡热启动也能自己认回来。
        let probe = storage.card.num_bytes();
        if let Ok(bytes) = probe {
            let b = (bytes / BLOCK_SIZE as u64) as u32;
            if b > 0 {
                storage.blocks.set(b);
                // 初始化完了才提速
                storage.card.spi(|d| d.set_freq(FREQ_FAST_HZ));
            }
        }
        match &probe {
            Ok(bytes) => info!("[sd] 就绪：{} 字节，{} 块", bytes, storage.blocks.get()),
            Err(e) => warn!("[sd] 初始化失败：{e:?}"),
        }

        Ok(storage)
    }

    /// 卡上一共多少个 512 字节块（0 = 无介质）
    pub fn blocks(&self) -> u32 {
        self.blocks.get()
    }

    /// 读一块。越界或卡报错都返回 false（MSC 那边会翻成 MEDIUM ERROR）
    pub fn read_block(&self, lba: u32, out: &mut [u8; BLOCK_SIZE]) -> bool {
        if lba >= self.blocks.get() {
            return false;
        }
        let mut blocks = [Block::new()];
        match self.card.read(&mut blocks, BlockIdx(lba)) {
            Ok(()) => {
                out.copy_from_slice(&blocks[0].contents);
                true
            }
            Err(_) => false,
        }
    }

    /// 写一块
    pub fn write_block(&self, lba: u32, data: &[u8; BLOCK_SIZE]) -> bool {
        if lba >= self.blocks.get() {
            return false;
        }
        let mut blocks = [Block::new()];
        blocks[0].contents.copy_from_slice(data);
        self.card.write(&blocks, BlockIdx(lba)).is_ok()
    }
}

/// 挂卡失败的原因（串口日志用）
///
/// 只有 SPI 外设配置失败会走到这里 —— 卡本身的问题不算错误（见 `mount`）。
#[derive(Debug)]
pub enum MountError {
    /// SPI 外设配置失败（频率算不出来之类，基本不会发生）
    SpiConfig,
}

impl core::fmt::Display for MountError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SpiConfig => write!(f, "SPI 配置失败"),
        }
    }
}
