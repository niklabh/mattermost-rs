//! Port of `html/template/css.go`: CSS decoding and the two CSS escapers.

use super::content::stringify;
use super::escape::FILTER_FAILSAFE;
use crate::value::{ContentType, Value};

/// `endsWithCSSKeyword` (css.go:22).
pub(crate) fn ends_with_css_keyword(b: &[u8], kw: &str) -> bool {
    let Some(i) = b.len().checked_sub(kw.len()) else {
        return false;
    };
    if i != 0 {
        let r = last_rune(&b[..i]);
        if is_css_nmchar(r) {
            return false;
        }
    }
    b[i..].to_ascii_lowercase() == kw.as_bytes()
}

/// `utf8.DecodeLastRune`, returning U+FFFD for an invalid tail.
fn last_rune(b: &[u8]) -> u32 {
    for start in (b.len().saturating_sub(4)..b.len()).rev() {
        if let Ok(s) = std::str::from_utf8(&b[start..])
            && let Some(c) = s.chars().next()
            && c.len_utf8() == b.len() - start
        {
            return c as u32;
        }
    }
    0xfffd
}

/// `isCSSNmchar` (css.go:37).
pub(crate) fn is_css_nmchar(r: u32) -> bool {
    (u32::from(b'a')..=u32::from(b'z')).contains(&r)
        || (u32::from(b'A')..=u32::from(b'Z')).contains(&r)
        || (u32::from(b'0')..=u32::from(b'9')).contains(&r)
        || r == u32::from(b'-')
        || r == u32::from(b'_')
        || (0x80..=0xd7ff).contains(&r)
        || (0xe000..=0xfffd).contains(&r)
        || (0x10000..=0x10ffff).contains(&r)
}

/// `isHex` (css.go:106).
pub(crate) fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn hex_decode(s: &[u8]) -> u32 {
    let mut n: u32 = 0;
    for &c in s {
        n <<= 4;
        n |= match c {
            b'0'..=b'9' => u32::from(c - b'0'),
            b'a'..=b'f' => u32::from(c - b'a') + 10,
            b'A'..=b'F' => u32::from(c - b'A') + 10,
            _ => 0,
        };
    }
    n
}

fn skip_css_space(c: &[u8]) -> &[u8] {
    match c.first() {
        None => c,
        Some(b'\t' | b'\n' | b'\x0c' | b' ') => &c[1..],
        Some(b'\r') => {
            if c.len() >= 2 && c[1] == b'\n' {
                &c[2..]
            } else {
                &c[1..]
            }
        }
        Some(_) => c,
    }
}

/// `isCSSSpace` (css.go:149).
fn is_css_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// `decodeCSS` (css.go:51): decodes CSS3 escapes in a string, token or URL.
pub(crate) fn decode_css(s: &[u8]) -> Vec<u8> {
    if !s.contains(&b'\\') {
        return s.to_vec();
    }
    let mut b: Vec<u8> = Vec::with_capacity(s.len());
    let mut s = s;
    while !s.is_empty() {
        let i = s.iter().position(|&c| c == b'\\').unwrap_or(s.len());
        b.extend_from_slice(&s[..i]);
        s = &s[i..];
        if s.len() < 2 {
            break;
        }
        if is_hex(s[1]) {
            let mut j = 2;
            while j < s.len() && j < 7 && is_hex(s[j]) {
                j += 1;
            }
            let mut r = hex_decode(&s[1..j]);
            if r > 0x10ffff {
                r /= 16;
                j -= 1;
            }
            let ch = char::from_u32(r).unwrap_or('\u{fffd}');
            let mut tmp = [0u8; 4];
            b.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            s = skip_css_space(&s[j..]);
        } else {
            let n = utf8_len_at(&s[1..]);
            b.extend_from_slice(&s[1..1 + n]);
            s = &s[1 + n..];
        }
    }
    b
}

/// The width of the UTF-8 sequence at the start of `b` (1 for an invalid one), as
/// `utf8.DecodeRune` reports it.
pub(crate) fn utf8_len_at(b: &[u8]) -> usize {
    for n in 1..=4.min(b.len()) {
        if let Ok(s) = std::str::from_utf8(&b[..n])
            && s.chars().count() == 1
        {
            return n;
        }
    }
    1.min(b.len())
}

fn css_replacement(r: char) -> Option<&'static str> {
    Some(match r {
        '\0' => r"\0",
        '\t' => r"\9",
        '\n' => r"\a",
        '\x0c' => r"\c",
        '\r' => r"\d",
        '"' => r"\22",
        '&' => r"\26",
        '\'' => r"\27",
        '(' => r"\28",
        ')' => r"\29",
        '+' => r"\2b",
        '/' => r"\2f",
        ':' => r"\3a",
        ';' => r"\3b",
        '<' => r"\3c",
        '>' => r"\3e",
        '\\' => r"\\",
        '{' => r"\7b",
        '}' => r"\7d",
        _ => return None,
    })
}

/// `cssEscaper` (css.go:159): escapes HTML and CSS special characters using `\<hex>+` escapes.
pub(crate) fn css_escaper(args: &[Option<&Value>]) -> String {
    let (s, _) = stringify(args);
    let bytes = s.as_bytes();
    let mut b = String::new();
    let mut written = 0;
    for (i, r) in s.char_indices() {
        let Some(repl) = css_replacement(r) else {
            continue;
        };
        b.push_str(&s[written..i]);
        b.push_str(repl);
        written = i + r.len_utf8();
        if repl != r"\\"
            && (written == bytes.len() || is_hex(bytes[written]) || is_css_space(bytes[written]))
        {
            b.push(' ');
        }
    }
    if written == 0 {
        return s;
    }
    b.push_str(&s[written..]);
    b
}

/// `cssValueFilter` (css.go:224): allows innocuous CSS values in the output including CSS
/// quantities (10px or 25%), ID or class literals (#foo, .bar), keyword values (inherit, blue),
/// and colors (#888). It filters out unsafe values, such as those that affect token boundaries,
/// and anything that might execute scripts.
pub(crate) fn css_value_filter(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if t == ContentType::Css {
        return s;
    }
    let b = decode_css(s.as_bytes());
    let mut id: Vec<u8> = Vec::with_capacity(64);
    for (i, &c) in b.iter().enumerate() {
        match c {
            0 | b'"' | b'\'' | b'(' | b')' | b'/' | b';' | b'@' | b'[' | b'\\' | b']' | b'`'
            | b'{' | b'}' | b'<' | b'>' => return FILTER_FAILSAFE.to_string(),
            b'-' => {
                if i != 0 && b[i - 1] == b'-' {
                    return FILTER_FAILSAFE.to_string();
                }
            }
            _ => {
                if c < 0x80 && is_css_nmchar(u32::from(c)) {
                    id.push(c);
                }
            }
        }
    }
    let id = id.to_ascii_lowercase();
    let contains = |needle: &[u8]| id.windows(needle.len()).any(|w| w == needle);
    if contains(b"expression") || contains(b"mozbinding") {
        return FILTER_FAILSAFE.to_string();
    }
    String::from_utf8_lossy(&b).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode() {
        assert_eq!(decode_css(br"\41\42 x"), b"ABx");
        assert_eq!(decode_css(br"a\\b"), br"a\b");
        assert_eq!(decode_css(br"\65 xpression"), b"expression");
    }

    #[test]
    fn filters() {
        let v = Value::str("expression(alert(1))");
        assert_eq!(css_value_filter(&[Some(&v)]), "ZgotmplZ");
        let v = Value::str("#fff");
        assert_eq!(css_value_filter(&[Some(&v)]), "#fff");
        let v = Value::str("a--b");
        assert_eq!(css_value_filter(&[Some(&v)]), "ZgotmplZ");
        let v = Value::str("a\"b c");
        assert_eq!(css_escaper(&[Some(&v)]), r"a\22 b c");
        assert!(ends_with_css_keyword(b"background: URL", "url"));
        assert!(!ends_with_css_keyword(b"xurl", "url"));
    }
}
