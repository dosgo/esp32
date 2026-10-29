//! 三个板载按键
//!
//! | 键 | GPIO | 行为 |
//! |----|------|------|
//! | PWR | 5 | 短按：**打断**（清空播放队列 + 通知网关停止）。长按 5 秒：重启设备 |
//! | PLUS | 4 | 音量 +10 |
//! | MINUS | 0 | 音量 -10 |
//!
//! # 为什么要打断
//!
//! 以前它一开始说话，你只能等它说完 —— 想说「不对，我问的是……」没有任何办法。
//! 打断要同时做三件事，少一件都会出问题：
//!
//! 1. 清空 `SPK_CH`：本地播放队列里还排着几秒音频，不清空的话声音会继续放
//! 2. `set_state(Idle)`：让麦克风立刻回到待机门控
//! 3. 置 `ABORT_REQ`：由 `run_session` 在下一轮循环里发 `{"type":"abort"}` 给网关，
//!    否则网关还在往下推 TTS 数据，你这边刚清空它又灌满了
//!
//! # 关于 IO0
//!
//! IO0 是 strapping 脚：上电瞬间被拉低会进下载模式。所以 MINUS 键**上电时别按**，
//! 运行中随便按。这不是设计缺陷，几乎所有开发板的 BOOT 键都挂 IO0。

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_time::{Duration, Timer};
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::gpio::AnyPin;

use crate::audio::{self, State, SPK_CH};

/// 当前音量（0~100）。`sensors_task` 每轮检查它有没有变，变了就写进 ES8311。
pub static VOLUME: AtomicU8 = AtomicU8::new(85);

/// 有按键请求打断。`run_session` 读到就清掉并发 abort 给网关。
pub static ABORT_REQ: AtomicBool = AtomicBool::new(false);

/// 取走一次打断请求（读后自动清）
pub fn take_abort() -> bool {
    ABORT_REQ.swap(false, Ordering::Relaxed)
}

/// 按键轮询周期。20ms 足够，人的手速到不了这个精度。
const TICK_MS: u64 = 20;
/// 连续读几次低电平才算真的按下（防抖）
const DEBOUNCE: u8 = 2;
/// 长按多久算"重启"
const LONG_PRESS_MS: u32 = 5000;

#[embassy_executor::task]
pub async fn keys_task() {
    let pwr = Input::new(
        unsafe { AnyPin::steal(crate::board::PIN_KEY_PWR) },
        InputConfig::default().with_pull(Pull::Up),
    );
    let plus = Input::new(
        unsafe { AnyPin::steal(crate::board::PIN_KEY_PLUS) },
        InputConfig::default().with_pull(Pull::Up),
    );
    let minus = Input::new(
        unsafe { AnyPin::steal(crate::board::PIN_KEY_MINUS) },
        InputConfig::default().with_pull(Pull::Up),
    );

    // 各键的防抖计数和按住时长
    let mut pwr_low = 0u8;
    let mut pwr_held_ms = 0u32;
    let mut plus_low = 0u8;
    let mut minus_low = 0u8;
    // 只在"按下的那一刻"触发一次，按住不放不重复
    let mut plus_latch = false;
    let mut minus_latch = false;

    loop {
        // ---- PWR：短按打断 / 长按重启 ----
        if pwr.is_low() {
            pwr_low = pwr_low.saturating_add(1);
            if pwr_low >= DEBOUNCE {
                pwr_held_ms = pwr_held_ms.saturating_add(TICK_MS as u32);
                if pwr_held_ms >= LONG_PRESS_MS {
                    // software_reset 不会返回，所以不需要额外标志防重复触发
                    log::warn!("[key] PWR 长按，重启设备");
                    esp_hal::system::software_reset();
                }
            }
        } else {
            // 松手：如果按住时间在防抖和长按之间，算一次短按
            if pwr_low >= DEBOUNCE && pwr_held_ms < 800 {
                do_abort();
            }
            pwr_low = 0;
            pwr_held_ms = 0;
        }

        // ---- PLUS ----
        if plus.is_low() {
            plus_low = plus_low.saturating_add(1);
            if plus_low >= DEBOUNCE && !plus_latch {
                plus_latch = true;
                bump_volume(10);
            }
        } else {
            plus_low = 0;
            plus_latch = false;
        }

        // ---- MINUS ----
        if minus.is_low() {
            minus_low = minus_low.saturating_add(1);
            if minus_low >= DEBOUNCE && !minus_latch {
                minus_latch = true;
                bump_volume(-10);
            }
        } else {
            minus_low = 0;
            minus_latch = false;
        }

        Timer::after(Duration::from_millis(TICK_MS)).await;
    }
}

/// 打断：清队列 + 回待机 + 通知网关
fn do_abort() {
    let before = SPK_CH.len();
    // 把排队的音频帧全丢掉
    while SPK_CH.try_receive().is_ok() {}
    audio::set_state(State::Idle);
    ABORT_REQ.store(true, Ordering::Relaxed);
    log::info!("[key] PWR 打断（丢弃 {before} 帧待播音频）");
}

/// 调音量，钳在 0~100
fn bump_volume(delta: i16) {
    let cur = VOLUME.load(Ordering::Relaxed) as i16;
    let next = (cur + delta).clamp(0, 100) as u8;
    if next != cur as u8 {
        VOLUME.store(next, Ordering::Relaxed);
        log::info!("[key] 音量 {next}");
    }
}
