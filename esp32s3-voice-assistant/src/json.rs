//! 极简 JSON 取值器
//!
//! 协议里只有几个固定字段（type / text / state / ...），
//! 引 serde-json-core 会让固件体积增加几十 KB，还会带来一堆泛型噪音。
//! 这里只做「取一个字符串字段」这一件事，够用且零依赖。
//!
//! 支持的处理：`\"` `\\` `\/` `\n` `\r` `\t` `\b` `\f`，以及 `\uXXXX`
//! （中文在协议里是直接 UTF-8，不会走 \u，但网关可能转义，保险起见做了 BMP 范围的转换）。

#![allow(dead_code)]

/// 取出 `"key": "value"` 里的 value（已反转义）。
///
/// `out` 不够大时返回 `None`。
pub fn get_str<'a>(json: &str, key: &str, out: &'a mut [u8]) -> Option<&'a str> {
    let bytes = json.as_bytes();
    let key_bytes = key.as_bytes();

    // 找 "key"
    let mut i = 0usize;
    let mut found = None;
    while i + key_bytes.len() + 2 <= bytes.len() {
        if bytes[i] == b'"' && &bytes[i + 1..i + 1 + key_bytes.len()] == key_bytes {
            // 后面必须紧跟引号
            if bytes.get(i + 1 + key_bytes.len()) == Some(&b'"') {
                found = Some(i + 2 + key_bytes.len());
                break;
            }
        }
        i += 1;
    }
    let mut p = found?;

    // 跳过空白和冒号
    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b':' || bytes[p] == b'\t') {
        p += 1;
    }
    if p >= bytes.len() || bytes[p] != b'"' {
        return None;
    }
    p += 1;

    let mut o = 0usize;
    while p < bytes.len() {
        let c = bytes[p];
        if c == b'"' {
            return core::str::from_utf8(&out[..o]).ok();
        } else if c == b'\\' {
            p += 1;
            let e = *bytes.get(p)?;
            let ch: char = match e {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'u' => {
                    // \uXXXX，只处理 BMP（中文都在这里）
                    if p + 4 >= bytes.len() {
                        return None;
                    }
                    let hex = core::str::from_utf8(&bytes[p + 1..p + 5]).ok()?;
                    let cp = u16::from_str_radix(hex, 16).ok()? as u32;
                    p += 4;
                    char::from_u32(cp)?
                }
                _ => '?',
            };
            let mut tmp = [0u8; 4];
            let s = ch.encode_utf8(&mut tmp);
            if o + s.len() > out.len() {
                return None;
            }
            out[o..o + s.len()].copy_from_slice(s.as_bytes());
            o += s.len();
        } else {
            if o >= out.len() {
                return None;
            }
            out[o] = c;
            o += 1;
        }
        p += 1;
    }
    None
}

/// 取布尔值，`"key": true`
pub fn get_bool(json: &str, key: &str) -> Option<bool> {
    let pos = json.find(key)?;
    let rest = &json[pos + key.len()..];
    if rest.contains("true") {
        Some(true)
    } else if rest.contains("false") {
        Some(false)
    } else {
        None
    }
}

/// 取整数字段，`"key": 123`
pub fn get_i64(json: &str, key: &str) -> Option<i64> {
    let pos = json.find(key)?;
    let rest = &json[pos + key.len()..];
    let start = rest.find(|c: char| c == '-' || c.is_ascii_digit())?;
    let tail = &rest[start..];
    let end = tail
        .find(|c: char| !(c == '-' || c.is_ascii_digit()))
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}
