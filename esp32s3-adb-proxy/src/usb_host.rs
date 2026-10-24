//! ESP32-S3 USB OTG **Host 模式** 初始化（HAL 层）
//!
//! esp-hal 1.1.2 的 `otg_fs` 模块只实现了 device 模式，而且 `Usb::_enable()` 里
//! 把 OTG 角色信号硬编码成了 device：
//!
//! ```ignore
//! IDDIG     = High  // mini-B 侧 -> device
//! BVALID    = High  // 强制 device 模式
//! VBUSVALID = High  // 从对端接收 VBUS
//! AVALID    = Low
//! ```
//!
//! 要做 host 就得把这些反过来，其余 PHY/PAD 上电流程与 esp-hal 保持一致，
//! 然后把寄存器交给 embassy-usb-synopsys-otg 的 host 驱动。

use embassy_usb_synopsys_otg::{
    PhyType,
    host::{HostStateStorage, OtgHostInstance, on_host_interrupt},
    otg_v1::Otg,
};
use esp_hal::gpio::{InputSignal, Level};
use esp_hal::handler;
use esp_hal::interrupt::{self, Priority};
use esp_hal::peripherals::{LPWR, USB_WRAP};

/// DWC2 主机通道数。每条 pipe（控制/批量端点各一条）占用一个通道。
/// 本项目只需要：控制管道 + ADB 的 bulk IN + bulk OUT = 3 条，留 1 条余量。
pub const CHANNEL_COUNT: usize = 4;

static HOST_STATE: HostStateStorage<CHANNEL_COUNT> = HostStateStorage::new();

#[inline(always)]
fn regs() -> Otg {
    // USB0 的寄存器基址由 esp-hal 的 peripherals 提供
    unsafe { Otg::from_ptr(esp_hal::peripherals::USB0::PTR as *mut ()) }
}

/// 初始化 USB OTG 为 host 模式，返回可直接喂给 `OtgHost::new()` 的实例。
///
/// `vbus_pin`：可选的 VBUS 5V 开关 GPIO（驱动 P-MOS 或高电平使能的电源芯片）。
/// ESP32-S3 没有片上 5V，必须外部给手机的 VBUS 供电，否则手机不会枚举。
///
/// - 有可控开关（DevKitC 之类引出 GPIO 的板）→ 传 `Some(&mut pin)`
/// - **转接头 / dongle 方案，VBUS 由外部电源常供** → 传 `None`
pub fn init(vbus_pin: Option<&mut esp_hal::gpio::Output<'static>>) -> OtgHostInstance<'static> {
    // ---------- 1. PHY / PAD 上电（与 esp-hal device 模式完全一致）----------
    USB_WRAP::regs().otg_conf().modify(|_, w| {
        w.usb_pad_enable().set_bit();
        w.phy_sel().clear_bit(); // 0 = 内部 PHY
        w.clk_en().set_bit();
        w.ahb_clk_force_on().set_bit();
        w.phy_clk_force_on().set_bit()
    });

    LPWR::regs().usb_conf().modify(|_, w| {
        w.sw_hw_usb_phy_sel().set_bit();
        w.sw_usb_phy_sel().set_bit()
    });

    // ---------- 2. 角色信号：翻转成 host（A-device）----------
    InputSignal::USB_OTG_IDDIG.connect_to(&Level::Low); // mini-A 侧 = host
    InputSignal::USB_SRP_BVALID.connect_to(&Level::Low); // 不再"强制 device"
    InputSignal::USB_OTG_VBUSVALID.connect_to(&Level::High); // VBUS 由我们提供，有效
    InputSignal::USB_OTG_AVALID.connect_to(&Level::High); // A-valid

    // ---------- 3. 打开 VBUS 5V ----------
    // None = 外部电源常供（转接头/dongle 方案），这里什么都不做
    if let Some(pin) = vbus_pin {
        pin.set_high();
    }

    // ---------- 4. 挂中断 ----------
    interrupt::bind_handler(esp_hal::peripherals::Interrupt::USB, usb_irq);
    interrupt::enable(esp_hal::peripherals::Interrupt::USB, Priority::max());

    OtgHostInstance {
        regs: regs(),
        state: HOST_STATE.as_host_state(),
        fifo_depth_words: 256,
        phy_type: PhyType::InternalFullSpeed,
    }
}

#[handler(priority = Priority::max())]
fn usb_irq() {
    // HOST_STATE 是 static，这里借出来就是 'static
    unsafe { on_host_interrupt(regs(), &HOST_STATE.as_host_state()) }
}
