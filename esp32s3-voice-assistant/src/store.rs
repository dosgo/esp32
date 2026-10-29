//! 设备配置持久化（WiFi / 网关 / AI / 语音）
//!
//! 以前这些全是 `config.rs` 里的常量，换 WiFi 或换 API key 就得改代码重烧。
//! 现在改成「网页配置 + 存 flash」：
//!
//! - 热点永远开着（`ESP32-VOICE` / 192.168.4.1），手机连上就能改
//! - 改完存 flash，掉电不丢，重启生效
//! - 真正要连的网关地址、各家 API key 都在这里
//!
//! # 为什么写 0x9000
//!
//! espflash 默认分区表里 0x9000~0xE000 是 nvs 区，裸机固件不启用 NVS
//! （wifi 初始化时 nvs_enable=0），这块空着。应用固件从 0x10000 起，
//! 不会撞上。`cargo run` 烧录只擦 app 分区和 otadata，**配置不会丢**。
//!
//! # 存储格式
//!
//! ```text
//! [0..4]   MAGIC  "VSCF"
//! [4..6]   VERSION (LE u16)
//! [6..8]   payload_len (LE u16)
//! [8..]    TLV 序列：tag(u8) + len(u8) + data
//! [末尾]   checksum(u8 累加)
//! ```
//!
//! 用 TLV 而不是固定偏移：以后加字段只需多一个 tag，老配置照样能读出来。
//! 读失败（magic 不对 / 校验错 / 没配过）统一返回 `None`，调用方回落默认值。

use core::str;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use heapless::String;

/// 配置区起始地址（4KB 扇区对齐）
pub const FLASH_ADDR: u32 = 0x9000;
/// 用几个扇区（实际只写第一个）
const SECTORS: u32 = 1;

const MAGIC: u32 = 0x4643_5356; // "VSCF"
const VERSION: u16 = 1;

/// 整个缓冲区大小（要 ≥ 实际配置体积）
const BUF: usize = 1024;

// ---------------- 字段长度上限 ----------------
pub const MAX_SSID: usize = 32;
pub const MAX_PASS: usize = 64;
pub const MAX_KEY: usize = 128;
pub const MAX_MODEL: usize = 48;
pub const MAX_URL: usize = 64;
pub const MAX_CHAIN: usize = 48;

// ---------------- 热点默认值 ----------------
/// 出厂热点名
pub const DEFAULT_AP_SSID: &str = "ESP32-VOICE";
/// 出厂热点密码（WPA2 要求 8~63 字符）
pub const DEFAULT_AP_PASS: &str = "voice1234";
/// 热点模式下设备自己的地址，也是配置页面的访问地址
pub const AP_IP: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 4, 1);

// ---------------- 各家默认值 ----------------
/// DeepSeek 官方接口
pub const DS_BASE_DEFAULT: &str = "https://api.deepseek.com";
pub const DS_MODEL_DEFAULT: &str = "deepseek-chat";
/// 硅基流动
pub const SF_BASE_DEFAULT: &str = "https://api.siliconflow.cn/v1";
pub const SF_ASR_DEFAULT: &str = "FunAudioLLM/SenseVoiceSmall";
pub const SF_TTS_DEFAULT: &str = "FunAudioLLM/CosyVoice2-0.5B:anna";
/// 默认 ASR / TTS 调用链。第一个失败自动降级到下一个。
pub const ASR_CHAIN_DEFAULT: &str = "local,local-lite,siliconflow";
pub const TTS_CHAIN_DEFAULT: &str = "siliconflow,piper";

// ---------------- TLV tag ----------------
const T_WIFI_SSID: u8 = 1;
const T_WIFI_PASS: u8 = 2;
const T_GW_HOST: u8 = 3;
const T_GW_PORT: u8 = 4;
const T_LLM_BACKEND: u8 = 5;
const T_LLM_KEY: u8 = 6;
const T_LLM_MODEL: u8 = 7;
const T_LLM_BASE: u8 = 8;
const T_ASR_CHAIN: u8 = 9;
const T_ASR_KEY: u8 = 10;
const T_ASR_MODEL: u8 = 11;
const T_TTS_CHAIN: u8 = 12;
const T_TTS_VOICE: u8 = 13;
const T_AP_SSID: u8 = 14;
const T_AP_PASS: u8 = 15;

/// 全部配置。字段都是 `heapless::String`，定长不占堆。
#[derive(Clone)]
pub struct Config {
    // ---- STA：连家里的 WiFi ----
    pub wifi_ssid: String<MAX_SSID>,
    pub wifi_pass: String<MAX_PASS>,

    // ---- 语音网关（跑在 PC / 安卓上）----
    pub gw_host: String<16>,
    pub gw_port: u16,

    // ---- 大模型 ----
    /// `deepseek` 或 `siliconflow`
    pub llm_backend: String<16>,
    pub llm_key: String<MAX_KEY>,
    pub llm_model: String<MAX_MODEL>,
    pub llm_base: String<MAX_URL>,

    // ---- 语音识别 ----
    /// 降级链，逗号分隔，如 `local,local-lite,siliconflow`
    pub asr_chain: String<MAX_CHAIN>,
    /// 硅基流动 key（ASR / TTS 共用，ASR 走云端时才需要）
    pub asr_key: String<MAX_KEY>,
    pub asr_model: String<MAX_MODEL>,

    // ---- 语音合成 ----
    pub tts_chain: String<MAX_CHAIN>,
    pub tts_voice: String<MAX_MODEL>,

    // ---- 热点自己 ----
    pub ap_ssid: String<MAX_SSID>,
    pub ap_pass: String<MAX_PASS>,
}

impl Config {
    /// 出厂默认值。没配过网时用它，里面有占位的网关地址。
    pub const fn new() -> Self {
        Self {
            wifi_ssid: String::new(),
            wifi_pass: String::new(),
            gw_host: String::new(),
            gw_port: 8765,
            llm_backend: String::new(),
            llm_key: String::new(),
            llm_model: String::new(),
            llm_base: String::new(),
            asr_chain: String::new(),
            asr_key: String::new(),
            asr_model: String::new(),
            tts_chain: String::new(),
            tts_voice: String::new(),
            ap_ssid: String::new(),
            ap_pass: String::new(),
        }
    }

    /// 是不是已经配过 WiFi 了。没配过就只开热点等人来配。
    pub fn wifi_set(&self) -> bool {
        !self.wifi_ssid.is_empty()
    }

    /// 实际用的热点名
    pub fn ap_ssid_or_default(&self) -> &str {
        if self.ap_ssid.is_empty() {
            DEFAULT_AP_SSID
        } else {
            self.ap_ssid.as_str()
        }
    }

    /// 实际用的热点密码
    pub fn ap_pass_or_default(&self) -> &str {
        if self.ap_pass.is_empty() {
            DEFAULT_AP_PASS
        } else {
            self.ap_pass.as_str()
        }
    }

    pub fn gw_host_or_default(&self) -> &str {
        if self.gw_host.is_empty() {
            "192.168.1.100"
        } else {
            self.gw_host.as_str()
        }
    }

    pub fn llm_backend_or_default(&self) -> &str {
        if self.llm_backend.is_empty() {
            "deepseek"
        } else {
            self.llm_backend.as_str()
        }
    }

    pub fn llm_model_or_default(&self) -> &str {
        if self.llm_model.is_empty() {
            DS_MODEL_DEFAULT
        } else {
            self.llm_model.as_str()
        }
    }

    pub fn llm_base_or_default(&self) -> &str {
        if self.llm_base.is_empty() {
            if self.llm_backend_or_default() == "siliconflow" {
                SF_BASE_DEFAULT
            } else {
                DS_BASE_DEFAULT
            }
        } else {
            self.llm_base.as_str()
        }
    }

    pub fn asr_chain_or_default(&self) -> &str {
        if self.asr_chain.is_empty() {
            ASR_CHAIN_DEFAULT
        } else {
            self.asr_chain.as_str()
        }
    }

    pub fn asr_model_or_default(&self) -> &str {
        if self.asr_model.is_empty() {
            SF_ASR_DEFAULT
        } else {
            self.asr_model.as_str()
        }
    }

    pub fn tts_chain_or_default(&self) -> &str {
        if self.tts_chain.is_empty() {
            TTS_CHAIN_DEFAULT
        } else {
            self.tts_chain.as_str()
        }
    }

    pub fn tts_voice_or_default(&self) -> &str {
        if self.tts_voice.is_empty() {
            SF_TTS_DEFAULT
        } else {
            self.tts_voice.as_str()
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------- 编码 / 解码 ----------------

/// 把 str 塞进 payload，返回新长度。超长就截断（不让网页输入把编码搞坏）
fn put(buf: &mut [u8], at: usize, tag: u8, val: &str) -> Option<usize> {
    let bytes = val.as_bytes();
    // 单条最长 255，再长就没意义了
    let n = bytes.len().min(255);
    if at + 2 + n > buf.len() {
        return None;
    }
    buf[at] = tag;
    buf[at + 1] = n as u8;
    buf[at + 2..at + 2 + n].copy_from_slice(&bytes[..n]);
    Some(at + 2 + n)
}

/// 把一个 TLV 的值读成 heapless::String
fn get<const N: usize>(payload: &[u8], tag: u8) -> String<N> {
    let mut i = 0;
    while i + 2 <= payload.len() {
        let t = payload[i];
        let len = payload[i + 1] as usize;
        let start = i + 2;
        if start + len > payload.len() {
            break;
        }
        if t == tag {
            let mut s = String::new();
            // 非 UTF-8 或超长都当空，宁可回落默认值
            if let Ok(v) = str::from_utf8(&payload[start..start + len]) {
                let _ = s.push_str(v);
            }
            return s;
        }
        i = start + len;
    }
    String::new()
}

/// 序列化成字节。返回实际长度。
fn encode(c: &Config, buf: &mut [u8; BUF]) -> Option<usize> {
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&VERSION.to_le_bytes());
    // [6..8] 的 payload_len 最后再回填

    let mut at = 8usize;
    macro_rules! w {
        ($tag:expr, $val:expr) => {
            at = put(buf, at, $tag, $val)?;
        };
    }
    w!(T_WIFI_SSID, &c.wifi_ssid);
    w!(T_WIFI_PASS, &c.wifi_pass);
    w!(T_GW_HOST, &c.gw_host);
    // 端口是唯一定长字段：tag, len=2, lo, hi。不走字符串，省一次解析
    if at + 4 > buf.len() {
        return None;
    }
    buf[at] = T_GW_PORT;
    buf[at + 1] = 2;
    buf[at + 2] = (c.gw_port & 0xff) as u8;
    buf[at + 3] = (c.gw_port >> 8) as u8;
    at += 4;
    w!(T_LLM_BACKEND, &c.llm_backend);
    w!(T_LLM_KEY, &c.llm_key);
    w!(T_LLM_MODEL, &c.llm_model);
    w!(T_LLM_BASE, &c.llm_base);
    w!(T_ASR_CHAIN, &c.asr_chain);
    w!(T_ASR_KEY, &c.asr_key);
    w!(T_ASR_MODEL, &c.asr_model);
    w!(T_TTS_CHAIN, &c.tts_chain);
    w!(T_TTS_VOICE, &c.tts_voice);
    w!(T_AP_SSID, &c.ap_ssid);
    w!(T_AP_PASS, &c.ap_pass);

    let payload_len = at - 8;
    buf[6..8].copy_from_slice(&(payload_len as u16).to_le_bytes());

    // checksum：从 0 到 at 累加
    let mut sum: u8 = 0;
    for b in &buf[0..at] {
        sum = sum.wrapping_add(*b);
    }
    buf[at] = sum;
    Some(at + 1)
}

/// 反序列化。任何异常都返回 None（当作没配过）。
fn decode(buf: &[u8]) -> Option<Config> {
    if buf.len() < 9 {
        return None;
    }
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != MAGIC {
        return None;
    }
    let ver = u16::from_le_bytes([buf[4], buf[5]]);
    if ver != VERSION {
        return None;
    }
    let payload_len = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    if 8 + payload_len + 1 > buf.len() {
        return None;
    }
    let mut sum: u8 = 0;
    for b in &buf[0..8 + payload_len] {
        sum = sum.wrapping_add(*b);
    }
    if sum != buf[8 + payload_len] {
        return None;
    }

    let p = &buf[8..8 + payload_len];

    // 端口是唯一定长的字段，单独取
    let mut port = 8765u16;
    let mut i = 0;
    while i + 2 <= p.len() {
        let t = p[i];
        let len = p[i + 1] as usize;
        if i + 2 + len > p.len() {
            break;
        }
        if t == T_GW_PORT && len == 2 {
            port = u16::from_le_bytes([p[i + 2], p[i + 3]]);
        }
        i += 2 + len;
    }

    Some(Config {
        wifi_ssid: get(p, T_WIFI_SSID),
        wifi_pass: get(p, T_WIFI_PASS),
        gw_host: get(p, T_GW_HOST),
        gw_port: port,
        llm_backend: get(p, T_LLM_BACKEND),
        llm_key: get(p, T_LLM_KEY),
        llm_model: get(p, T_LLM_MODEL),
        llm_base: get(p, T_LLM_BASE),
        asr_chain: get(p, T_ASR_CHAIN),
        asr_key: get(p, T_ASR_KEY),
        asr_model: get(p, T_ASR_MODEL),
        tts_chain: get(p, T_TTS_CHAIN),
        tts_voice: get(p, T_TTS_VOICE),
        ap_ssid: get(p, T_AP_SSID),
        ap_pass: get(p, T_AP_PASS),
    })
}

// ---------------- flash 读写 ----------------

/// 从 flash 读配置。没配过 / 校验失败都返回 None。
pub fn load(f: &mut FlashStorage<'_>) -> Option<Config> {
    let mut raw = Buf([0u8; BUF]);
    if ReadNorFlash::read(f, FLASH_ADDR, &mut raw.0).is_err() {
        return None;
    }
    decode(&raw.0)
}

/// 写配置。返回是否成功。
///
/// 先擦后写，中间断电会丢配置（下次开机回落默认值，重新配网即可）。
/// 要更保险可以搞 A/B 双扇区，但 4KB 存这点东西、而且配置很少改，不值得。
pub fn save(f: &mut FlashStorage<'_>, c: &Config) -> bool {
    let mut raw = Buf([0u8; BUF]);
    let len = match encode(c, &mut raw.0) {
        Some(n) => n,
        None => return false,
    };
    let _ = len;

    let end = FLASH_ADDR + SECTORS * FlashStorage::SECTOR_SIZE;
    if NorFlash::erase(f, FLASH_ADDR, end).is_err() {
        return false;
    }
    NorFlash::write(f, FLASH_ADDR, &raw.0[..len]).is_ok()
}

/// 擦掉配置（恢复出厂）
pub fn erase(f: &mut FlashStorage<'_>) -> bool {
    let end = FLASH_ADDR + SECTORS * FlashStorage::SECTOR_SIZE;
    NorFlash::erase(f, FLASH_ADDR, end).is_ok()
}

/// flash 读写要求 4 字节对齐，包一层
struct Buf([u8; BUF]);
