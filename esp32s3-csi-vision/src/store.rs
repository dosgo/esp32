//! Wi-Fi 账号密码持久化（网页配网写 flash，掉电不丢）。
//!
//! 为什么写 flash 而不是写死在代码里：换个 Wi-Fi 就得改代码重烧，太麻烦。
//! 现在的流程是「热点配网」：
//!   - 热点 `CSI-Vision` 永远开着，配网、救急都靠它
//!   - 没配过 → 浏览器打开 192.168.4.1 → 填账号密码 → 存 flash → 立刻去连
//!   - 配过 → 开机自动连
//!
//! 为什么是 0x9000：默认分区表里这块是 `nvs` 区，但本项目裸机固件
//! 根本不用 NVS（wifi 初始化时 nvs_enable=0），拿来存一百来个字节很安全；
//! 应用固件从 0x10000 起，不会撞上。

use core::str;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use heapless::String;
use log::warn;

/// 配置区起始地址（必须 4KB 扇区对齐，落在 nvs 区）
pub const CREDS_FLASH_ADDR: u32 = 0x9000;

const MAGIC: u32 = 0x4353_4956; // "CSIV"
pub const MAX_SSID: usize = 32;
pub const MAX_PASS: usize = 64;

pub type Ssid = String<MAX_SSID>;
pub type Pass = String<MAX_PASS>;

#[derive(Clone, Default)]
pub struct Creds {
    pub ssid: Ssid,
    pub pass: Pass,
}

impl Creds {
    pub fn is_set(&self) -> bool {
        !self.ssid.is_empty()
    }
}

/// 保证 4 字节对齐，免得 esp-storage 内部为了对齐在栈上开 4KB 临时缓冲。
/// 布局：`[0..4] magic  [4] ssid 长度  [5] 密码长度  [6..] 数据  [末尾] 校验和`
#[repr(align(4))]
struct Buf([u8; 128]);

/// 读出上次保存的账号密码；没存过 / 数据坏了 → None
pub fn load(f: &mut FlashStorage<'_>) -> Option<Creds> {
    let mut b = Buf([0; 128]);
    if let Err(e) = ReadNorFlash::read(f, CREDS_FLASH_ADDR, &mut b.0) {
        warn!("[store] flash 读失败({e:?})");
        return None;
    }
    if u32::from_le_bytes([b.0[0], b.0[1], b.0[2], b.0[3]]) != MAGIC {
        return None;
    }
    let sl = b.0[4] as usize;
    let pl = b.0[5] as usize;
    if sl == 0 || sl > MAX_SSID || pl > MAX_PASS || 6 + sl + pl + 1 > b.0.len() {
        return None;
    }
    let mut sum: u8 = 0;
    for i in 0..(6 + sl + pl) {
        sum = sum.wrapping_add(b.0[i]);
    }
    if sum != b.0[6 + sl + pl] {
        warn!("[store] flash 校验和不对，当作没配过");
        return None;
    }

    let mut c = Creds::default();
    if c.ssid
        .push_str(str::from_utf8(&b.0[6..6 + sl]).ok()?)
        .is_err()
    {
        return None;
    }
    if c.pass
        .push_str(str::from_utf8(&b.0[6 + sl..6 + sl + pl]).ok()?)
        .is_err()
    {
        return None;
    }
    Some(c)
}

/// 写进 flash。失败返回 false（本次可能连得上，但下次上电会忘）。
pub fn save(f: &mut FlashStorage<'_>, c: &Creds) -> bool {
    let sl = c.ssid.len();
    let pl = c.pass.len();
    if sl == 0 || sl > MAX_SSID || pl > MAX_PASS {
        warn!("[store] SSID/密码长度不合法，不写");
        return false;
    }

    let mut b = Buf([0u8; 128]);
    b.0[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    b.0[4] = sl as u8;
    b.0[5] = pl as u8;
    b.0[6..6 + sl].copy_from_slice(c.ssid.as_bytes());
    b.0[6 + sl..6 + sl + pl].copy_from_slice(c.pass.as_bytes());

    let mut sum: u8 = 0;
    for i in 0..(6 + sl + pl) {
        sum = sum.wrapping_add(b.0[i]);
    }
    b.0[6 + sl + pl] = sum;

    // NOR flash 只能把 1 写成 0，所以必须先擦再写
    if let Err(e) = NorFlash::erase(
        f,
        CREDS_FLASH_ADDR,
        CREDS_FLASH_ADDR + FlashStorage::SECTOR_SIZE,
    ) {
        warn!("[store] flash 擦除失败({e:?})，配网不会被记住");
        return false;
    }
    if let Err(e) = NorFlash::write(f, CREDS_FLASH_ADDR, &b.0) {
        warn!("[store] flash 写入失败({e:?})，配网不会被记住");
        return false;
    }
    true
}

/// 忘掉已存的 Wi-Fi（网页上的「清除」按钮）。
pub fn clear(f: &mut FlashStorage<'_>) -> bool {
    NorFlash::erase(
        f,
        CREDS_FLASH_ADDR,
        CREDS_FLASH_ADDR + FlashStorage::SECTOR_SIZE,
    )
    .is_ok()
}
