//! USB 大容量存储（MSC / U 盘）—— 让 TF 卡直接出现在电脑里。
//!
//! ## 为什么自己实现
//!
//! `embassy-usb` 0.6.0（crates.io 上的最新发布版，2026-03）**没有** MSC 设备类，
//! 上游 main 分支的实现是 2026-10 才重构完、还没发布，且依赖新的模块结构，搬不动。
//! 所以这里按 USB MSC BBB 规范（Mass Storage Class Bulk-Only Transport r1.0）
//! 写一个够 Windows / Linux / macOS 认盘的最小实现。
//!
//! ## 协议分层
//!
//! ```text
//! 主机                         设备（本文件）
//!  ├─ CBW (31B)  ───────────►  解析 tag / 数据长度 / 方向 / CDB
//!  ├─ 数据阶段（可选） ◄─────►  按块读写 TF 卡
//!  └─ CSW (13B)  ◄──────────  回 tag + residue + 状态
//! ```
//!
//! SCSI 命令只实现 U 盘必备子集：INQUIRY / TEST UNIT READY / READ CAPACITY /
//! MODE SENSE / READ / WRITE / VERIFY / SYNCHRONIZE CACHE，另外把
//! PREVENT ALLOW、START STOP 当成空操作（Windows 弹盘、休眠恢复会发）。
//!
//! ## 和 HID 共存
//!
//! 同一个 configuration 里多一个 interface 而已：HID 走 interrupt 端点，
//! MSC 走一对 bulk 端点，Windows 会分别枚举成「键盘/鼠标/媒体键」和「可移动磁盘」，
//! 互不影响。目前一共 4 个 interface，正好卡在 `max-interface-count-4` 上限。

use core::cmp::min;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::yield_now;
use embassy_usb::{
    Builder, Handler,
    control::{InResponse, OutResponse, Request, RequestType},
    driver::{Driver, Endpoint, EndpointError, EndpointIn, EndpointOut},
};
use embassy_time::{Duration, with_timeout};

use crate::sdcard::{BLOCK_SIZE, SdStorage};

/// MSC class code（interface descriptor 用）
const MSC_CLASS: u8 = 0x08;
/// SCSI transparent command set
const MSC_SUBCLASS: u8 = 0x06;
/// Bulk-Only Transport
const MSC_PROTOCOL: u8 = 0x50;

/// GET MAX LUN：主机问"你挂了几个 LUN"
const REQ_GET_MAX_LUN: u8 = 0xFE;
/// Bulk-Only Mass Storage Reset
const REQ_BULK_RESET: u8 = 0xFF;

/// CBW 固定 31 字节，CSW 固定 13 字节
const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;
/// CBW 签名 "USBC"（小端 0x43425355）
const CBW_SIG: [u8; 4] = [0x55, 0x53, 0x42, 0x43];
/// CSW 签名 "USBS"
const CSW_SIG: [u8; 4] = [0x55, 0x53, 0x42, 0x53];

/// 复位标志：主机发 Bulk-Only Reset 时置位，MSC 任务看到就丢掉当前状态重来。
/// 单实例，用静态变量比在 handler 和任务之间传引用省事。
static BULK_RESET: AtomicBool = AtomicBool::new(false);

// ==================== 控制请求 ====================

/// MSC 的 class-specific 控制请求。
///
/// 这两个请求不属于标准请求，USB 栈自己不会处理，必须由 class 的 Handler 接。
pub struct MscHandler;

impl Handler for MscHandler {
    /// GET MAX LUN（bmRequestType=0xA1，方向 IN，长度 1）
    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        if req.request_type == RequestType::Class && req.request == REQ_GET_MAX_LUN {
            if buf.is_empty() {
                return Some(InResponse::Rejected);
            }
            // 只有 1 个 LUN（LUN 0）
            buf[0] = 0;
            return Some(InResponse::Accepted(&buf[..1]));
        }
        None
    }

    /// Bulk-Only Mass Storage Reset（bmRequestType=0x21，方向 OUT，无数据）
    fn control_out(&mut self, req: Request, _data: &[u8]) -> Option<OutResponse> {
        if req.request_type == RequestType::Class && req.request == REQ_BULK_RESET {
            BULK_RESET.store(true, Ordering::Release);
            return Some(OutResponse::Accepted);
        }
        None
    }
}

// ==================== 设备本体 ====================

pub struct Msc<'d, D: Driver<'d>> {
    ep_in: D::EndpointIn,
    ep_out: D::EndpointOut,
    /// 没插卡时是 None —— 依然枚举成一个"空读卡器"，只是读写全部报 NOT READY
    sd: Option<SdStorage>,
    /// 上一次命令失败的 sense（REQUEST SENSE 要回给主机）
    sense_key: u8,
    sense_asc: u8,
    sense_ascq: u8,
}

impl<'d, D: Driver<'d>> Msc<'d, D> {
    /// 在 USB 描述符里加一个 MSC interface（双 bulk 端点），返回设备对象。
    pub fn new(builder: &mut Builder<'d, D>, sd: Option<SdStorage>) -> Self {
        let mut func = builder.function(MSC_CLASS, MSC_SUBCLASS, MSC_PROTOCOL);
        let mut iface = func.interface();
        let mut alt = iface.alt_setting(MSC_CLASS, MSC_SUBCLASS, MSC_PROTOCOL, None);

        // 全速设备 bulk 端点最大包长只能是 64
        let ep_out = alt.endpoint_bulk_out(None, 64);
        let ep_in = alt.endpoint_bulk_in(None, 64);

        Self {
            ep_in,
            ep_out,
            sd,
            sense_key: 0,
            sense_asc: 0,
            sense_ascq: 0,
        }
    }

    /// 主循环。端点被主机使能（SET_CONFIGURATION）后才开始收 CBW。
    pub async fn run(mut self) -> ! {
        loop {
            self.ep_in.wait_enabled().await;
            self.ep_out.wait_enabled().await;

            // serve() 只在端点出错或主机要求复位时返回
            let _ = self.serve().await;
            self.flush_out().await;
        }
    }

    /// 把 OUT 端点里残留的数据读掉（复位后旧 CBW 还在 FIFO 里，
    /// 不清掉会拿它当新命令解析，后面整条链路错位）
    async fn flush_out(&mut self) {
        let mut tmp = [0u8; 64];
        loop {
            match with_timeout(Duration::from_millis(20), self.ep_out.read(&mut tmp)).await {
                Ok(Ok(n)) if n > 0 => continue,
                _ => break,
            }
        }
    }

    /// 把 OUT 端点里"还在路上"的数据读掉。
    ///
    /// 写卡失败时主机往往已经把一整段数据推过来了，直接回 CSW 会让剩下的包
    /// 堆在端点里，下一条 CBW 就从这个偏移开始读 —— 整条链路错位。
    async fn drain_out(&mut self, mut bytes: usize) {
        let mut tmp = [0u8; 64];
        while bytes > 0 {
            let n = min(bytes, 64);
            match with_timeout(Duration::from_millis(300), self.ep_out.read(&mut tmp[..n])).await {
                Ok(Ok(r)) if r > 0 => bytes -= min(r, bytes),
                _ => break,
            }
        }
    }

    /// CBW → 数据 → CSW 循环
    async fn serve(&mut self) -> Result<(), EndpointError> {
        let mut buf = [0u8; BLOCK_SIZE];
        let mut cbw = [0u8; CBW_LEN];

        loop {
            if BULK_RESET.swap(false, Ordering::AcqRel) {
                return Ok(());
            }

            let n = self.ep_out.read(&mut cbw).await?;
            if n != CBW_LEN || cbw[0..4] != CBW_SIG {
                // 不是合法 CBW：丢弃这次，继续等下一个
                continue;
            }

            let tag = [cbw[4], cbw[5], cbw[6], cbw[7]];
            let data_len =
                u32::from_le_bytes([cbw[8], cbw[9], cbw[10], cbw[11]]) as usize;
            let dir_in = cbw[12] & 0x80 != 0;
            let cb_len = min((cbw[14] & 0x1F) as usize, 16);
            let cdb = &cbw[15..15 + cb_len];

            let (status, residue) = self.dispatch(cdb, data_len, dir_in, &mut buf).await?;

            // 数据阶段失败时，主机还在那儿等剩下的字节 —— 必须用填充字节把
            // dCBWDataTransferLength 补满再回 CSW，否则主机会一直等到 5 秒超时，
            // 现象就是"U 盘一读某个文件就卡死"。
            //
            // 注意不能用零长度包收尾：esp-hal 的 EndpointIn::write 对空切片
            // 直接返回 Ok、根本不启动传输，ZLP 发不出去。
            let mut residue = residue;
            if status != 0 && dir_in && residue > 0 {
                let fill = [0xFFu8; 64];
                while residue > 0 {
                    let n = min(residue, 64);
                    if self.ep_in.write_transfer(&fill[..n], false).await.is_err() {
                        break;
                    }
                    residue -= n;
                }
                residue = 0;
            }

            let csw = make_csw(tag, residue as u32, status);
            self.ep_in.write_transfer(&csw, false).await?;
        }
    }

    /// 执行一条 SCSI 命令，返回 (CSW 状态, 残留字节数)
    ///
    /// 状态：0=成功 1=失败 2=阶段错误
    ///
    /// `_dir_in` 是 CBW 里声明的数据方向，这里不拿它做判断 ——
    /// 方向和长度都是由具体 SCSI 命令决定的（READ 一定 IN、WRITE 一定 OUT），
    /// 主机的 CBW 标志位只用来在出错时决定要不要补零长度包。
    async fn dispatch(
        &mut self,
        cdb: &[u8],
        data_len: usize,
        _dir_in: bool,
        buf: &mut [u8; BLOCK_SIZE],
    ) -> Result<(u8, usize), EndpointError> {
        if cdb.is_empty() {
            self.set_sense(0x05, 0x20, 0x00);
            return Ok((1, data_len));
        }

        match cdb[0] {
            // ---- TEST UNIT READY ----
            0x00 => {
                if self.sd.is_none() {
                    self.set_sense(0x02, 0x3A, 0x00); // NOT READY / MEDIUM NOT PRESENT
                    return Ok((1, data_len));
                }
                self.clear_sense();
                Ok((0, 0))
            }

            // ---- REQUEST SENSE ----
            0x03 => {
                let p = self.sense_page();
                let n = min(data_len, p.len());
                self.ep_in.write_transfer(&p[..n], false).await?;
                // sense 读一次就清掉（SCSI 惯例）
                self.clear_sense();
                Ok((0, data_len - n))
            }

            // ---- INQUIRY ----
            0x12 => {
                let evpd = cdb.len() > 1 && (cdb[1] & 0x01) != 0;
                let page = cdb.get(2).copied().unwrap_or(0);
                if evpd {
                    let (p, plen) = match page {
                        // 支持的 VPD 页列表
                        0x00 => {
                            let mut r = [0u8; 12];
                            r[1] = 0x00;
                            r[3] = 2;
                            r[4] = 0x00;
                            r[5] = 0x80;
                            (r, 6usize)
                        }
                        // 序列号
                        0x80 => {
                            let mut r = [0u8; 12];
                            r[1] = 0x80;
                            r[3] = 8;
                            r[4..12].copy_from_slice(b"00000001");
                            (r, 12)
                        }
                        _ => {
                            self.set_sense(0x05, 0x24, 0x00); // INVALID FIELD IN CDB
                            return Ok((1, data_len));
                        }
                    };
                    let n = min(data_len, plen);
                    self.ep_in.write_transfer(&p[..n], false).await?;
                    Ok((0, data_len - n))
                } else {
                    let mut r = [0u8; 36];
                    // 字节 0：外设类型 0x00 = 直接存取块设备
                    r[0] = 0x00;
                    // 字节 1：0x80 = 可移动介质
                    r[1] = 0x80;
                    r[2] = 0x02; // SPC-3
                    r[3] = 0x02; // 响应数据格式
                    r[4] = 31; // 后续字节数
                    r[8..16].copy_from_slice(b"DOSGO   ");
                    r[16..32].copy_from_slice(b"PocketDongle SD ");
                    r[32..36].copy_from_slice(b"1.00");
                    let n = min(data_len, 36);
                    self.ep_in.write_transfer(&r[..n], false).await?;
                    Ok((0, data_len - n))
                }
            }

            // ---- MODE SENSE(6) ----
            0x1A => {
                // 4 字节头：长度=3，介质类型=0，设备参数=0，块描述符长度=0
                let r = [0x03u8, 0x00, 0x00, 0x00];
                let n = min(data_len, r.len());
                self.ep_in.write_transfer(&r[..n], false).await?;
                Ok((0, data_len - n))
            }

            // ---- MODE SENSE(10) ----
            0x5A => {
                let r = [0u8, 0, 0, 6, 0, 0, 0, 0];
                let n = min(data_len, r.len());
                self.ep_in.write_transfer(&r[..n], false).await?;
                Ok((0, data_len - n))
            }

            // ---- READ CAPACITY(10) ----
            0x25 => {
                let Some(sd) = &self.sd else {
                    self.set_sense(0x02, 0x3A, 0x00);
                    return Ok((1, data_len));
                };
                let last = sd.blocks().saturating_sub(1);
                let mut r = [0u8; 8];
                r[0..4].copy_from_slice(&last.to_be_bytes());
                r[4..8].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
                let n = min(data_len, 8);
                self.ep_in.write_transfer(&r[..n], false).await?;
                Ok((0, data_len - n))
            }

            // ---- READ CAPACITY(16) ----
            0x9E if (cdb.get(1).copied().unwrap_or(0) & 0x1F) == 0x10 => {
                let Some(sd) = &self.sd else {
                    self.set_sense(0x02, 0x3A, 0x00);
                    return Ok((1, data_len));
                };
                let last = sd.blocks().saturating_sub(1) as u64;
                let mut r = [0u8; 32];
                r[0..8].copy_from_slice(&last.to_be_bytes());
                r[8..12].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
                let n = min(data_len, 32);
                self.ep_in.write_transfer(&r[..n], false).await?;
                Ok((0, data_len - n))
            }

            // ---- READ(10) / READ(12) ----
            0x28 | 0xA8 => {
                let (lba, count) = parse_rw_cdb(cdb);
                let Some(sd) = &self.sd else {
                    self.set_sense(0x02, 0x3A, 0x00);
                    return Ok((1, data_len));
                };
                if count == 0 {
                    return Ok((0, data_len));
                }
                // 数据阶段长度以 CBW 为准，别信 CDB 的 count（主机可能缩过）
                let want = min(data_len, count.saturating_mul(BLOCK_SIZE as u32) as usize);
                let mut sent = 0usize;
                while sent < want {
                    let off = (sent / BLOCK_SIZE) as u32;
                    if !sd.read_block(lba.saturating_add(off), buf) {
                        self.set_sense(0x03, 0x11, 0x00); // MEDIUM ERROR / UNRECOVERED READ
                        // 剩下的字节由 serve() 统一用 0xFF 补齐（ZLP 发不出去）
                        return Ok((1, data_len - sent));
                    }
                    let n = min(BLOCK_SIZE, want - sent);
                    self.ep_in.write_transfer(&buf[..n], false).await?;
                    sent += n;
                    // 128 块的一次读要 ~40ms，别一直霸占执行器（WiFi/BLE 还在跑）
                    yield_now().await;
                }
                Ok((0, data_len - sent))
            }

            // ---- WRITE(10) / WRITE(12) ----
            0x2A | 0xAA => {
                let (lba, count) = parse_rw_cdb(cdb);
                if self.sd.is_none() {
                    self.set_sense(0x02, 0x3A, 0x00);
                    return Ok((1, data_len));
                }
                if count == 0 {
                    return Ok((0, 0));
                }
                let want = min(data_len, count.saturating_mul(BLOCK_SIZE as u32) as usize);
                let mut got = 0usize;
                let mut filled = 0usize;
                while got < want {
                    let room = min(64, BLOCK_SIZE - filled);
                    let n = self.ep_out.read(&mut buf[filled..filled + room]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                    got += n;
                    if filled == BLOCK_SIZE {
                        let off = (got / BLOCK_SIZE - 1) as u32;
                        // 用 as_ref() 临时借用：drain_out 要 &mut self，
                        // 这里不能把 sd 的借用留到那个调用之后
                        let ok = self
                            .sd
                            .as_ref()
                            .map(|s| s.write_block(lba.saturating_add(off), buf))
                            .unwrap_or(false);
                        if !ok {
                            self.set_sense(0x03, 0x0C, 0x00); // WRITE ERROR
                            // 主机可能还有半截数据在路上，读掉它，
                            // 否则残留在端点里会让下一个 CBW 错位
                            self.drain_out(data_len - got).await;
                            return Ok((1, 0));
                        }
                        filled = 0;
                        yield_now().await;
                    }
                }
                Ok((0, data_len - got))
            }

            // ---- VERIFY(10) / SYNCHRONIZE CACHE(10)：认了就行，不用真做 ----
            0x2F | 0x35 => Ok((0, data_len)),

            // ---- PREVENT ALLOW MEDIUM REMOVAL / START STOP UNIT ----
            0x1E | 0x1B => Ok((0, 0)),

            // ---- 其它命令：拒绝，让主机知道我们没实现 ----
            _ => {
                self.set_sense(0x05, 0x20, 0x00); // ILLEGAL REQUEST / INVALID COMMAND
                Ok((1, data_len))
            }
        }
    }

    fn set_sense(&mut self, key: u8, asc: u8, ascq: u8) {
        self.sense_key = key;
        self.sense_asc = asc;
        self.sense_ascq = ascq;
    }

    fn clear_sense(&mut self) {
        self.sense_key = 0;
        self.sense_asc = 0;
        self.sense_ascq = 0;
    }

    /// 18 字节固定格式 sense 数据
    fn sense_page(&self) -> [u8; 18] {
        let mut r = [0u8; 18];
        r[0] = 0x70; // 当前错误，固定格式
        r[2] = self.sense_key;
        r[7] = 10; // 后续字节数
        r[12] = self.sense_asc;
        r[13] = self.sense_ascq;
        r
    }
}

/// 组 CSW
fn make_csw(tag: [u8; 4], residue: u32, status: u8) -> [u8; CSW_LEN] {
    let mut c = [0u8; CSW_LEN];
    c[0..4].copy_from_slice(&CSW_SIG);
    c[4..8].copy_from_slice(&tag);
    c[8..12].copy_from_slice(&residue.to_le_bytes());
    c[12] = status;
    c
}

/// 从 READ/WRITE 的 CDB 里取 (起始 LBA, 块数)
///
/// READ/WRITE(10)：LBA 在 2..6，长度在 7..9（2 字节）
/// READ/WRITE(12)：LBA 在 2..6，长度在 6..10（4 字节）
fn parse_rw_cdb(cdb: &[u8]) -> (u32, u32) {
    if cdb.len() < 10 {
        return (0, 0);
    }
    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
    let count = if cdb[0] == 0xA8 || cdb[0] == 0xAA {
        if cdb.len() < 10 {
            0
        } else {
            u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]])
        }
    } else {
        u16::from_be_bytes([cdb[7], cdb[8]]) as u32
    };
    (lba, count)
}
