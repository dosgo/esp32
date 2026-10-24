//! 最小 USB 枚举 + 描述符解析：只为找到安卓手机的 ADB 接口
//!
//! ADB 的 USB 接口特征（Android 官方 adb 的 USB interface）：
//!   bInterfaceClass    = 0xFF  (Vendor Specific)
//!   bInterfaceSubClass = 0x42  (ADB)
//!   bInterfaceProtocol = 0x01
//! 接口下挂两个 bulk 端点（IN / OUT）。
//!
//! 这里刻意不做完整的 class 驱动框架，只做"枚举 -> 选配置 -> 拿到两个 bulk 端点"。

use embassy_time::{Duration, Timer};
use embassy_usb_driver::{
    Direction, EndpointAddress, EndpointInfo, EndpointType,
    host::{UsbHostAllocator, UsbPipe, pipe},
};
use embassy_usb_synopsys_otg::host::{Channel, OtgHostAllocator};

/// 分配给手机的 USB 地址
pub const DEV_ADDR: u8 = 1;

const DESC_DEVICE: u8 = 0x01;
const DESC_CONFIG: u8 = 0x02;

/// 找到的 ADB 接口信息
#[derive(Clone, Copy, Debug, Default)]
pub struct AdbInterface {
    /// 要下发的 SET_CONFIGURATION 值
    pub config_value: u8,
    pub interface_number: u8,
    /// bulk IN 端点 (端点地址含 0x80 方向位, 最大包长)
    pub ep_in: Option<(u8, u16)>,
    /// bulk OUT 端点 (端点地址, 最大包长)
    pub ep_out: Option<(u8, u16)>,
}

impl AdbInterface {
    pub fn is_complete(&self) -> bool {
        self.ep_in.is_some() && self.ep_out.is_some()
    }
}

fn get_descriptor(desc_type: u8, desc_index: u8, len: u16) -> [u8; 8] {
    [
        0x80, // bmRequestType: Device-to-host, Standard, Device
        0x06, // bRequest: GET_DESCRIPTOR
        desc_index,
        desc_type,
        0x00,
        0x00,
        (len & 0xFF) as u8,
        (len >> 8) as u8,
    ]
}

fn set_address(addr: u8) -> [u8; 8] {
    [0x00, 0x05, addr, 0x00, 0x00, 0x00, 0x00, 0x00]
}

fn set_configuration(value: u8) -> [u8; 8] {
    [0x00, 0x09, value, 0x00, 0x00, 0x00, 0x00, 0x00]
}

type CtrlPipe<'a> = Channel<'a, pipe::Control, pipe::InOut>;

fn ep0_info(mps: u16) -> EndpointInfo {
    EndpointInfo {
        addr: EndpointAddress::from_parts(0, Direction::Out),
        ep_type: EndpointType::Control,
        max_packet_size: mps,
        interval_ms: 0,
    }
}

/// 枚举并激活 ADB 接口。
///
/// `alloc` 来自 `OtgHost::allocator()`，可以随意复制，因此这里按值传。
pub async fn enumerate_adb(alloc: &OtgHostAllocator<'_>) -> Result<AdbInterface, &'static str> {
    // 复位后给设备和安卓 USB gadget 一点时间稳定
    Timer::after(Duration::from_millis(200)).await;

    // ---------- 阶段 1：默认地址 0，先用最小 8 字节 MPS 探路 ----------
    let mut pipe: CtrlPipe<'_> = alloc
        .alloc_pipe::<pipe::Control, pipe::InOut>(0, &ep0_info(8), None)
        .map_err(|_| "分配控制管道失败")?;

    let mut hdr = [0u8; 8];
    pipe.control_in(&get_descriptor(DESC_DEVICE, 0, 8), &mut hdr)
        .await
        .map_err(|_| "读取 device 描述符(8) 失败")?;
    let mps0 = core::cmp::max(hdr[7] as u16, 8);
    log::info!("[usb] ep0 MPS = {}", mps0);

    pipe.control_out(&set_address(DEV_ADDR), &[])
        .await
        .map_err(|_| "SET_ADDRESS 失败")?;
    drop(pipe);
    // 规范：SET_ADDRESS 后设备需要 2ms 完成状态阶段，留 10ms 保险
    Timer::after(Duration::from_millis(10)).await;

    // ---------- 阶段 2：用正式地址重开控制管道 ----------
    let mut pipe: CtrlPipe<'_> = alloc
        .alloc_pipe::<pipe::Control, pipe::InOut>(DEV_ADDR, &ep0_info(mps0), None)
        .map_err(|_| "重新分配控制管道失败")?;

    let mut dev = [0u8; 18];
    pipe.control_in(&get_descriptor(DESC_DEVICE, 0, 18), &mut dev)
        .await
        .map_err(|_| "读取 device 描述符(18) 失败")?;
    let vid = u16::from_le_bytes([dev[8], dev[9]]);
    let pid = u16::from_le_bytes([dev[10], dev[11]]);
    log::info!("[usb] 设备 VID={:#06x} PID={:#06x}", vid, pid);

    // 配置描述符：先读 4 字节拿总长度
    let mut cfg_hdr = [0u8; 4];
    pipe.control_in(&get_descriptor(DESC_CONFIG, 0, 4), &mut cfg_hdr)
        .await
        .map_err(|_| "读取 config 描述符头失败")?;
    let total = u16::from_le_bytes([cfg_hdr[2], cfg_hdr[3]]);
    log::info!("[usb] 配置描述符总长 = {} 字节", total);

    // 全量读取（截断到缓冲区大小）
    const CFG_BUF: usize = 512;
    let take = core::cmp::min(total as usize, CFG_BUF);
    let mut cfg = [0u8; CFG_BUF];
    let n = pipe
        .control_in(&get_descriptor(DESC_CONFIG, 0, take as u16), &mut cfg)
        .await
        .map_err(|_| "读取 config 描述符失败")?;

    let mut found = AdbInterface::default();
    found.config_value = cfg[5];

    // ---------- 解析：找 ADB 接口 + 它的两个 bulk 端点 ----------
    let mut i = 0usize;
    let mut in_adb_iface = false;
    while i + 1 < n {
        let len = cfg[i] as usize;
        let typ = cfg[i + 1];
        if len < 2 || i + len > n {
            break;
        }
        match typ {
            0x04 => {
                // Interface Descriptor
                let class = cfg[i + 5];
                let sub = cfg[i + 6];
                let proto = cfg[i + 7];
                in_adb_iface = class == 0xFF && sub == 0x42 && proto == 0x01;
                if in_adb_iface {
                    found.interface_number = cfg[i + 2];
                    log::info!("[usb] 找到 ADB 接口 #{}", cfg[i + 2]);
                } else {
                    log::info!(
                        "[usb] 接口 #{} class={:#04x} sub={:#04x} proto={:#04x} (非 ADB)",
                        cfg[i + 2],
                        class,
                        sub,
                        proto
                    );
                }
            }
            0x05 if in_adb_iface => {
                // Endpoint Descriptor，属于刚识别到的 ADB 接口
                let addr = cfg[i + 2];
                let mps = u16::from_le_bytes([cfg[i + 4], cfg[i + 5]]);
                if addr & 0x80 != 0 {
                    found.ep_in = Some((addr, mps));
                    log::info!("[usb]   ADB bulk IN  EP={:#04x} MPS={}", addr, mps);
                } else {
                    found.ep_out = Some((addr, mps));
                    log::info!("[usb]   ADB bulk OUT EP={:#04x} MPS={}", addr, mps);
                }
            }
            _ => {}
        }
        i += len;
    }

    if !found.is_complete() {
        return Err("未找到完整 ADB 接口（手机需开启 USB 调试）");
    }

    // ---------- 激活配置 ----------
    pipe.control_out(&set_configuration(found.config_value), &[])
        .await
        .map_err(|_| "SET_CONFIGURATION 失败")?;
    drop(pipe);
    Timer::after(Duration::from_millis(50)).await;

    log::info!(
        "[usb] 已激活配置 {}，ADB 接口就绪",
        found.config_value
    );
    Ok(found)
}
