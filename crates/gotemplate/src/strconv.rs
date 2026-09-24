//! The pieces of Go's `strconv` (and the `unicode` predicates behind them) that templates reach:
//! quoting for `%q` and error messages, unquoting of string and character literals, and the
//! integer/float parsers and float formatter the template number rules are written against.

use crate::unicode_tables::{IS_DIGIT_RANGES, IS_LETTER_RANGES, IS_PRINT_RANGES};

fn in_table(r: u32, table: &[(u32, u32)]) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < r {
                std::cmp::Ordering::Less
            } else if lo > r {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `unicode.IsPrint` (and `strconv.IsPrint`, which is defined to agree with it).
pub(crate) fn is_print(r: u32) -> bool {
    in_table(r, IS_PRINT_RANGES)
}

/// `unicode.IsLetter`.
pub(crate) fn is_letter(r: u32) -> bool {
    in_table(r, IS_LETTER_RANGES)
}

/// `unicode.IsDigit`.
pub(crate) fn is_digit(r: u32) -> bool {
    in_table(r, IS_DIGIT_RANGES)
}

const LOWERHEX: &[u8; 16] = b"0123456789abcdef";

/// `appendEscapedRune` (strconv/quote.go).
fn append_escaped_rune(out: &mut String, r: u32, quote: u8, ascii_only: bool) {
    if r == u32::from(quote) || r == u32::from(b'\\') {
        out.push('\\');
        out.push(char::from_u32(r).unwrap_or('\u{fffd}'));
        return;
    }
    if ascii_only {
        if r < 0x80 && is_print(r) {
            out.push(char::from_u32(r).unwrap_or('\u{fffd}'));
            return;
        }
    } else if is_print(r) {
        out.push(char::from_u32(r).unwrap_or('\u{fffd}'));
        return;
    }
    match r {
        0x07 => out.push_str("\\a"),
        0x08 => out.push_str("\\b"),
        0x0c => out.push_str("\\f"),
        0x0a => out.push_str("\\n"),
        0x0d => out.push_str("\\r"),
        0x09 => out.push_str("\\t"),
        0x0b => out.push_str("\\v"),
        _ => {
            if r < u32::from(b' ') || r == 0x7f {
                out.push_str("\\x");
                out.push(LOWERHEX[(r >> 4) as usize & 0xf] as char);
                out.push(LOWERHEX[r as usize & 0xf] as char);
            } else {
                let r = if char::from_u32(r).is_none() {
                    0xfffd
                } else {
                    r
                };
                if r < 0x10000 {
                    out.push_str("\\u");
                    for s in (0..4).rev() {
                        out.push(LOWERHEX[(r >> (s * 4)) as usize & 0xf] as char);
                    }
                } else {
                    out.push_str("\\U");
                    for s in (0..8).rev() {
                        out.push(LOWERHEX[(r >> (s * 4)) as usize & 0xf] as char);
                    }
                }
            }
        }
    }
}

fn quote_with(s: &str, quote: u8, ascii_only: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote as char);
    for c in s.chars() {
        append_escaped_rune(&mut out, c as u32, quote, ascii_only);
    }
    out.push(quote as char);
    out
}

/// `strconv.Quote`. Rust strings are valid UTF-8, so Go's `\x` escape for an undecodable byte
/// has no counterpart here.
pub(crate) fn quote(s: &str) -> String {
    quote_with(s, b'"', false)
}

/// `strconv.QuoteToASCII`.
pub(crate) fn quote_to_ascii(s: &str) -> String {
    quote_with(s, b'"', true)
}

/// `strconv.QuoteRune` (`ascii_only`: `QuoteRuneToASCII`).
pub(crate) fn quote_rune(r: u32, ascii_only: bool) -> String {
    let r = if char::from_u32(r).is_none() {
        0xfffd
    } else {
        r
    };
    let mut out = String::from("'");
    append_escaped_rune(&mut out, r, b'\'', ascii_only);
    out.push('\'');
    out
}

/// `strconv.CanBackquote`.
pub(crate) fn can_backquote(s: &str) -> bool {
    for c in s.chars() {
        if c == '\u{feff}' {
            return false;
        }
        if (c as u32) < 0x80 && ((c < ' ' && c != '\t') || c == '`' || c == '\u{7f}') {
            return false;
        }
    }
    true
}

fn unhex(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some(u32::from(b - b'0')),
        b'a'..=b'f' => Some(u32::from(b - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(b - b'A' + 10)),
        _ => None,
    }
}

/// The result of `strconv.UnquoteChar`: the value, whether it is a multi-byte rune (as opposed
/// to a single byte from `\x` or an octal escape), and the unconsumed tail.
pub(crate) struct UnquotedChar<'a> {
    pub value: u32,
    pub multibyte: bool,
    pub tail: &'a str,
}

/// `strconv.UnquoteChar`. `Err(())` is `strconv.ErrSyntax`.
pub(crate) fn unquote_char(s: &str, quote: u8) -> Result<UnquotedChar<'_>, ()> {
    let bytes = s.as_bytes();
    let Some(&c) = bytes.first() else {
        return Err(());
    };
    if c == quote && (quote == b'\'' || quote == b'"') {
        return Err(());
    }
    if c >= 0x80 {
        let ch = s.chars().next().ok_or(())?;
        return Ok(UnquotedChar {
            value: ch as u32,
            multibyte: true,
            tail: &s[ch.len_utf8()..],
        });
    }
    if c != b'\\' {
        return Ok(UnquotedChar {
            value: u32::from(c),
            multibyte: false,
            tail: &s[1..],
        });
    }
    if bytes.len() <= 1 {
        return Err(());
    }
    let c = bytes[1];
    let mut rest = &s[2..];
    let mut multibyte = false;
    let value = match c {
        b'a' => 0x07,
        b'b' => 0x08,
        b'f' => 0x0c,
        b'n' => 0x0a,
        b'r' => 0x0d,
        b't' => 0x09,
        b'v' => 0x0b,
        b'x' | b'u' | b'U' => {
            let n = match c {
                b'x' => 2,
                b'u' => 4,
                _ => 8,
            };
            if rest.len() < n {
                return Err(());
            }
            let mut v: u32 = 0;
            for &b in &rest.as_bytes()[..n] {
                v = (v << 4) | unhex(b).ok_or(())?;
            }
            rest = &rest[n..];
            if c != b'x' {
                if char::from_u32(v).is_none() {
                    return Err(());
                }
                multibyte = true;
            }
            v
        }
        b'0'..=b'7' => {
            let mut v = u32::from(c - b'0');
            if rest.len() < 2 {
                return Err(());
            }
            for &b in &rest.as_bytes()[..2] {
                if !(b'0'..=b'7').contains(&b) {
                    return Err(());
                }
                v = (v << 3) | u32::from(b - b'0');
            }
            rest = &rest[2..];
            if v > 255 {
                return Err(());
            }
            v
        }
        b'\\' => u32::from(b'\\'),
        b'\'' | b'"' => {
            if c != quote {
                return Err(());
            }
            u32::from(c)
        }
        _ => return Err(()),
    };
    Ok(UnquotedChar {
        value,
        multibyte,
        tail: rest,
    })
}

/// `strconv.Unquote`. `Err(())` is `strconv.ErrSyntax`.
///
/// A `\x` or octal escape of a byte `>= 0x80` produces a byte that is not valid UTF-8 on its own;
/// Go keeps the raw byte, this port (whose strings are UTF-8) decodes the result lossily.
pub(crate) fn unquote(s: &str) -> Result<String, ()> {
    let b = s.as_bytes();
    if b.len() < 2 {
        return Err(());
    }
    let quote = b[0];
    let end = match b[1..].iter().position(|&c| c == quote) {
        Some(e) => e + 2,
        None => return Err(()),
    };
    match quote {
        b'`' => {
            if end != b.len() {
                return Err(());
            }
            Ok(s[1..end - 1].chars().filter(|&c| c != '\r').collect())
        }
        b'"' | b'\'' => {
            let head = &s[..end];
            if !head.contains('\\') && !head.contains('\n') {
                let valid = if quote == b'"' {
                    true
                } else {
                    let inner = &s[1..end - 1];
                    let mut it = inner.chars();
                    match it.next() {
                        Some(ch) => 1 + ch.len_utf8() + 1 == end,
                        None => false,
                    }
                };
                if valid {
                    if end != b.len() {
                        return Err(());
                    }
                    return Ok(s[1..end - 1].to_string());
                }
            }
            let mut buf: Vec<u8> = Vec::new();
            let mut rest = &s[1..];
            while let Some(&c0) = rest.as_bytes().first() {
                if c0 == quote {
                    break;
                }
                if c0 == b'\n' {
                    return Err(());
                }
                let u = unquote_char(rest, quote)?;
                rest = u.tail;
                if u.value < 0x80 || !u.multibyte {
                    buf.push(u.value as u8);
                } else {
                    let ch = char::from_u32(u.value).unwrap_or('\u{fffd}');
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                }
                if quote == b'\'' {
                    break;
                }
            }
            if rest.as_bytes().first() != Some(&quote) {
                return Err(());
            }
            if rest.len() != 1 {
                return Err(());
            }
            Ok(String::from_utf8_lossy(&buf).into_owned())
        }
        _ => Err(()),
    }
}

fn lower(c: u8) -> u8 {
    c | 0x20
}

/// `underscoreOK` (strconv/atoi.go).
fn underscore_ok(s: &str) -> bool {
    let mut b = s.as_bytes();
    let mut saw = b'^';
    let mut i = 0;
    if !b.is_empty() && (b[0] == b'-' || b[0] == b'+') {
        b = &b[1..];
    }
    let mut hex = false;
    if b.len() >= 2 && b[0] == b'0' && matches!(lower(b[1]), b'b' | b'o' | b'x') {
        i = 2;
        saw = b'0';
        hex = lower(b[1]) == b'x';
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() || (hex && (b'a'..=b'f').contains(&lower(c))) {
            saw = b'0';
            i += 1;
            continue;
        }
        if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
            i += 1;
            continue;
        }
        if saw == b'_' {
            return false;
        }
        saw = b'!';
        i += 1;
    }
    saw != b'_'
}

/// `strconv.ParseUint(s, 0, 64)`; `None` for any error.
pub(crate) fn parse_uint0(s: &str) -> Option<u64> {
    let s0 = s;
    let mut b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut base: u64 = 10;
    if b[0] == b'0' {
        if b.len() >= 3 && lower(b[1]) == b'b' {
            base = 2;
            b = &b[2..];
        } else if b.len() >= 3 && lower(b[1]) == b'o' {
            base = 8;
            b = &b[2..];
        } else if b.len() >= 3 && lower(b[1]) == b'x' {
            base = 16;
            b = &b[2..];
        } else {
            base = 8;
            b = &b[1..];
        }
    }
    let mut underscores = false;
    let mut n: u64 = 0;
    for &c in b {
        let d = if c == b'_' {
            underscores = true;
            continue;
        } else if c.is_ascii_digit() {
            u64::from(c - b'0')
        } else if lower(c).is_ascii_lowercase() {
            u64::from(lower(c) - b'a' + 10)
        } else {
            return None;
        };
        if d >= base {
            return None;
        }
        n = n.checked_mul(base)?.checked_add(d)?;
    }
    if underscores && !underscore_ok(s0) {
        return None;
    }
    Some(n)
}

/// `strconv.ParseInt(s, 0, 64)`; `None` for any error.
pub(crate) fn parse_int0(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.as_bytes()[0] {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    let un = parse_uint0(body)?;
    // ParseUint was handed the unsigned body; underscoreOK is re-checked on it there.
    if neg {
        if un > 1u64 << 63 {
            return None;
        }
        Some((un as i64).wrapping_neg())
    } else {
        if un >= 1u64 << 63 {
            return None;
        }
        Some(un as i64)
    }
}

/// `strconv.ParseFloat(s, 64)`; `None` for any error (syntax or range).
///
/// Decimal input is correctly rounded (Rust's parser and Go's agree on that). Hexadecimal
/// mantissas wider than 64 bits are truncated, a divergence only a template literal of more than
/// sixteen hex digits could reach.
pub(crate) fn parse_float(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    // Infinity and NaN, case-insensitively, as Go's `special` accepts them.
    let rest = &s[i..];
    let lower_rest = rest.to_ascii_lowercase();
    if lower_rest == "inf" || lower_rest == "infinity" {
        return Some(if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if lower_rest == "nan" && i == 0 {
        return Some(f64::NAN);
    }
    let mut hex = false;
    if i + 2 < b.len() && b[i] == b'0' && lower(b[i + 1]) == b'x' {
        hex = true;
        i += 2;
    }
    let mut underscores = false;
    let mut sawdot = false;
    let mut sawdigits = false;
    let mut mant: u64 = 0;
    let mut hex_exp_adj: i64 = 0;
    let mut hex_trunc = false;
    let mut hex_digits = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'_' {
            underscores = true;
        } else if c == b'.' {
            if sawdot {
                break;
            }
            sawdot = true;
        } else if c.is_ascii_digit() || (hex && (b'a'..=b'f').contains(&lower(c))) {
            sawdigits = true;
            if hex {
                let d = unhex(c).map(u64::from).unwrap_or(0);
                if hex_digits < 16 {
                    if mant != 0 || d != 0 {
                        hex_digits += 1;
                    }
                    mant = mant * 16 + d;
                    if sawdot {
                        hex_exp_adj -= 4;
                    }
                } else {
                    if d != 0 {
                        hex_trunc = true;
                    }
                    if !sawdot {
                        hex_exp_adj += 4;
                    }
                }
            }
        } else {
            break;
        }
        i += 1;
    }
    if !sawdigits {
        return None;
    }
    let exp_char = if hex { b'p' } else { b'e' };
    let mut exp: i64 = 0;
    if i < b.len() && lower(b[i]) == exp_char {
        i += 1;
        if i >= b.len() {
            return None;
        }
        let mut esign = 1;
        if b[i] == b'+' {
            i += 1;
        } else if b[i] == b'-' {
            i += 1;
            esign = -1;
        }
        if i >= b.len() || !b[i].is_ascii_digit() {
            return None;
        }
        let mut e: i64 = 0;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            if b[i] == b'_' {
                underscores = true;
            } else if e < 10000 {
                e = e * 10 + i64::from(b[i] - b'0');
            }
            i += 1;
        }
        exp = e * esign;
    } else if hex {
        return None;
    }
    if i != b.len() {
        return None;
    }
    if underscores && !underscore_ok(s) {
        return None;
    }
    let v = if hex {
        let _ = hex_trunc;
        (mant as f64) * 2f64.powi((exp + hex_exp_adj).clamp(-2000, 2000) as i32)
    } else {
        let cleaned: String = s.chars().filter(|&c| c != '_').collect();
        cleaned.parse::<f64>().ok()?
    };
    if v.is_infinite() {
        return None;
    }
    Some(if hex && neg { -v } else { v })
}

/// A decimal digit string `d` (no leading zeros) with the decimal point after `dp` digits, as
/// strconv's `decimalSlice`.
struct Digits {
    d: Vec<u8>,
    dp: i32,
}

fn trim_zeros(mut d: Vec<u8>) -> Vec<u8> {
    while d.last() == Some(&b'0') {
        d.pop();
    }
    d
}

/// Parses Rust's `{:e}` output of a non-negative value into strconv digits.
fn digits_from_exp(s: &str) -> Digits {
    let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let d: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).collect();
    let d = trim_zeros(d);
    if d.is_empty() {
        return Digits { d, dp: 0 };
    }
    Digits { d, dp: exp + 1 }
}

/// Parses Rust's fixed `{:.N}` output of a non-negative value into strconv digits.
fn digits_from_fixed(s: &str) -> Digits {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let mut all: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
    let mut dp = int.len() as i32;
    let lead = all.iter().take_while(|&&c| c == b'0').count();
    all.drain(..lead);
    dp -= lead as i32;
    let d = trim_zeros(all);
    if d.is_empty() {
        return Digits { d, dp: 0 };
    }
    Digits { d, dp }
}

fn fmt_e(out: &mut String, neg: bool, d: &Digits, prec: i32, fmt: u8) {
    if neg {
        out.push('-');
    }
    let nd = d.d.len() as i32;
    out.push(if nd != 0 { d.d[0] as char } else { '0' });
    if prec > 0 {
        out.push('.');
        let mut i = 1;
        let m = nd.min(prec + 1);
        if i < m {
            for &c in &d.d[i as usize..m as usize] {
                out.push(c as char);
            }
            i = m;
        }
        while i <= prec {
            out.push('0');
            i += 1;
        }
    }
    out.push(fmt as char);
    let mut exp = d.dp - 1;
    if nd == 0 {
        exp = 0;
    }
    if exp < 0 {
        out.push('-');
        exp = -exp;
    } else {
        out.push('+');
    }
    if exp < 10 {
        out.push('0');
        out.push((b'0' + exp as u8) as char);
    } else if exp < 100 {
        out.push((b'0' + (exp / 10) as u8) as char);
        out.push((b'0' + (exp % 10) as u8) as char);
    } else {
        out.push((b'0' + (exp / 100) as u8) as char);
        out.push((b'0' + ((exp / 10) % 10) as u8) as char);
        out.push((b'0' + (exp % 10) as u8) as char);
    }
}

fn fmt_f(out: &mut String, neg: bool, d: &Digits, prec: i32) {
    if neg {
        out.push('-');
    }
    let nd = d.d.len() as i32;
    if d.dp > 0 {
        let m = nd.min(d.dp);
        for &c in &d.d[..m as usize] {
            out.push(c as char);
        }
        for _ in m..d.dp {
            out.push('0');
        }
    } else {
        out.push('0');
    }
    if prec > 0 {
        out.push('.');
        for i in 0..prec {
            let j = d.dp + i;
            let ch = if 0 <= j && j < nd {
                d.d[j as usize] as char
            } else {
                '0'
            };
            out.push(ch);
        }
    }
}

/// `strconv.FormatFloat(f, fmt, prec, 64)` for the formats `e E f g G` (`prec < 0` is the
/// shortest representation that round-trips).
pub(crate) fn format_float(f: f64, fmt: u8, prec: i32) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-Inf" } else { "+Inf" }.to_string();
    }
    let neg = f.is_sign_negative();
    let a = f.abs();
    let shortest = prec < 0;
    let mut out = String::new();
    if shortest {
        let d = digits_from_exp(&format!("{a:e}"));
        let nd = d.d.len() as i32;
        let prec = match fmt {
            b'e' | b'E' => (nd - 1).max(0),
            b'f' => (nd - d.dp).max(0),
            _ => nd,
        };
        format_digits(&mut out, true, neg, &d, prec, fmt);
        return out;
    }
    match fmt {
        b'e' | b'E' => {
            let d = digits_from_exp(&format!("{a:.*e}", prec as usize));
            fmt_e(&mut out, neg, &d, prec, fmt);
        }
        b'f' => {
            let d = digits_from_fixed(&format!("{a:.*}", prec as usize));
            fmt_f(&mut out, neg, &d, prec);
        }
        _ => {
            let prec = if prec == 0 { 1 } else { prec };
            let d = digits_from_exp(&format!("{a:.*e}", (prec - 1) as usize));
            format_digits(&mut out, false, neg, &d, prec, fmt);
        }
    }
    out
}

fn format_digits(out: &mut String, shortest: bool, neg: bool, d: &Digits, prec: i32, fmt: u8) {
    match fmt {
        b'e' | b'E' => fmt_e(out, neg, d, prec, fmt),
        b'f' => fmt_f(out, neg, d, prec),
        _ => {
            let nd = d.d.len() as i32;
            let mut prec = prec;
            let mut eprec = prec;
            if eprec > nd && nd >= d.dp {
                eprec = nd;
            }
            if shortest {
                eprec = 6;
            }
            let exp = d.dp - 1;
            if exp < -4 || exp >= eprec {
                if prec > nd {
                    prec = nd;
                }
                fmt_e(out, neg, d, prec - 1, fmt + b'e' - b'g');
                return;
            }
            if prec > d.dp {
                prec = nd;
            }
            fmt_f(out, neg, d, (prec - d.dp).max(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_matches_go_spellings() {
        assert_eq!(quote("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(quote("\x7f\u{1}\n"), r#""\x7f\x01\n""#);
        assert_eq!(quote("\u{200b}é"), "\"\\u200bé\"");
        assert_eq!(quote_to_ascii("é"), r#""\u00e9""#);
        assert_eq!(quote_rune(u32::from('\''), false), r"'\''");
    }

    #[test]
    fn unquote_branches() {
        assert_eq!(unquote(r#""a\tb""#), Ok("a\tb".to_string()));
        assert_eq!(unquote("`a\r\nb`"), Ok("a\nb".to_string()));
        assert_eq!(unquote(r#""\u00e9""#), Ok("é".to_string()));
        assert_eq!(unquote(r#""\q""#), Err(()));
        assert_eq!(unquote("\"a\nb\""), Err(()));
        assert_eq!(unquote(r#""\101""#), Ok("A".to_string()));
    }

    #[test]
    fn number_parsers() {
        assert_eq!(parse_int0("0x1F"), Some(31));
        assert_eq!(parse_int0("-0"), Some(0));
        assert_eq!(parse_uint0("-0"), None);
        assert_eq!(parse_int0("089"), None);
        assert_eq!(parse_int0("1_000"), Some(1000));
        assert_eq!(parse_int0("1__0"), None);
        assert_eq!(parse_int0("9223372036854775808"), None);
        assert_eq!(parse_int0("-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse_float("089"), Some(89.0));
        assert_eq!(parse_float("1e400"), None);
        assert_eq!(parse_float("0x1p-2"), Some(0.25));
        assert_eq!(parse_float("1.5e3"), Some(1500.0));
    }

    #[test]
    fn float_formats() {
        assert_eq!(format_float(1e6, b'g', -1), "1e+06");
        assert_eq!(format_float(123456.0, b'g', -1), "123456");
        assert_eq!(format_float(0.0001, b'g', -1), "0.0001");
        assert_eq!(format_float(0.00001, b'g', -1), "1e-05");
        assert_eq!(format_float(1.5, b'f', 6), "1.500000");
        assert_eq!(format_float(-0.0, b'g', -1), "-0");
        assert_eq!(format_float(1.5, b'g', 5), "1.5");
        assert_eq!(format_float(1234.5678, b'e', 2), "1.23e+03");
        assert_eq!(format_float(0.0, b'e', 2), "0.00e+00");
    }
}
