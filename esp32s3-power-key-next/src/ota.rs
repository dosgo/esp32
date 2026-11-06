//! OTA 固件升级（网页上传 .bin → 写入备用分区 → 校验 → 切换 → 重启）。
//!
//! 依赖 `esp-bootloader-esp-idf 0.6` 的 `OtaUpdater`（otadata 管理、分区查找都是现成的），
//! 本文件只做三件事：
//!
//! 1. **流式接收**：POST /ota 的 body 按块收，不整块缓存（固件 ~1MB，RAM 放不下）。
//!    边收边算 SHA1，首字节必须是 0xE9（ESP 应用镜像 magic）。
//! 2. **逐扇区擦 + 写**：绝不先整分区擦完再收 —— 2MB 擦一遍要十几秒全程阻断，
//!    Wi-Fi 断流太久手机/电脑的连接会直接掉。逐扇区擦在两次网络读之间留了喘息，
//!    每次 4KB 擦除只卡 ~40ms，Wi-Fi 完全扛得住（保存配网时写 flash 就是这么跑的）。
//! 3. **回读校验**：写完把整个镜像读回来重算 SHA1 对比。校验不过就报错返回，
//!    otadata 没动、当前分区没碰，原固件继续跑，用户直接重传即可。
//!
//! 安全新保证：
//! - 写的目标永远是**另一个**分区（`next_partition()` 会跳过正在运行的分区），
//!   写坏也不影响当前系统。
//! - 校验通过才 `activate_next_partition()`（写 otadata）。在此之前无论断电还是
//!   断网，下次上电 bootloader 仍引导旧固件。
//! - 就算 otadata 指向了坏镜像，ESP-IDF bootloader 校验失败会回退到另一个有效分区，
//!   不会变砖。
//!
//! 升级后防回滚：见 `boot_confirm()` —— 开机把当前镜像状态标成 Valid，
//! 防止回滚型 bootloader 把升级退回旧分区（症状：升级成功一次、重启又变回去）。

use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN;
use embassy_net::tcp::TcpSocket;
use sha1::{Digest, Sha1};

macro_rules! dlog {
    ($($a:tt)*) => {
        crate::push_log(format_args!($($a)*))
    };
}

/// flash 扇区大小（4KB，ESP32 通用）
const SECTOR: u32 = 4096;

/// 开机确认当前镜像状态为 Valid。
///
/// 必须在 `FLASH` 全局被占走之前、用 main 里的 flash 句柄调用（同步函数）。
/// 分区表读出来没有 OTA 双分区（旧分区表固件）就静默跳过，一切照旧。
pub fn boot_confirm(flash: &mut esp_storage::FlashStorage<'_>) {
    // 分区表缓冲 0xC00 字节：放 PSRAM。放内部 .bss 会把主栈挤小 3KB —— 这块板子
    // 主栈本来就紧（堆加 1KB 栈少 1KB 的老问题），没必要再砍。
    let raw = crate::alloc_psram_buf(PARTITION_TABLE_MAX_LEN);
    let (buf, _) = raw.split_at_mut(PARTITION_TABLE_MAX_LEN);
    let buf: &mut [u8; PARTITION_TABLE_MAX_LEN] = match buf.try_into() {
        Ok(b) => b,
        Err(_) => return,
    };

    let mut updater = match OtaUpdater::new(flash, buf) {
        Ok(u) => u,
        Err(_) => {
            dlog!("[ota] 分区表没有 OTA 双分区（旧表），跳过镜像确认");
            return;
        }
    };

    match updater.selected_partition() {
        Ok(p) => dlog!("[ota] 当前启动分区 {:?}", p),
        Err(_) => {}
    }

    match updater.current_ota_state() {
        Ok(OtaImageState::Valid) => {} // 已经确认过，不用再写 flash
        Ok(s) => {
            // Undefined（otadata 空的第一跳）/ New / PendingVerify 都在这里被确认。
            // 就算 bootloader 没开回滚，多写这一下也无害。
            if updater.set_current_ota_state(OtaImageState::Valid).is_ok() {
                dlog!("[ota] 镜像状态 {:?} -> Valid（已确认）", s);
            }
        }
        Err(_) => {}
    }
}

/// 处理一次 OTA 上传（POST /ota）。
///
/// - `content_length`：请求头里的 Content-Length，即固件大小
/// - `prefix`：请求头之后已经落进 `staging` 的 body 前缀范围（含头不含尾）
/// - `staging`：调用方的缓冲复用（serve_once 的 2KB 请求缓冲，头部解析完就没用了）
///
/// 返回 Ok(成功提示) / Err(失败原因)。调用方负责把结果发给浏览器，
/// 成功后等几百毫秒再重启（把响应发完整）。
pub async fn handle_ota(
    socket: &mut TcpSocket<'_>,
    content_length: usize,
    prefix: (usize, usize),
    staging: &mut [u8],
) -> Result<&'static str, &'static str> {
    // 有效应用镜像至少要有镜像头(24B) + app 描述(256B) + 一点代码，4KB 是极宽的下限
    if content_length < 0x1000 {
        return Err("固件太小（不足 4KB），不可能是有效的应用镜像");
    }

    let mut flash_g = crate::FLASH.lock().await;
    let flash = match flash_g.as_mut() {
        Some(f) => f,
        None => return Err("flash 不可用"),
    };

    // 分区表缓冲：0xC00，从 PSRAM 出（理由同 boot_confirm）
    let raw = crate::alloc_psram_buf(PARTITION_TABLE_MAX_LEN);
    let (buf, _) = raw.split_at_mut(PARTITION_TABLE_MAX_LEN);
    let pt_buf: &mut [u8; PARTITION_TABLE_MAX_LEN] = match buf.try_into() {
        Ok(b) => b,
        Err(_) => return Err("内部错误：分区表缓冲分配失败"),
    };

    let mut updater = match OtaUpdater::new(flash, pt_buf) {
        Ok(u) => u,
        Err(_) => {
            return Err("分区表里没有 OTA 双分区。请先用 espflash -T partitions.csv 重新烧录一次（只需一次，见 README）")
        }
    };

    // region 借着 updater，用块作用域把生命周期限制住，出来才能 activate
    let subtype = {
        let (mut region, subtype) = match updater.next_partition() {
            Ok(v) => v,
            Err(_) => return Err("找不到备用 OTA 分区"),
        };

        if content_length > region.partition_size() {
            return Err("固件比 OTA 分区还大，拒绝写入");
        }

        dlog!(
            "[ota] 开始升级 -> {:?}，{}KB",
            subtype,
            content_length / 1024
        );

        // ---------- 流式接收 + 逐扇区擦写 ----------
        let mut hasher = Sha1::new();
        let mut received = 0usize; // 已收 body 字节数
        let mut flash_off = 0u32; // 已写入的分区内偏移（含 4 字节补齐）
        let mut erased_to = 0u32; // 已擦到的分区内偏移（扇区对齐）

        // 先把请求头后面已收到的 body 前缀搬到 staging 开头
        // （copy_within = memmove，prefix 和目标区可能重叠）
        let mut fill = 0usize;
        if prefix.1 > prefix.0 {
            let n = (prefix.1 - prefix.0).min(staging.len());
            staging.copy_within(prefix.0..prefix.0 + n, 0);
            fill = n;
        }

        while received < content_length {
            // staging 不满、body 没收完 → 继续从 socket 读
            while fill < staging.len() && received + fill < content_length {
                match socket.read(&mut staging[fill..]).await {
                    Ok(0) => return Err("连接中断，固件没有传完（原固件未受影响，可重试）"),
                    Ok(n) => fill += n,
                    Err(_) => return Err("连接中断（超时或断网）。原固件未受影响，可重试"),
                }
            }
            if fill == 0 {
                return Err("没有收到数据");
            }

            // 首字节必须是 0xE9（ESP 应用镜像 magic）—— 最常见的错误是
            // 传了 elf / 全量 flash 镜像 / 别的文件，在这里就能拦下来
            if flash_off == 0 && staging[0] != 0xE9 {
                return Err("这不是 ESP32 应用镜像（开头不是 0xE9）。请上传 espflash save-image 导出的 .bin");
            }

            received += fill;
            hasher.update(&staging[..fill]);

            // flash 写入要求 4 字节对齐：只有最后一块可能不足，补 0xFF
            // （NOR flash 擦除态就是 0xFF，补齐不影响镜像内容）
            let mut wlen = fill;
            if received == content_length && wlen % 4 != 0 {
                let pad = 4 - (wlen % 4);
                staging[wlen..wlen + pad].fill(0xFF);
                wlen += pad;
            }
            if wlen % 4 != 0 {
                return Err("内部错误：写入长度未按 4 字节对齐");
            }

            // 逐扇区擦：保证 [flash_off, flash_off+wlen) 已擦除。
            // 每次只擦一个扇区（~40ms），两个扇区之间网络是通的，
            // 上传连接不会被长时间断流挤掉。
            let end = flash_off + wlen as u32;
            let need_to = (end + SECTOR - 1) / SECTOR * SECTOR;
            while erased_to < need_to {
                if region.erase(erased_to, erased_to + SECTOR).is_err() {
                    return Err("flash 擦除失败（扇区擦不动，一般意味着 flash 硬件有问题）");
                }
                erased_to += SECTOR;
            }

            if region.write(flash_off, &staging[..wlen]).is_err() {
                return Err("flash 写入失败。原固件未受影响，可重试");
            }
            flash_off += wlen as u32;
            fill = 0;
        }

        // ---------- 回读校验（SHA1，只校验原始内容，不含补的 0xFF）----------
        let expect = hasher.finalize();
        let mut vh = Sha1::new();
        let mut off = 0u32;
        let mut remaining = content_length;
        while remaining > 0 {
            let n = remaining.min(staging.len());
            if region.read(off, &mut staging[..n]).is_err() {
                return Err("flash 回读失败");
            }
            vh.update(&staging[..n]);
            off += n as u32;
            remaining -= n;
        }
        let actual = vh.finalize();
        let mut same = true;
        for i in 0..20 {
            if expect[i] != actual[i] {
                same = false;
            }
        }
        if !same {
            return Err("校验失败：写入的内容回读不一致。备用分区已作废但 otadata 没动，原固件不受影响，可重试");
        }

        dlog!("[ota] 已写入 {}KB 并通过校验", content_length / 1024);
        subtype
    }; // region 的借用到这里结束

    // ---------- 切换启动分区（写 otadata）----------
    // 从这里开始：下次上电就会引导新固件。
    if updater.activate_next_partition().is_err() {
        return Err("切换启动分区失败（otadata 写入出错），原固件不受影响");
    }
    dlog!("[ota] 已切换启动分区 -> {:?}，准备重启", subtype);

    Ok("固件升级成功，设备正在重启…")
}
