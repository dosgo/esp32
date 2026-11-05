//! 配置持久化。
//!
//! Rust 侧没有引入 ESP-IDF 的 NVS 库（esp-wifi-sys 的 blob 里也没有 libnvs），
//! 所以自己写一个带 magic / 版本 / 校验和的固定布局配置块，放在 flash 的
//! 0x3F0000 处一个独立的 4KB 扇区。
//!
//! 为什么不放 NVS 分区：那里的 PHY/Wi-Fi 校准数据归驱动管，写坏了很难查。
//! 0x3F0000 在 factory app 分区（0x10000 + 0xfa0000）里、但远在 app 镜像
//! （目前约 620KB）之后，属于确定空闲的区域，擦它不影响启动。
//!
//! 读写走 esp-storage 的 FlashStorage；Flash 外设由常驻的 config_task 独占，
//! 启动阶段先用 reborrow 读一次（那时 Wi-Fi 还没初始化，配置必须先恢复）。

use core::net::Ipv4Addr;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use heapless::{String, Vec};
use log::{info, warn};

use crate::state;

/// 配置扇区偏移：4KB 对齐，16MB flash 内，避开 app 镜像
const SECTOR: u32 = 0x003F_0000;
const SECTOR_LEN: usize = 4096;
const BLOCK_LEN: usize = 512;
const MAGIC: &[u8; 4] = b"PGCF";
const VERSION: u8 = 1;
/// magic(4) + version(1) + payload_len(2)
const HEADER: usize = 7;

/// 网页保存配置后打一下，config_task 收到后合并 400ms 内的多次改动再写 flash
pub static SAVE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// NOR flash 写要求缓冲区 4 字节对齐
#[repr(align(4))]
struct Buf([u8; BLOCK_LEN]);

fn hash(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &x in b {
        h ^= x as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn put_u8(v: &mut Vec<u8, BLOCK_LEN>, x: u8) -> bool {
    v.push(x).is_ok()
}

fn put_u32(v: &mut Vec<u8, BLOCK_LEN>, x: u32) -> bool {
    v.extend_from_slice(&x.to_le_bytes()).is_ok()
}

fn put_str(v: &mut Vec<u8, BLOCK_LEN>, s: &str) -> bool {
    let b = s.as_bytes();
    b.len() < 256 && put_u8(v, b.len() as u8) && v.extend_from_slice(b).is_ok()
}

/// 从当前全局状态拼出待写入的字节块
fn build() -> Option<(Buf, usize)> {
    let (ssid, pass) = state::net_copy();
    let (up_ssid, up_pass, up_on) = state::uplink_copy();

    let mut v: Vec<u8, BLOCK_LEN> = Vec::new();
    v.extend_from_slice(MAGIC).ok()?;
    if !put_u8(&mut v, VERSION) {
        return None;
    }
    // 先占两个字节的 payload 长度，稍后回填
    v.extend_from_slice(&[0, 0]).ok()?;

    let ok = put_u8(&mut v, state::enabled() as u8)
        && put_u8(&mut v, state::wide_band() as u8)
        && put_u8(&mut v, state::ap_channel())
        && put_u8(&mut v, state::route_mode())
        && put_u32(&mut v, state::fou_ip_raw())
        && put_u32(&mut v, state::fou_port() as u32)
        && put_u32(&mut v, state::up_target_raw())
        && put_u8(&mut v, up_on as u8)
        && put_str(&mut v, ssid.as_str())
        && put_str(&mut v, pass.as_str())
        && put_str(&mut v, up_ssid.as_str())
        && put_str(&mut v, up_pass.as_str());
    if !ok {
        return None;
    }

    let payload_len = (v.len() - HEADER) as u16;
    v[5..7].copy_from_slice(&payload_len.to_le_bytes());
    let h = hash(&v);
    v.extend_from_slice(&h.to_le_bytes()).ok()?;
    // NOR flash 写长度必须是 4 的倍数；padding 用 0xFF 而不是 0，
    // 这样和擦除后的空白态一致，save_to 的「内容没变就跳过」比较才准。
    while v.len() % 4 != 0 {
        v.push(0xFF).ok()?;
    }

    let mut b = Buf([0u8; BLOCK_LEN]);
    b.0[..v.len()].copy_from_slice(&v);
    Some((b, v.len()))
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn u8(&mut self) -> Option<u8> {
        let x = *self.b.get(self.p)?;
        self.p += 1;
        Some(x)
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn str(&mut self) -> Option<String<64>> {
        let n = self.u8()? as usize;
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        let mut out = String::new();
        out.push_str(core::str::from_utf8(s).ok()?).ok()?;
        Some(out)
    }
}

/// 校验并恢复到全局状态。任何一步不对就当没存过，保留默认值。
fn parse(b: &[u8]) -> bool {
    if b.len() < HEADER + 4 || &b[0..4] != MAGIC || b[4] != VERSION {
        return false;
    }
    let payload_len = u16::from_le_bytes([b[5], b[6]]) as usize;
    let end = HEADER + payload_len;
    if end + 4 > b.len() {
        return false;
    }
    let expect = u32::from_le_bytes([b[end], b[end + 1], b[end + 2], b[end + 3]]);
    if hash(&b[..end]) != expect {
        warn!("[cfg] checksum mismatch, using defaults");
        return false;
    }

    let mut c = Cursor {
        b: &b[HEADER..end],
        p: 0,
    };
    let (Some(enabled), Some(wide), Some(ch), Some(route), Some(fou_ip), Some(fou_port), Some(up_target), Some(up_on), Some(ssid), Some(pass), Some(up_ssid), Some(up_pass)) = (
        c.u8(), c.u8(), c.u8(), c.u8(), c.u32(), c.u32(), c.u32(), c.u8(),
        c.str(), c.str(), c.str(), c.str(),
    ) else {
        warn!("[cfg] truncated payload, using defaults");
        return false;
    };

    // 顺序有讲究：set_uplink 会 signal UP_DIRTY，启动时 uplink 任务还没起来，
    // 这条信号只是让首次连接多跑一次循环，无害。
    if !state::set_net(ssid.as_str(), pass.as_str()) {
        warn!("[cfg] bad AP credentials, ignored");
    }
    let _ = state::set_uplink(up_ssid.as_str(), up_pass.as_str(), up_on != 0);
    state::set_enabled(enabled != 0);
    state::set_wide_band(wide != 0);
    let _ = state::set_ap_channel(ch);
    state::set_route_mode(route);
    state::set_fou_ip(Ipv4Addr::from(fou_ip));
    state::set_fou_port(fou_port as u16);
    state::set_up_target(Ipv4Addr::from(up_target));

    info!(
        "[cfg] restored: ch={} wide={} up_on={} route={}",
        ch, wide != 0, up_on != 0, route
    );
    true
}

fn load_from(st: &mut FlashStorage<'_>) -> bool {
    let mut b = Buf([0u8; BLOCK_LEN]);
    if let Err(e) = st.read(SECTOR, &mut b.0) {
        warn!("[cfg] flash read failed: {:?}", e);
        return false;
    }
    parse(&b.0)
}

fn save_to(st: &mut FlashStorage<'_>) {
    let Some((new, len)) = build() else {
        warn!("[cfg] encode failed, not written");
        return;
    };
    // 没变化就别擦，省 flash 寿命（NOR 扇区约 10 万次）
    let mut old = Buf([0u8; BLOCK_LEN]);
    if st.read(SECTOR, &mut old.0).is_ok() && old.0[..len] == new.0[..len] {
        info!("[cfg] unchanged, skip write");
        return;
    }
    if let Err(e) = st.erase(SECTOR, SECTOR + SECTOR_LEN as u32) {
        warn!("[cfg] flash erase failed: {:?}", e);
        return;
    }
    match st.write(SECTOR, &new.0[..len]) {
        Ok(()) => info!("[cfg] saved {} bytes", len),
        Err(e) => warn!("[cfg] flash write failed: {:?}", e),
    }
}

/// 启动早期调用：Wi-Fi 初始化之前把配置恢复好。
/// 用 reborrow 临时建一个 FlashStorage，之后 Flash 外设仍可交给常驻任务。
pub fn load_early(flash: &mut FLASH<'static>) -> bool {
    let mut st = FlashStorage::new(flash.reborrow());
    let ok = load_from(&mut st);
    if !ok {
        // 首次启动 / 校验失败：把当前默认值落一次盘，
        // 下次复位就该走 restored 分支，串口能直接看到。
        info!("[cfg] no valid config, writing defaults");
        save_to(&mut st);
    }
    ok
}

/// 常驻任务：独占 Flash 外设，串行处理网页发来的保存请求。
#[embassy_executor::task]
pub async fn config_task(flash: FLASH<'static>) {
    let mut st = FlashStorage::new(flash);
    loop {
        SAVE.wait().await;
        // 合并短时间内的连续改动，避免一次表单提交擦好几次
        Timer::after(Duration::from_millis(400)).await;
        save_to(&mut st);
    }
}
