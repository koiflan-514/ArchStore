//! 缓存 key 编码：只允许 [a-z0-9._-]，其余字节用 %XX 转义。
//!
//! 严禁把用户输入或包名直接当作路径片段（防目录穿越）。
//! 额外规则：前导点号强制转义，保证编码结果既不等于 "." 也不等于 ".."。

/// 无需转义的字符集合：小写字母、数字、点、下划线、连字符。
fn is_plain(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
}

/// 把一个任意字符串编码为安全的文件名片段。
///
/// 编码是单射的：解码后必定还原原文（UTF-8 按字节转义）。
pub fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, b) in raw.as_bytes().iter().copied().enumerate() {
        let forced = i == 0 && b == b'.';
        if is_plain(b) && !forced {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    if out.is_empty() {
        // 空串使用保留编码 %00。调用方不应使用空 key（Cache 层始终传入非空 key），
        // 这里只是为了避免产出空文件名。
        out.push_str("%00");
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// 解码 encode() 的结果；遇到非法编码返回 None。
pub fn decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_val(bytes[i + 1])?;
                let lo = hex_val(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b if is_plain(b) => {
                out.push(b);
                i += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

/// 判断一个已编码的 key 是否安全（不可为空、无路径分隔符、非 . / ..）。
///
/// 逐字节检查无需单独做：decode() 只接受 [a-z0-9._-] 与合法的 %XX，
/// 因此任何字面量路径分隔符（'/'）都会让 decode 返回 None。
pub fn is_safe(encoded: &str) -> bool {
    !encoded.is_empty()
        && encoded != "."
        && encoded != ".."
        && !encoded.starts_with('.')
        && decode(encoded).is_some()
}

/// 组合出索引使用的完整 key："ns/key"（ns 同样被编码）。
pub fn namespaced(ns: &str, key: &str) -> String {
    format!("{}/{}", encode(ns), encode(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_strings_pass_through() {
        assert_eq!(encode("firefox"), "firefox");
        assert_eq!(encode("aur.search-1_x"), "aur.search-1_x");
    }

    #[test]
    fn uppercase_and_specials_are_escaped() {
        assert_eq!(encode("Firefox"), "%46irefox");
        assert_eq!(encode("a/b"), "a%2Fb");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn leading_dot_is_escaped_so_dotdot_is_impossible() {
        assert_eq!(encode(".."), "%2E.");
        assert_eq!(encode("."), "%2E");
        assert_eq!(encode("../etc/passwd"), "%2E.%2Fetc%2Fpasswd");
        // 编码结果本身必须安全（可以当作文件名），且绝不等于 "." / ".."
        for raw in ["..", ".", "../x", "./x"] {
            let enc = encode(raw);
            assert_ne!(enc, ".");
            assert_ne!(enc, "..");
            assert!(!enc.starts_with('.'), "{raw} -> {enc}");
            assert!(is_safe(&enc), "{raw} -> {enc}");
        }
        // 未编码的原始输入必须被拒绝
        for raw in ["..", ".", "../x", "a/b", "UPPER"] {
            assert!(!is_safe(raw), "{raw} 必须被拒绝");
        }
    }

    #[test]
    fn empty_key_uses_reserved_encoding() {
        // 空串是保留情形：编码结果非空且安全，但解码回 "%00" 而不是 ""
        assert_eq!(encode(""), "%00");
        assert!(is_safe("%00"));
    }

    #[test]
    fn encode_decode_is_injective_on_samples() {
        for s in [
            "firefox",
            "org.mozilla.firefox",
            "中文包名",
            "a/b\\c",
            "..",
            "AUR:info:yay",
            "emoji-🎉",
            "org.gnome.Calculator",
        ] {
            let e = encode(s);
            assert!(is_safe(&e), "{s} -> {e}");
            assert_eq!(decode(&e).as_deref(), Some(s), "roundtrip {s}");
        }
    }

    #[test]
    fn decode_rejects_malformed() {
        assert_eq!(decode("%2"), None);
        assert_eq!(decode("%ZZ"), None);
        assert_eq!(decode("%FF%FE"), None); // 非法 UTF-8
        assert_eq!(decode("UPPER"), None);
    }

    #[test]
    fn namespaced_joins_safely() {
        assert_eq!(namespaced("aur", "search/firefox"), "aur/search%2Ffirefox");
        let joined = namespaced("aur", "../../etc/passwd");
        let (ns, key) = joined.split_once('/').expect("ns/key");
        assert!(is_safe(ns));
        assert!(is_safe(key));
        assert_eq!(decode(key).as_deref(), Some("../../etc/passwd"));
    }
}
