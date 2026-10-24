//! STA（联网）用的 WiFi 账号密码持久化 + 热点（AP）参数。
//!
//! 为什么需要：原来 SSID/密码是写死在 main.rs 里的常量，换个 WiFi 就得改代码重烧。
//! 现在改成「热点配网」：
//!   - 热点永远开着（`ESP32-POWER` / 192.168.4.1），配网、救急都靠它
//!   - 没配过 → 就用热点，浏览器打开 192.168.4.1 填账号密码
//!   - 配过 → 开机自动连，连上才能连 Blinker 云（MQTT 需要外网）
//!
//! 为什么写 flash 而不是 RTC memory：RTC memory 掉电就没了，而且 esp-hal 没暴露公开 API。
//! 为什么写到 0x9000：默认分区表里这块是 nvs 区，裸机固件根本不用 NVS（wifi 初始化里
//! nvs_enable=0），拿来存一百来个字节很安全；应用固件在 0x10000 之后，不会撞上。

use core::str;

use embassy_net::Ipv4Address;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use heapless::String;
use log::warn;

/// 存账号密码的 flash 地址（必须 4KB 扇区对齐；0x9000 落在 nvs 区）
pub const CREDS_FLASH_ADDR: u32 = 0x9000;

/// 整个配置区占几个扇区：0x9000 ~ 0xD000（共 16KB）。
/// 目前只用了第一块存 WiFi，后面几块留给将来的设置。
/// nvs 区在默认分区表里是 0x9000~0xE000，app 从 0x10000 起，擦这块安全。
pub const CONFIG_SECTORS: u32 = 4;

const MAGIC: u32 = 0x574B_564D; // "WKVM"
pub const MAX_SSID: usize = 32;
pub const MAX_PASS: usize = 64;

// ---------------- 热点参数 ----------------

/// 和 KVM 那个热点区分开，免得两个设备搞混
pub const AP_SSID: &str = "ESP32-POWER";
/// 留空 = 开放网络（不用密码）；否则必须 8~63 个字符（WPA2 的硬性要求）
pub const AP_PASSWORD: &str = "power1234";
/// 热点模式下 ESP32 自己的地址（也是配网页面的访问地址）
pub const AP_IP: Ipv4Address = Ipv4Address::new(192, 168, 4, 1);

// ---------------- 凭证 ----------------

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

// ---------------- flash 读写 ----------------

/// 保证 4 字节对齐，免得 esp-storage 内部为了对齐在栈上开 4KB 临时缓冲。
/// 布局：[0..4] magic  [4] ssid 长度  [5] 密码长度  [6..] 数据  [末尾] 校验和
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

/// 写进 flash。失败返回 false（本次连得上，但下次上电会忘）。
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

    // NOR flash 只能把 1 改写成 0，所以要先擦再写
    if let Err(e) = NorFlash::erase(f, CREDS_FLASH_ADDR, CREDS_FLASH_ADDR + FlashStorage::SECTOR_SIZE)
    {
        warn!("[store] flash 擦除失败({e:?})，配网不会被记住");
        return false;
    }
    if let Err(e) = NorFlash::write(f, CREDS_FLASH_ADDR, &b.0) {
        warn!("[store] flash 写入失败({e:?})，配网不会被记住");
        return false;
    }
    true
}

/// 恢复出厂设置（长按 BOOT 3 秒 / 网页按钮）：
/// 把**整个配置区**（4 个扇区）擦干净，不只是 WiFi 那一块。
pub fn factory_reset(f: &mut FlashStorage<'_>) -> bool {
    let start = CREDS_FLASH_ADDR;
    let end = start + CONFIG_SECTORS * FlashStorage::SECTOR_SIZE;
    if let Err(e) = NorFlash::erase(f, start, end) {
        warn!("[store] 恢复出厂失败({e:?})");
        return false;
    }
    true
}

// =============== Blinker 凭据（网页里可改，改完不用重烧） ===============
//
// 占用配置区的第二块扇区（0xA000）。三个值都是字符串：
//   deviceName（也是 MQTT clientId）/ iotId（MQTT 用户名）/ iotToken（MQTT 密码）
// 它们来自浏览器打开下面这个地址后返回的 JSON：
//   https://iot.diandeng.tech/api/v1/user/device/diy/auth?authKey=你的authKey&protocol=mqtt

pub const BLINKER_FLASH_ADDR: u32 = CREDS_FLASH_ADDR + FlashStorage::SECTOR_SIZE;
pub const MAX_DEV: usize = 40;
pub const MAX_TOKEN: usize = 72;

#[derive(Clone, Default)]
pub struct BlinkerCreds {
    pub device: String<MAX_DEV>,
    pub iot_id: String<MAX_DEV>,
    pub token: String<MAX_TOKEN>,
}

impl BlinkerCreds {
    pub const fn new() -> Self {
        Self {
            device: String::new(),
            iot_id: String::new(),
            token: String::new(),
        }
    }
    pub fn is_set(&self) -> bool {
        !self.device.is_empty() && !self.token.is_empty()
    }
}

/// 布局：[0..4] magic  [4] dev 长度  [5] iotId 长度  [6] token 长度  [7..] 数据  [末尾] 校验和
pub fn load_blinker(f: &mut FlashStorage<'_>) -> Option<BlinkerCreds> {
    let mut b = Buf([0; 128]);
    if ReadNorFlash::read(f, BLINKER_FLASH_ADDR, &mut b.0).is_err() {
        return None;
    }
    if u32::from_le_bytes([b.0[0], b.0[1], b.0[2], b.0[3]]) != MAGIC {
        return None;
    }
    let dl = b.0[4] as usize;
    let il = b.0[5] as usize;
    let tl = b.0[6] as usize;
    if dl == 0 || dl > MAX_DEV || il > MAX_DEV || tl == 0 || tl > MAX_TOKEN {
        return None;
    }
    let total = 7 + dl + il + tl;
    if total + 1 > b.0.len() {
        return None;
    }
    let mut sum: u8 = 0;
    for i in 0..total {
        sum = sum.wrapping_add(b.0[i]);
    }
    if sum != b.0[total] {
        return None;
    }

    let mut c = BlinkerCreds::new();
    let mut off = 7;
    c.device.push_str(str::from_utf8(&b.0[off..off + dl]).ok()?).ok()?;
    off += dl;
    c.iot_id.push_str(str::from_utf8(&b.0[off..off + il]).ok()?).ok()?;
    off += il;
    c.token.push_str(str::from_utf8(&b.0[off..off + tl]).ok()?).ok()?;
    Some(c)
}

pub fn save_blinker(f: &mut FlashStorage<'_>, c: &BlinkerCreds) -> bool {
    let dl = c.device.len();
    let il = c.iot_id.len();
    let tl = c.token.len();
    if dl == 0 || dl > MAX_DEV || il > MAX_DEV || tl == 0 || tl > MAX_TOKEN {
        warn!("[store] Blinker 凭据长度不合法，不写");
        return false;
    }

    let mut b = Buf([0u8; 128]);
    b.0[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    b.0[4] = dl as u8;
    b.0[5] = il as u8;
    b.0[6] = tl as u8;
    let mut off = 7;
    b.0[off..off + dl].copy_from_slice(c.device.as_bytes());
    off += dl;
    b.0[off..off + il].copy_from_slice(c.iot_id.as_bytes());
    off += il;
    b.0[off..off + tl].copy_from_slice(c.token.as_bytes());
    off += tl;
    let mut sum: u8 = 0;
    for i in 0..off {
        sum = sum.wrapping_add(b.0[i]);
    }
    b.0[off] = sum;

    let sector = FlashStorage::SECTOR_SIZE;
    if NorFlash::erase(f, BLINKER_FLASH_ADDR, BLINKER_FLASH_ADDR + sector).is_err() {
        warn!("[store] Blinker 扇区擦除失败");
        return false;
    }
    if NorFlash::write(f, BLINKER_FLASH_ADDR, &b.0).is_err() {
        warn!("[store] Blinker 写入失败");
        return false;
    }
    true
}
