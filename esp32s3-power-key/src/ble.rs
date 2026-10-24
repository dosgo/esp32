//! BLE HID（HOGP）—— 让这块 dongle 变成手机 / 平板能直接用的蓝牙键鼠
//!
//! ## 为什么是这个形态
//!
//! esp-radio 只有 BLE 控制器（没有经典蓝牙 BR/EDR），所以只能走 **HID over GATT**。
//! 代价写清楚：BIOS / 装机界面 / Win7 不认 BLE 键盘，那些场景只有 USB 那条路能用。
//!
//! ## 服务和特征
//!
//! ```text
//! 0x1800 GAP     0x2A00 名字 / 0x2A01 外观（0x03C1 = 键盘）
//! 0x1801 GATT
//! 0x1812 HID     0x2A4A HID Information   （bcdHID=0x0111, country=0, flags=0x02）
//!                0x2A4B Report Map        （下面的 REPORT_MAP，靠 Read Blob 分片读）
//!                0x2A4E Protocol Mode     （0=Boot, 1=Report）
//!                0x2A4C Control Point     （suspend / exit suspend，只记日志）
//!                0x2A22 Boot 键盘输入报告  （8 字节，Read + Notify）
//!                0x2A32 Boot 鼠标输入报告  （4 字节，Read + Notify）
//!                0x2A4D Report × 3        （Report ID 1/2/3 = 键盘/鼠标/媒体键，
//!                                          每个带 0x2908 Report Reference 描述符）
//! ```
//!
//! Boot 报告和 Report 报告**两条都给**：安卓 / iOS 通常走 Report 协议（看 Report Map），
//! 少数主机只认 Boot 报告。通知时两条都发，没订阅的那条 `notify()` 直接返回 Ok，
//! 不浪费带宽。
//!
//! ## 加密
//!
//! 这一版**没开 SMP 配对**（trouble 的 `security` feature）。不加密的 HID 安卓 / iOS
//! 一般都能用；哪台主机拒绝，再开 security + bond 存 flash。
//!
//! ## 数据流
//!
//! 网页键鼠（`/kvm`）/ Blinker → `push_hid()` → 同时进 `HID_Q`（USB）和 `BLE_Q`（这里）
//! → 本任务把 `HidEvent` 转成 HID 报告 notify 出去。两路互不影响：蓝牙没连上就丢。

use bt_hci::controller::ExternalController;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
// GATT 属性表的锁必须用 trouble 内部那一份 embassy-sync（0.7），
// 用本工程主体的 0.8 会报 "RawMutex is not implemented"（两个 crate 版本不同）
use embassy_sync_07::blocking_mutex::raw::CriticalSectionRawMutex as BleMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::peripherals::BT;
use esp_hal::rng::Rng;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;

use crate::HidEvent;

/// 同时允许的连接数（一条足够，多主机切换属于后续工作）
const CONNS: usize = 1;
/// L2CAP 通道数（ATT 一条 + 余量）
const CHANNELS: usize = 2;
/// HCI 包缓冲个数
const HCI_PACKETS: usize = 4;
/// 属性表上限（当前用到 39 条）
const ATT_MAX: usize = 48;
/// CCCD 表上限（5 个可通知特征）
const CCCD_MAX: usize = 6;

/// 广播名（手机蓝牙列表里看到的就是它）
const BLE_NAME: &[u8] = b"ESP32-POWER-BLE";

/// 固定静态随机地址：手机记住这台设备之后，换固件也不会变
const BLE_ADDR: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0xC0];

/// 网页 / Blinker 下发的键鼠事件往这里塞（和 USB 那条队列并行）
pub static BLE_Q: Channel<CriticalSectionRawMutex, HidEvent, 16> = Channel::new();

/// 记前几次 GATT 读（主机的服务发现），只用来打日志
static READ_CNT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
use core::sync::atomic::Ordering;

/// 控制器类型：esp-radio 的 HCI 连接器 + bt-hci 的通用控制器壳
type BleController = ExternalController<BleConnector<'static>, HCI_PACKETS>;
type GattServer =
    AttributeServer<'static, BleMutex, DefaultPacketPool, ATT_MAX, CCCD_MAX, CONNS>;

// ===================== HID 报告描述符（BLE 版）=====================
//
// 和 USB 那份（main.rs 的 HID_REPORT_DESC）的区别：这里每个报告必须带 Report ID，
// 因为一个 GATT 服务里塞了三个逻辑设备。
//
//   Report ID 1 = 键盘（8 字节：[mod, 0, k1..k6]）
//   Report ID 2 = 鼠标（4 字节：[btn, dx, dy, wheel]）
//   Report ID 3 = 媒体键（2 字节，16 位 consumer usage）
const REPORT_MAP: &[u8] = &[
    // ---- 键盘 (Report ID 1) ----
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID 1
    0x05, 0x07, //   Usage Page (Keyboard)
    0x19, 0xE0, //   Usage Min (0xE0)
    0x29, 0xE7, //   Usage Max (0xE7)
    0x15, 0x00, //   Logical Min 0
    0x25, 0x01, //   Logical Max 1
    0x75, 0x01, //   Report Size 1
    0x95, 0x08, //   Report Count 8     -> 8 个修饰键
    0x81, 0x02, //   Input (Data,Var,Abs)
    0x95, 0x01, //   Report Count 1
    0x75, 0x08, //   Report Size 8      -> 保留字节
    0x81, 0x03, //   Input (Cnst)
    0x95, 0x06, //   Report Count 6     -> 6 个同时按下的键
    0x75, 0x08, //   Report Size 8
    0x15, 0x00, //   Logical Min 0
    0x25, 0x65, //   Logical Max 0x65
    0x05, 0x07, //   Usage Page (Keyboard)
    0x19, 0x00, //   Usage Min 0
    0x29, 0x65, //   Usage Max 0x65
    0x81, 0x00, //   Input (Data,Ary,Abs)
    0xC0, // End Collection
    // ---- 鼠标 (Report ID 2) ----
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x02, //   Report ID 2
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Buttons)
    0x19, 0x01, //     Usage Min 1
    0x29, 0x05, //     Usage Max 5
    0x15, 0x00, //     Logical Min 0
    0x25, 0x01, //     Logical Max 1
    0x75, 0x01, //     Report Size 1
    0x95, 0x05, //     Report Count 5   -> 5 个按键位
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x75, 0x03, //     Report Size 3
    0x95, 0x01, //     Report Count 1
    0x81, 0x03, //     Input (Cnst)     -> 补齐 3 位
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Min -127
    0x25, 0x7F, //     Logical Max 127
    0x75, 0x08, //     Report Size 8
    0x95, 0x03, //     Report Count 3   -> X / Y / Wheel
    0x81, 0x06, //     Input (Data,Var,Rel)
    0xC0, //   End Collection
    0xC0, // End Collection
    // ---- 媒体键 (Report ID 3) ----
    0x05, 0x0C, // Usage Page (Consumer)
    0x09, 0x01, // Usage (Consumer Control)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x03, //   Report ID 3
    0x75, 0x10, //   Report Size 16
    0x95, 0x01, //   Report Count 1
    0x15, 0x00, //   Logical Min 0
    0x26, 0xFF, 0x03, //   Logical Max 1023
    0x19, 0x00, //   Usage Min 0
    0x2A, 0xFF, 0x03, //   Usage Max 1023
    0x81, 0x00, //   Input (Data,Ary,Abs)
    0xC0, // End Collection
];

/// HID Information：bcdHID 0x0111 / country 0 / flags 0x02（normally connectable）
const HID_INFO: &[u8] = &[0x11, 0x01, 0x00, 0x02];
/// 外观：0x03C1 = HID Keyboard
const APPEARANCE: &[u8] = &[0xC1, 0x03];
/// Report Reference 描述符（0x2908）的值：[report_id, report_type]，1 = Input
const RR_KB: [u8; 2] = [1, 1];
const RR_MS: [u8; 2] = [2, 1];
const RR_CC: [u8; 2] = [3, 1];

/// 手写广播数据（28 字节）：
///   Flags(3) + 16 位服务 UUID 0x1812(4) + 外观 0x03C1(4) + 完整名字(17)
///
/// 为什么不用 `AdStructure`：trouble 的 `AdStructure` 没有 Appearance 字段，
/// 而安卓要靠它把设备归类成键盘（否则显示的图标/分类是通用设备）。
const ADV_DATA: &[u8] = &[
    0x02, 0x01, 0x06, // Flags: LE General Discoverable | BR/EDR Not Supported
    0x03, 0x03, 0x12, 0x18, // 16-bit Service UUIDs: 0x1812 (HID)
    0x03, 0x19, 0xC1, 0x03, // Appearance: 0x03C1 HID Keyboard
    0x0E, 0x09, b'E', b'S', b'P', b'3', b'2', b'-', b'P', b'O', b'W', b'E', b'R', b'-', b'B',
    b'L', b'E', // Complete Local Name
];

// ===================== 随机数（给 SMP 播种）=====================
//
// esp-hal 的 `Rng` 只有 `random() -> u32`，没实现 rand_core 的 trait，
// 而 trouble 配对的密钥材料要求 CSPRNG 播种，所以这里套一层。
// S3 的 RNG 是硬件真随机（Wi-Fi/BT 熵源），拿去生成 LTK 够用。
struct HalRng(Rng);

impl rand_core::RngCore for HalRng {
    fn next_u32(&mut self) -> u32 {
        self.0.random()
    }
    fn next_u64(&mut self) -> u64 {
        ((self.0.random() as u64) << 32) | self.0.random() as u64
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let v = self.0.random().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
impl rand_core::CryptoRng for HalRng {}

// ===================== 静态资源 =====================

static RESOURCES: static_cell::StaticCell<HostResources<DefaultPacketPool, CONNS, CHANNELS>> =
    static_cell::StaticCell::new();

static SERVER: static_cell::StaticCell<GattServer> = static_cell::StaticCell::new();

/// 主栈空闲空间（从链接脚本符号算），用来判断还能不能再加东西
fn stack_headroom() -> usize {
    unsafe extern "C" {
        static _stack_start: u32;
        static _stack_end: u32;
    }
    let start = core::ptr::addr_of!(_stack_start) as usize;
    let end = core::ptr::addr_of!(_stack_end) as usize;
    start.saturating_sub(end)
}

macro_rules! bl {
    ($($a:tt)*) => {{
        crate::push_log(format_args!($($a)*));
        log::info!($($a)*);
    }};
}

/// 通知用的句柄集合
#[derive(Clone, Copy)]
struct HidHandles {
    /// Boot 键盘报告（0x2A22，无 Report ID）
    kb_boot: Characteristic<[u8; 8]>,
    /// Boot 鼠标报告（0x2A32）
    ms_boot: Characteristic<[u8; 4]>,
    /// Report 协议：键盘（ID 1）
    kb: Characteristic<[u8; 8]>,
    /// Report 协议：鼠标（ID 2）
    ms: Characteristic<[u8; 4]>,
    /// Report 协议：媒体键（ID 3）
    cc: Characteristic<[u8; 2]>,
    /// Control Point 句柄（只用来认日志）
    cp: u16,
}

/// 建 GATT 表，返回服务端引用和通知句柄
fn build_gatt_table() -> (&'static GattServer, HidHandles) {
    let mut table: AttributeTable<'static, BleMutex, ATT_MAX> = AttributeTable::new();

    // ---- GAP ----
    let mut gap = table.add_service(Service::new(0x1800u16));
    let _ = gap.add_characteristic_ro(0x2a00u16, BLE_NAME);
    let _ = gap.add_characteristic_ro(0x2a01u16, APPEARANCE);
    gap.build();

    // ---- GATT（必须有，否则服务发现会报错）----
    table.add_service(Service::new(0x1801u16)).build();

    // ---- HID ----
    let mut hid = table.add_service(Service::new(0x1812u16));

    let _ = hid.add_characteristic_ro(0x2a4a_u16, HID_INFO);
    let _ = hid.add_characteristic_ro(0x2a4b_u16, REPORT_MAP);

    // Protocol Mode：0 = Boot，1 = Report
    let _ = hid
        .add_characteristic_small(
            0x2a4e_u16,
            &[
                CharacteristicProp::Read,
                CharacteristicProp::WriteWithoutResponse,
            ],
            1u8,
        )
        .build();

    // Control Point：0 = suspend，1 = exit suspend
    let cp: Characteristic<u8> = hid
        .add_characteristic_small(
            0x2a4c_u16,
            &[CharacteristicProp::WriteWithoutResponse],
            0u8,
        )
        .build();

    // Boot 报告
    let kb_boot: Characteristic<[u8; 8]> = hid
        .add_characteristic_small(
            0x2a22_u16,
            &[CharacteristicProp::Read, CharacteristicProp::Notify],
            [0u8; 8],
        )
        .build();
    let ms_boot: Characteristic<[u8; 4]> = hid
        .add_characteristic_small(
            0x2a32_u16,
            &[CharacteristicProp::Read, CharacteristicProp::Notify],
            [0u8; 4],
        )
        .build();

    // Report 协议下的三个报告（UUID 都是 0x2A4D，靠 Report Reference 区分）
    // 注意顺序：先 build() 出特征，再补 0x2908 描述符，
    // 这样句柄顺序才是「声明 → 值 → 描述符 → CCCD」
    // 描述符要在 build() 之前补，而且 add_descriptor_ro 返回的是 Descriptor 不是 builder，
    // 所以先把 builder 存进变量，加完描述符再 build()
    let mut kb_b = hid.add_characteristic_small(
        0x2a4d_u16,
        &[CharacteristicProp::Read, CharacteristicProp::Notify],
        [0u8; 8],
    );
    let _ = kb_b.add_descriptor_ro(0x2908_u16, PermissionLevel::Allowed, &RR_KB);
    let kb: Characteristic<[u8; 8]> = kb_b.build();

    let mut ms_b = hid.add_characteristic_small(
        0x2a4d_u16,
        &[CharacteristicProp::Read, CharacteristicProp::Notify],
        [0u8; 4],
    );
    let _ = ms_b.add_descriptor_ro(0x2908_u16, PermissionLevel::Allowed, &RR_MS);
    let ms: Characteristic<[u8; 4]> = ms_b.build();

    let mut cc_b = hid.add_characteristic_small(
        0x2a4d_u16,
        &[CharacteristicProp::Read, CharacteristicProp::Notify],
        [0u8; 2],
    );
    let _ = cc_b.add_descriptor_ro(0x2908_u16, PermissionLevel::Allowed, &RR_CC);
    let cc: Characteristic<[u8; 2]> = cc_b.build();

    hid.build();

    let server = SERVER.init(AttributeServer::new(table));
    (
        server,
        HidHandles {
            kb_boot,
            ms_boot,
            kb,
            ms,
            cc,
            cp: cp.handle,
        },
    )
}

#[embassy_executor::task]
pub async fn ble_task(bt: BT<'static>) {
    bl!(
        "[ble] 启动：内部堆空 {} B，主栈空 {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into()),
        stack_headroom()
    );

    // ---- 1. 控制器（esp-radio 的 btdm）----
    let connector = match BleConnector::new(bt, Default::default()) {
        Ok(c) => c,
        Err(e) => {
            bl!("[ble] 控制器初始化失败：{:?}", e);
            return;
        }
    };
    bl!(
        "[ble] HCI 就绪，内部堆空 {} B",
        esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into())
    );

    // ---- 2. 协议栈 ----
    let controller: BleController = ExternalController::new(connector);
    let resources = RESOURCES.init(HostResources::new());
    // 配对要 CSPRNG 播种（trouble 在 build() 里检查，不播种直接 panic）
    let mut rng = HalRng(Rng::new());
    let stack = trouble_host::new(controller, resources)
        .set_random_address(Address::random(BLE_ADDR))
        .set_random_generator_seed(&mut rng);
    // Just Works：没有屏幕也没有配对键盘，让主机直接配对，
    // 链路仍然加密（只是不做 MITM 认证），这是键盘类设备的常规做法。
    stack.set_io_capabilities(IoCapabilities::NoInputNoOutput);
    let Host {
        mut peripheral,
        mut runner,
        ..
    } = stack.build();

    // ---- 2.5 恢复上次的配对信息 ----
    //
    // 手机（安卓/iOS）配对后会一直记着这把 LTK，重连时直接要求加密。
    // 我们要是每次上电都忘掉，手机上还留着旧记录 → 永远 Authentication Failure，
    // 用户必须去「忘记设备」重新配对。所以 bond 存 flash，开机再灌回去。
    restore_bond(&stack).await;

    // ---- 3. GATT 表（HID 服务）----
    let (server, handles) = build_gatt_table();
    bl!("[ble] GATT 就绪（HID 0x1812），地址 {}", Address::random(BLE_ADDR));

    // ---- 4. runner + 广播/连接循环 ----
    match select(
        runner.run(),
        advertise_loop(&mut peripheral, server, &stack, handles),
    )
    .await
    {
        Either::First(e) => bl!("[ble] runner 退出：{:?}", e),
        Either::Second(()) => bl!("[ble] 广播循环退出（不该发生）"),
    }
}

/// 广播 → 等连接 → 跑 GATT + 转发键鼠 → 断开后重新广播
async fn advertise_loop(
    peripheral: &mut Peripheral<'_, BleController, DefaultPacketPool>,
    server: &'static GattServer,
    stack: &Stack<'_, BleController, DefaultPacketPool>,
    h: HidHandles,
) {
    loop {
        bl!("[ble] 广播中「{}」", "ESP32-POWER-BLE");

        let acceptor = match with_timeout(
            Duration::from_secs(10),
            peripheral.advertise(
                &Default::default(),
                Advertisement::ConnectableScannableUndirected {
                    adv_data: ADV_DATA,
                    scan_data: &[],
                },
            ),
        )
        .await
        {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                bl!("[ble] 广播失败：{:?}，1s 后重试", e);
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
            Err(_) => {
                bl!("[ble] 广播超时（10s），重试");
                continue;
            }
        };

        let conn = match acceptor.accept().await {
            Ok(c) => c,
            Err(e) => {
                bl!("[ble] accept 失败：{:?}", e);
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        };

        // 必须在 SMP 开始之前设：安卓连上就会立刻发起配对
        if let Err(e) = conn.set_bondable(true) {
            bl!("[ble] set_bondable 失败：{:?}（配对会失败）", e);
        }

        let peer = conn.peer_address().into_inner();
        bl!(
            "[ble] 已连接：{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            peer[5],
            peer[4],
            peer[3],
            peer[2],
            peer[1],
            peer[0]
        );

        match conn.with_attribute_server(server) {
            Ok(gatt) => serve_connection(&gatt, stack, h).await,
            Err(e) => bl!("[ble] 挂上 GATT 失败：{:?}", e),
        }

        bl!("[ble] 断开，准备重新广播");
        Timer::after(Duration::from_millis(500)).await;
    }
}

/// 一条连接的生命周期：处理 GATT 事件 + 把网页来的键鼠转成 HID 通知
async fn serve_connection(
    gatt: &GattConnection<'_, '_, DefaultPacketPool>,
    stack: &Stack<'_, BleController, DefaultPacketPool>,
    h: HidHandles,
) {
    loop {
        match select(gatt.next(), BLE_Q.receive()).await {
            // ---- GATT 事件 ----
            Either::First(GattConnectionEvent::Disconnected { reason }) => {
                bl!("[ble] 断开：{:?}", reason);
                // 断开时把所有键松开，免得主机侧卡键
                let _ = h.kb.notify(gatt, &[0u8; 8]).await;
                let _ = h.ms.notify(gatt, &[0u8; 4]).await;
                let _ = h.cc.notify(gatt, &[0u8; 2]).await;
                return;
            }
            Either::First(GattConnectionEvent::RequestConnectionParams(req)) => {
                // 安卓连上后一般会要求 7.5~15ms 的连接间隔，不回应会被断链
                let _ = req.accept(None, stack).await;
            }
            Either::First(GattConnectionEvent::PairingComplete { security_level, bond }) => {
                match bond {
                    Some(b) => {
                        save_bond(&b).await;
                        bl!(
                            "[ble] 配对完成（{:?}），bond 已存 flash（IRK {}）",
                            security_level,
                            if b.identity.irk.is_some() { "有" } else { "无" }
                        );
                    }
                    None => bl!("[ble] 配对完成（{:?}），但没生成 bond", security_level),
                }
            }
            Either::First(GattConnectionEvent::PairingFailed(e)) => {
                bl!("[ble] 配对失败：{:?}", e);
            }
            Either::First(GattConnectionEvent::PassKeyConfirm(_)) => {
                // Just Works 一般不会走到这里；真弹了就替用户点"确认"
                let _ = gatt.pass_key_confirm();
            }
            Either::First(GattConnectionEvent::PassKeyDisplay(_)) => {
                let _ = gatt.pass_key_confirm();
            }
            Either::First(GattConnectionEvent::Gatt { event }) => match event {
                // 读请求：Drop 时自动从表里取值回复（Report Map 走 Read Blob 分片）
                GattEvent::Read(ev) => {
                    let n = READ_CNT.fetch_add(1, Ordering::Relaxed);
                    if n < 6 {
                        bl!("[ble] 读句柄 0x{:04X}", ev.handle());
                    }
                }
                GattEvent::Write(ev) => {
                    let handle = ev.handle();
                    if let Ok(reply) = ev.accept() {
                        reply.send().await;
                    }
                    if handle == h.cp {
                        bl!("[ble] Control Point 被写（挂起/恢复）");
                    }
                }
                GattEvent::Other(_) => {}
                // 权限被拒：Drop 时自动回 ATT 错误
                GattEvent::NotAllowed(_) => {}
            },

            // ---- 网页 / Blinker 来的键鼠 ----
            Either::Second(ev) => {
                let r = match ev {
                    HidEvent::Kbd(rep) => {
                        // boot 报告（无 ID）和 report 协议（ID 1）两条都发
                        let _ = h.kb_boot.notify(gatt, &rep).await;
                        h.kb.notify(gatt, &rep).await
                    }
                    HidEvent::Mouse(rep) => {
                        let _ = h.ms_boot.notify(gatt, &rep).await;
                        h.ms.notify(gatt, &rep).await
                    }
                    HidEvent::Cc(rep) => h.cc.notify(gatt, &rep).await,
                };
                if let Err(e) = &r {
                    bl!("[ble] 通知失败：{:?}", e);
                }
            }

            Either::First(_) => {}
        }
    }
}

// ===================== bond 持久化 =====================

fn level_to_u8(l: SecurityLevel) -> u8 {
    match l {
        SecurityLevel::NoEncryption => 0,
        SecurityLevel::Encrypted => 1,
        SecurityLevel::EncryptedAuthenticated => 2,
    }
}

fn u8_to_level(v: u8) -> SecurityLevel {
    match v {
        1 => SecurityLevel::Encrypted,
        2 => SecurityLevel::EncryptedAuthenticated,
        _ => SecurityLevel::NoEncryption,
    }
}

/// 开机把 flash 里的配对信息灌回协议栈
async fn restore_bond(stack: &Stack<'_, BleController, DefaultPacketPool>) {
    let rec = {
        let mut g = crate::FLASH.lock().await;
        match g.as_mut() {
            Some(f) => crate::store::load_bond(f),
            None => None,
        }
    };
    let Some(rec) = rec else {
        bl!("[ble] 没有历史配对");
        return;
    };
    let identity = Identity {
        bd_addr: bt_hci::param::BdAddr::new(rec.addr),
        irk: rec.irk.map(|b| IdentityResolvingKey::new(u128::from_le_bytes(b))),
    };
    let info = BondInformation::new(
        identity,
        LongTermKey::from_le_bytes(rec.ltk),
        u8_to_level(rec.level),
        true,
    );
    match stack.add_bond_information(info) {
        Ok(()) => bl!(
            "[ble] 已恢复历史配对（IRK {}）",
            if rec.irk.is_some() { "有" } else { "无" }
        ),
        Err(e) => bl!("[ble] 恢复配对失败：{:?}", e),
    }
}

/// 配对成功 → 写 flash
async fn save_bond(b: &BondInformation) {
    let addr = b.identity.bd_addr.into_inner();
    let rec = crate::store::BondRec {
        ltk: b.ltk.to_le_bytes(),
        addr,
        // 写死 random：手机走 LE 随机地址，public 的情况极少
        kind: 1,
        irk: b.identity.irk.map(|k| k.0.to_le_bytes()),
        level: level_to_u8(b.security_level),
    };
    let mut g = crate::FLASH.lock().await;
    let ok = match g.as_mut() {
        Some(f) => crate::store::save_bond(f, &rec),
        None => false,
    };
    if ok {
        bl!("[ble] bond 已写入 flash");
    } else {
        bl!("[ble] bond 写 flash 失败（下次上电会忘）");
    }
}
