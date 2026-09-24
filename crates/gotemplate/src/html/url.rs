//! Port of `html/template/url.go`: URL filtering, normalization and escaping, and `srcset`.

use super::content::stringify;
use super::escape::FILTER_FAILSAFE;
use crate::value::{ContentType, Value};

/// `strings.EqualFold(s, target)` for an ASCII-lowercase `target`: Unicode simple folding adds
/// only `ſ` (U+017F) → `s` and `K` (U+212A) → `k` to ASCII case-insensitivity.
fn equal_fold(s: &str, target: &str) -> bool {
    let mut a = s.chars();
    let mut b = target.chars();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                let x = match x {
                    '\u{17f}' => 's',
                    '\u{212a}' => 'k',
                    c => c.to_ascii_lowercase(),
                };
                if x != y {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// `isSafeURL` (url.go:48): a URL with a scheme is safe only if the scheme is http, https or
/// mailto.
pub(crate) fn is_safe_url(s: &str) -> bool {
    if let Some((protocol, _)) = s.split_once(':')
        && !protocol.contains('/')
        && !equal_fold(protocol, "http")
        && !equal_fold(protocol, "https")
        && !equal_fold(protocol, "mailto")
    {
        return false;
    }
    true
}

/// `urlFilter` (url.go:36).
pub(crate) fn url_filter(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if t == ContentType::Url {
        return s;
    }
    if !is_safe_url(&s) {
        return format!("#{FILTER_FAILSAFE}");
    }
    s
}

/// `urlEscaper` (url.go:59).
pub(crate) fn url_escaper(args: &[Option<&Value>]) -> String {
    url_processor(false, args)
}

/// `urlNormalizer` (url.go:65).
pub(crate) fn url_normalizer(args: &[Option<&Value>]) -> String {
    url_processor(true, args)
}

fn url_processor(norm: bool, args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    let norm = norm || t == ContentType::Url;
    let mut b = String::new();
    if process_url_onto(&s, norm, &mut b) {
        return b;
    }
    s
}

/// `processURLOnto` (url.go:87): percent-encodes what is not allowed in a URL (or, when `norm`,
/// only what is not allowed anywhere in one). Reports whether anything was written.
pub(crate) fn process_url_onto(s: &str, norm: bool, b: &mut String) -> bool {
    let bytes = s.as_bytes();
    let mut written = 0;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'!' | b'#' | b'$' | b'&' | b'*' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?'
            | b'@' | b'[' | b']' => {
                if norm {
                    continue;
                }
            }
            b'-' | b'.' | b'_' | b'~' => continue,
            b'%' => {
                if norm && i + 2 < bytes.len() && is_hex(bytes[i + 1]) && is_hex(bytes[i + 2]) {
                    continue;
                }
            }
            _ => {
                if c.is_ascii_alphanumeric() {
                    continue;
                }
            }
        }
        b.push_str(&String::from_utf8_lossy(&bytes[written..i]));
        b.push_str(&format!("%{c:02x}"));
        written = i + 1;
    }
    b.push_str(&String::from_utf8_lossy(&bytes[written..]));
    written != 0
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// `srcsetFilterAndEscaper` (url.go:139).
pub(crate) fn srcset_filter_and_escaper(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    match t {
        ContentType::Srcset => return s,
        ContentType::Url => {
            let mut b = String::new();
            let s = if process_url_onto(&s, true, &mut b) {
                b
            } else {
                s
            };
            return s.replace(',', "%2c");
        }
        _ => {}
    }
    let mut b = String::new();
    let mut written = 0;
    for (i, &c) in s.as_bytes().iter().enumerate() {
        if c == b',' {
            filter_srcset_element(&s, written, i, &mut b);
            b.push(',');
            written = i + 1;
        }
    }
    filter_srcset_element(&s, written, s.len(), &mut b);
    b
}

/// `isHTMLSpace` (url.go:173).
fn is_html_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// `isHTMLSpaceOrASCIIAlnum` (url.go:177).
fn is_html_space_or_ascii_alnum(c: u8) -> bool {
    is_html_space(c) || c.is_ascii_alphanumeric()
}

/// `filterSrcsetElement` (url.go:181).
fn filter_srcset_element(s: &str, left: usize, right: usize, b: &mut String) {
    let bytes = s.as_bytes();
    let mut start = left;
    while start < right && is_html_space(bytes[start]) {
        start += 1;
    }
    let mut end = right;
    for (i, &c) in bytes.iter().enumerate().take(right).skip(start) {
        if is_html_space(c) {
            end = i;
            break;
        }
    }
    let url = &s[start..end];
    if is_safe_url(url) {
        let metadata_ok = bytes[end..right]
            .iter()
            .all(|&c| is_html_space_or_ascii_alnum(c));
        if metadata_ok {
            b.push_str(&s[left..start]);
            process_url_onto(url, true, b);
            b.push_str(&s[end..right]);
            return;
        }
    }
    b.push('#');
    b.push_str(FILTER_FAILSAFE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter() {
        assert!(is_safe_url("HTTP://x"));
        assert!(is_safe_url("/a:b"));
        assert!(!is_safe_url("javascript:x"));
        assert!(is_safe_url("httpſ://x"));
        let v = Value::str("javascript:alert(1)");
        assert_eq!(url_filter(&[Some(&v)]), "#ZgotmplZ");
    }

    #[test]
    fn normalize_and_escape() {
        let v = Value::str("/a b%20c%zz?q=é&x");
        assert_eq!(url_normalizer(&[Some(&v)]), "/a%20b%20c%25zz?q=%c3%a9&x");
        assert_eq!(
            url_escaper(&[Some(&v)]),
            "%2fa%20b%2520c%25zz%3fq%3d%c3%a9%26x"
        );
        let v = Value::str("a.png 1x, javascript:x 2x");
        assert_eq!(srcset_filter_and_escaper(&[Some(&v)]), "a.png 1x,#ZgotmplZ");
    }
}
