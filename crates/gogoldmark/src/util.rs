//! Port of goldmark's `util` package (`util/util.go`, `util/html5entities.go`,
//! `util/unicode_case_folding.go`) — the parts the parser and the HTML renderer call.
//!
//! The byte tables are copied verbatim from util.go; the entity table, the case-folding table and
//! the rune predicates live in [`crate::tables_generated`], generated from goldmark's own data.

use crate::tables_generated::{
    CASE_FOLDINGS, HTML5_ENTITIES, PUNCT_RUNE_RANGES, SPACE_RUNE_RANGES,
};

/// `spaceTable` (util.go), verbatim.
#[rustfmt::skip]
static SPACE_TABLE: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// `punctTable` (util.go), verbatim.
#[rustfmt::skip]
static PUNCT_TABLE: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1,
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1,
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// `urlEscapeTable` (util.go), verbatim.
#[rustfmt::skip]
static URL_ESCAPE_TABLE: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 0, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 1,
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// `utf8lenTable` (util.go), verbatim.
#[rustfmt::skip]
static UTF8_LEN_TABLE: [u8; 256] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// `urlTable` (util.go), verbatim.
#[rustfmt::skip]
static URL_TABLE: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 5, 1, 5, 5, 1, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 1, 1, 0, 1, 0, 1,
    1, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 1, 1, 1, 1, 1,
    1, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

/// `emailTable` (util.go), verbatim.
#[rustfmt::skip]
static EMAIL_TABLE: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 0, 1, 1, 1, 1, 1, 0, 0, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 1,
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
/// Port of `util.IsPunct`: ASCII punctuation per goldmark's table.
pub fn is_punct(c: u8) -> bool {
    PUNCT_TABLE[c as usize] == 1
}

/// Port of `util.IsSpace`: `\t \n \v \f \r` and space.
pub fn is_space(c: u8) -> bool {
    SPACE_TABLE[c as usize] == 1
}

/// Port of `util.IsNumeric`.
pub fn is_numeric(c: u8) -> bool {
    c.is_ascii_digit()
}

/// Port of `util.IsHexDecimal`.
pub fn is_hex_decimal(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// Port of `util.IsAlphaNumeric`.
pub fn is_alpha_numeric(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

fn in_ranges(ranges: &[(u32, u32)], r: char) -> bool {
    let v = r as u32;
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < v {
                std::cmp::Ordering::Less
            } else if lo > v {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Port of `util.IsPunctRune`: `unicode.IsSymbol(r) || unicode.IsPunct(r)`.
pub fn is_punct_rune(r: char) -> bool {
    in_ranges(&PUNCT_RUNE_RANGES, r)
}

/// Port of `util.IsSpaceRune`.
pub fn is_space_rune(r: char) -> bool {
    in_ranges(&SPACE_RUNE_RANGES, r)
}

/// Port of `utf8.RuneStart`.
pub(crate) fn rune_start(b: u8) -> bool {
    b & 0xC0 != 0x80
}

/// Port of `utf8.DecodeRune`: an empty input is `(RuneError, 0)`, an invalid encoding is
/// `(RuneError, 1)`.
pub(crate) fn decode_rune(b: &[u8]) -> (char, usize) {
    let Some(&c) = b.first() else {
        return ('\u{FFFD}', 0);
    };
    if c < 0x80 {
        return (c as char, 1);
    }
    let n = match c {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return ('\u{FFFD}', 1),
    };
    if b.len() < n {
        return ('\u{FFFD}', 1);
    }
    match std::str::from_utf8(&b[..n]) {
        Ok(s) => match s.chars().next() {
            Some(ch) => (ch, n),
            None => ('\u{FFFD}', 1),
        },
        Err(_) => ('\u{FFFD}', 1),
    }
}

/// Port of `util.ToRune`: the rune containing byte `pos`.
pub fn to_rune(source: &[u8], pos: usize) -> char {
    let mut i = pos as isize;
    while i >= 0 {
        if rune_start(source[i as usize]) {
            break;
        }
        i -= 1;
    }
    let i = i.max(0) as usize;
    decode_rune(&source[i..]).0
}

/// Port of `util.ToValidRune` over the value Go's `rune(v)` conversion would produce:
/// zero, surrogates and anything past U+10FFFF (including values that wrap negative in
/// `int32`) become U+FFFD.
pub(crate) fn to_valid_rune(v: u64) -> char {
    if v == 0 || v > 0x10FFFF {
        return '\u{FFFD}';
    }
    char::from_u32(v as u32).unwrap_or('\u{FFFD}')
}

/// Port of `util.TabWidth`.
pub fn tab_width(current_pos: isize) -> isize {
    4 - current_pos % 4
}

/// Port of `util.IndentPosition`.
pub fn indent_position(bs: &[u8], current_pos: isize, width: isize) -> (isize, isize) {
    indent_position_padding(bs, current_pos, 0, width)
}

/// Port of `util.IndentPositionPadding`.
pub fn indent_position_padding(
    bs: &[u8],
    current_pos: isize,
    paddingv: isize,
    width: isize,
) -> (isize, isize) {
    if width == 0 {
        return (0, paddingv);
    }
    let mut w: isize = 0;
    let mut i: isize = 0;
    let l = bs.len() as isize;
    let mut p = paddingv;
    while i < l {
        if p > 0 {
            p -= 1;
            w += 1;
            i += 1;
            continue;
        }
        let c = bs[i as usize];
        if c == b'\t' && w < width {
            w += tab_width(current_pos + w);
        } else if c == b' ' && w < width {
            w += 1;
        } else {
            break;
        }
        i += 1;
    }
    if w >= width {
        return (i - paddingv, w - width);
    }
    (-1, -1)
}

/// Port of `util.IndentWidth`: `(width, pos)`.
pub fn indent_width(bs: &[u8], current_pos: isize) -> (isize, isize) {
    let mut width: isize = 0;
    let mut pos: isize = 0;
    for &c in bs {
        match c {
            b' ' => {
                width += 1;
                pos += 1;
            }
            b'\t' => {
                width += tab_width(current_pos + width);
                pos += 1;
            }
            _ => return (width, pos),
        }
    }
    (width, pos)
}

/// Port of `util.FirstNonSpacePosition`.
pub fn first_non_space_position(bs: &[u8]) -> isize {
    for (i, &c) in bs.iter().enumerate() {
        if c == b' ' || c == b'\t' {
            continue;
        }
        if c == b'\n' {
            return -1;
        }
        return i as isize;
    }
    -1
}

/// Port of `util.IsBlank`.
pub fn is_blank(bs: &[u8]) -> bool {
    bs.iter().all(|&b| is_space(b))
}

/// Port of `util.TrimLeftLength` for a set of bytes.
pub fn trim_left_length(source: &[u8], b: &[u8]) -> usize {
    source.iter().take_while(|c| b.contains(c)).count()
}

/// Port of `util.TrimRightLength` for a set of bytes.
pub fn trim_right_length(source: &[u8], b: &[u8]) -> usize {
    source.iter().rev().take_while(|c| b.contains(c)).count()
}

/// Port of `util.TrimLeftSpaceLength`.
pub fn trim_left_space_length(source: &[u8]) -> usize {
    source.iter().take_while(|&&c| is_space(c)).count()
}

/// Port of `util.TrimRightSpaceLength`.
pub fn trim_right_space_length(source: &[u8]) -> usize {
    source.iter().rev().take_while(|&&c| is_space(c)).count()
}

/// Port of `util.TrimLeftSpace`.
pub fn trim_left_space(source: &[u8]) -> &[u8] {
    &source[trim_left_space_length(source)..]
}

/// Port of `util.TrimRightSpace`.
pub fn trim_right_space(source: &[u8]) -> &[u8] {
    &source[..source.len() - trim_right_space_length(source)]
}

/// Port of `util.ReadWhile` over `source[index.0..index.1]`: `(stop, matched_any)`.
pub(crate) fn read_while(
    source: &[u8],
    start: usize,
    limit: usize,
    pred: fn(u8) -> bool,
) -> (usize, bool) {
    let mut j = start;
    let mut ok = false;
    while j < limit {
        if pred(source[j]) {
            ok = true;
            j += 1;
            continue;
        }
        break;
    }
    (j, ok)
}

/// Port of `util.DoFullUnicodeCaseFolding`.
pub fn do_full_unicode_case_folding(v: &[u8]) -> Vec<u8> {
    let mut out: Option<Vec<u8>> = None;
    let mut n = 0;
    let mut i = 0;
    while i < v.len() {
        let c = v[i];
        if c < 0xb5 {
            if c.is_ascii_uppercase() {
                let o = out.get_or_insert_with(|| Vec::with_capacity(v.len() + 20));
                o.extend_from_slice(&v[n..i]);
                o.push(c + 32);
                n = i + 1;
            }
            i += 1;
            continue;
        }
        if !rune_start(c) {
            i += 1;
            continue;
        }
        let (r, length) = decode_rune(&v[i..]);
        if r == '\u{FFFD}' {
            i += 1;
            continue;
        }
        let Ok(idx) = CASE_FOLDINGS.binary_search_by(|&(from, _)| from.cmp(&r)) else {
            i += 1;
            continue;
        };
        let o = out.get_or_insert_with(|| Vec::with_capacity(v.len() + 20));
        o.extend_from_slice(&v[n..i]);
        o.extend_from_slice(CASE_FOLDINGS[idx].1.as_bytes());
        i += length;
        n = i;
    }
    match out {
        Some(mut o) => {
            o.extend_from_slice(&v[n..]);
            o
        }
        None => v.to_vec(),
    }
}

/// Port of `util.ReplaceSpaces`.
pub fn replace_spaces(source: &[u8], repl: u8) -> Vec<u8> {
    let mut ret: Option<Vec<u8>> = None;
    let mut start: isize = -1;
    for (i, &c) in source.iter().enumerate() {
        let iss = is_space(c);
        if start < 0 && iss {
            start = i as isize;
            continue;
        } else if start >= 0 && iss {
            continue;
        } else if start >= 0 {
            let r = ret.get_or_insert_with(|| {
                let mut r = Vec::with_capacity(source.len());
                r.extend_from_slice(&source[..start as usize]);
                r
            });
            r.push(repl);
            start = -1;
        }
        if let Some(r) = ret.as_mut() {
            r.push(c);
        }
    }
    if start >= 0
        && let Some(r) = ret.as_mut()
    {
        r.push(repl);
    }
    ret.unwrap_or_else(|| source.to_vec())
}

/// Port of `util.ToLinkReference`: trim, full case fold, collapse space runs.
pub fn to_link_reference(v: &[u8]) -> Vec<u8> {
    let v = trim_left_space(v);
    let v = trim_right_space(v);
    let v = do_full_unicode_case_folding(v);
    replace_spaces(&v, b' ')
}

/// Port of `util.EscapeHTMLByte`: `"`, `&`, `<`, `>` and NUL (→ U+FFFD).
pub fn escape_html_byte(b: u8) -> Option<&'static [u8]> {
    match b {
        0 => Some("\u{FFFD}".as_bytes()),
        b'"' => Some(b"&quot;"),
        b'&' => Some(b"&amp;"),
        b'<' => Some(b"&lt;"),
        b'>' => Some(b"&gt;"),
        _ => None,
    }
}

/// Port of `util.EscapeHTML`.
pub fn escape_html(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len());
    for &c in v {
        match escape_html_byte(c) {
            Some(e) => out.extend_from_slice(e),
            None => out.push(c),
        }
    }
    out
}

/// Port of `util.UnescapePunctuations`.
pub fn unescape_punctuations(source: &[u8]) -> Vec<u8> {
    let limit = source.len();
    let mut out = Vec::with_capacity(limit);
    let mut i = 0;
    while i < limit {
        let c = source[i];
        if i + 1 < limit && c == b'\\' && is_punct(source[i + 1]) {
            out.push(source[i + 1]);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `strconv.ParseUint(s, base, 32)`'s value for the digit strings goldmark hands it: an overflow
/// saturates (Go returns the maximum with `ErrRange`), a syntax error is zero. `base == 0` is
/// Go's prefix detection, and for the all-digit strings reachable here that means a leading `0`
/// selects **octal** — so `&#0123;` is U+0053 and `&#08;` is a syntax error.
pub(crate) fn go_parse_uint32(s: &[u8], base: u32) -> u64 {
    let (digits, base) = if base == 0 {
        if s.first() == Some(&b'0') {
            (&s[1..], 8)
        } else {
            (s, 10)
        }
    } else {
        (s, base)
    };
    let mut n: u64 = 0;
    for &c in digits {
        let d = match c {
            b'0'..=b'9' => u64::from(c - b'0'),
            b'a'..=b'z' => u64::from(c - b'a' + 10),
            b'A'..=b'Z' => u64::from(c - b'A' + 10),
            _ => return 0,
        };
        if d >= u64::from(base) {
            return 0;
        }
        n = n.saturating_mul(u64::from(base)).saturating_add(d);
    }
    n.min(u64::from(u32::MAX))
}

fn push_rune(out: &mut Vec<u8>, r: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(r.encode_utf8(&mut buf).as_bytes());
}

/// Port of `util.ResolveNumericReferences` (`&#1234;`, `&#x1F;`).
pub fn resolve_numeric_references(source: &[u8]) -> Vec<u8> {
    let limit = source.len();
    let mut out = Vec::with_capacity(limit);
    let mut n = 0;
    let mut i = 0;
    while i < limit {
        if source[i] == b'&' {
            let pos = i;
            let next = i + 1;
            if next < limit && source[next] == b'#' {
                let nnext = next + 1;
                if nnext < limit {
                    let nc = source[nnext];
                    if nc == b'x' || nc == b'X' {
                        let start = nnext + 1;
                        let (j, ok) = read_while(source, start, limit, is_hex_decimal);
                        if ok && j < limit && source[j] == b';' {
                            let v = go_parse_uint32(&source[start..j], 16);
                            out.extend_from_slice(&source[n..pos]);
                            n = j + 1;
                            push_rune(&mut out, to_valid_rune(v));
                            i = j + 1;
                            continue;
                        }
                    } else if nc.is_ascii_digit() {
                        let start = nnext;
                        let (j, ok) = read_while(source, start, limit, is_numeric);
                        if ok && j < limit && j - start < 8 && source[j] == b';' {
                            let v = go_parse_uint32(&source[start..j], 0);
                            out.extend_from_slice(&source[n..pos]);
                            n = j + 1;
                            push_rune(&mut out, to_valid_rune(v));
                            i = j + 1;
                            continue;
                        }
                    }
                }
            }
            i = next;
            continue;
        }
        i += 1;
    }
    out.extend_from_slice(&source[n..]);
    out
}

/// Port of `util.LookUpHTML5EntityByName`: the characters of `&name;`.
pub fn lookup_html5_entity(name: &[u8]) -> Option<&'static [u8]> {
    HTML5_ENTITIES
        .binary_search_by(|&(k, _)| k.cmp(name))
        .ok()
        .map(|i| HTML5_ENTITIES[i].1)
}

/// Port of `util.ResolveEntityNames` (`&ouml;`).
pub fn resolve_entity_names(source: &[u8]) -> Vec<u8> {
    let limit = source.len();
    let mut out = Vec::with_capacity(limit);
    let mut n = 0;
    let mut i = 0;
    while i < limit {
        if source[i] == b'&' {
            let pos = i;
            let next = i + 1;
            if !(next < limit && source[next] == b'#') {
                let start = next;
                let (j, ok) = read_while(source, start, limit, is_alpha_numeric);
                if ok
                    && j < limit
                    && source[j] == b';'
                    && let Some(chars) = lookup_html5_entity(&source[start..j])
                {
                    out.extend_from_slice(&source[n..pos]);
                    n = j + 1;
                    out.extend_from_slice(chars);
                    i = j + 1;
                    continue;
                }
            }
            i = next;
            continue;
        }
        i += 1;
    }
    out.extend_from_slice(&source[n..]);
    out
}

/// Go's `url.QueryEscape` over raw bytes.
fn query_escape(v: &[u8], out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &c in v {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(c);
        } else if c == b' ' {
            out.push(b'+');
        } else {
            out.push(b'%');
            out.push(HEX[(c >> 4) as usize]);
            out.push(HEX[(c & 15) as usize]);
        }
    }
}

/// Port of `util.URLEscape`. With `resolve_reference`, backslash escapes, numeric references
/// and entity references are resolved first. `%xx` is kept — Go's check reads `v[i+1]` twice
/// and never `v[i+2]`, so `%4g` counts as an escape too.
pub fn url_escape(v: &[u8], resolve_reference: bool) -> Vec<u8> {
    let resolved;
    let v: &[u8] = if resolve_reference {
        let a = unescape_punctuations(v);
        let b = resolve_numeric_references(&a);
        resolved = resolve_entity_names(&b);
        &resolved
    } else {
        v
    };
    let limit = v.len();
    let mut out = Vec::with_capacity(limit + 8);
    // Go's CopyOnWriteBuffer: until the first write the original is returned untouched, which
    // is what keeps the lone truncated lead byte below (`u8len == 0`) in the output.
    let mut copied = false;
    let mut n = 0;
    let mut i = 0;
    while i < limit {
        let c = v[i];
        if URL_ESCAPE_TABLE[c as usize] == 1 {
            i += 1;
            continue;
        }
        if c == b'%' && i + 2 < limit && is_hex_decimal(v[i + 1]) && is_hex_decimal(v[i + 1]) {
            i += 3;
            continue;
        }
        let mut u8len = UTF8_LEN_TABLE[c as usize] as usize;
        if u8len == 99 {
            i += 1;
            continue;
        }
        if c == b' ' {
            copied = true;
            out.extend_from_slice(&v[n..i]);
            out.extend_from_slice(b"%20");
            i += 1;
            n = i;
            continue;
        }
        if u8len > v.len() {
            u8len = v.len() - 1;
        }
        if u8len == 0 {
            i += 1;
            n = i;
            continue;
        }
        copied = true;
        out.extend_from_slice(&v[n..i]);
        let stop = i + u8len;
        if stop > v.len() {
            i += 1;
            n = i;
            continue;
        }
        query_escape(&v[i..stop], &mut out);
        i += u8len;
        n = i;
    }
    if !copied {
        return v.to_vec();
    }
    if n < limit {
        out.extend_from_slice(&v[n..]);
    }
    out
}

/// Port of `util.FindURLIndex`: `[A-Za-z][A-Za-z0-9.+-]{1,31}:[^<>\x00-\x20]*`.
pub fn find_url_index(b: &[u8]) -> isize {
    let mut i = 0;
    if !(!b.is_empty() && URL_TABLE[b[i] as usize] & 7 == 7) {
        return -1;
    }
    i += 1;
    while i < b.len() {
        if URL_TABLE[b[i] as usize] & 4 != 4 {
            break;
        }
        i += 1;
    }
    if i == 1 || i > 33 || i >= b.len() {
        return -1;
    }
    if b[i] != b':' {
        return -1;
    }
    i += 1;
    while i < b.len() {
        if URL_TABLE[b[i] as usize] & 1 != 1 {
            break;
        }
        i += 1;
    }
    i as isize
}

/// One label of `emailDomainRegexp`: `[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?`, under
/// RE2's leftmost-first preference (the optional group takes the longest run that ends in an
/// alphanumeric). Returns the end of the label, or `None` when `b[at]` is not alphanumeric.
fn email_domain_label(b: &[u8], at: usize) -> Option<usize> {
    if at >= b.len() || !b[at].is_ascii_alphanumeric() {
        return None;
    }
    let body = at + 1;
    let mut best = body;
    let mut t = 0;
    // t counts the `[a-zA-Z0-9-]` characters before the closing alphanumeric.
    while t <= 61 {
        let close = body + t;
        if close >= b.len() {
            break;
        }
        if b[close].is_ascii_alphanumeric() {
            best = close + 1;
        }
        if !(b[close].is_ascii_alphanumeric() || b[close] == b'-') {
            break;
        }
        t += 1;
    }
    Some(best)
}

/// `emailDomainRegexp.FindSubmatchIndex(b)[1]`, hand-matched; see [`email_domain_label`].
fn email_domain_match(b: &[u8]) -> Option<usize> {
    let mut end = email_domain_label(b, 0)?;
    while end < b.len() && b[end] == b'.' {
        match email_domain_label(b, end + 1) {
            Some(e) => end = e,
            None => break,
        }
    }
    Some(end)
}

/// Port of `util.FindEmailIndex`.
pub fn find_email_index(b: &[u8]) -> isize {
    let mut i = 0;
    while i < b.len() {
        if EMAIL_TABLE[b[i] as usize] & 1 != 1 {
            break;
        }
        i += 1;
    }
    if i == 0 {
        return -1;
    }
    if i >= b.len() || b[i] != b'@' {
        return -1;
    }
    i += 1;
    if i >= b.len() {
        return -1;
    }
    match email_domain_match(&b[i..]) {
        Some(m) => (i + m) as isize,
        None => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_references_follow_go_parse_uint() {
        assert_eq!(resolve_numeric_references(b"&#65;&#x42;"), b"AB");
        // Base 0: a leading zero is octal, an invalid octal digit is a syntax error (zero).
        assert_eq!(resolve_numeric_references(b"&#0123;"), b"S");
        assert_eq!(resolve_numeric_references(b"&#08;"), "\u{fffd}".as_bytes());
        assert_eq!(resolve_numeric_references(b"&#0;"), "\u{fffd}".as_bytes());
        // Hex has no length cap here (the renderer's writer has one); overflow is U+FFFD.
        assert_eq!(
            resolve_numeric_references(b"&#x1234567890;"),
            "\u{fffd}".as_bytes()
        );
        assert_eq!(resolve_numeric_references(b"&#12345678;"), b"&#12345678;");
        assert_eq!(resolve_numeric_references(b"&#;&#x;&"), b"&#;&#x;&");
    }

    #[test]
    fn entity_names() {
        assert_eq!(
            resolve_entity_names(b"&amp;&ouml;&bogus;&#65;"),
            "&\u{f6}&bogus;&#65;".as_bytes()
        );
        assert_eq!(lookup_html5_entity(b"AElig"), Some("\u{c6}".as_bytes()));
        assert_eq!(lookup_html5_entity(b"amp;"), None);
    }

    #[test]
    fn url_escape_quirks() {
        assert_eq!(url_escape(b"a b", false), b"a%20b");
        assert_eq!(url_escape(b"%41%4g%4", false), b"%41%4g%254");
        assert_eq!(url_escape("\u{e9}".as_bytes(), false), b"%C3%A9");
        assert_eq!(url_escape(b"[x]\"<>`", false), b"%5Bx%5D%22%3C%3E%60");
        // A lone lead byte is kept when nothing else was escaped (copy-on-write) …
        assert_eq!(url_escape(b"\xc3", false), b"\xc3");
        // … and dropped once the buffer has been copied.
        assert_eq!(url_escape(b" \xc3", false), b"%20");
        // Continuation bytes are copied through raw.
        assert_eq!(url_escape(b"a\x80", false), b"a\x80");
        assert_eq!(url_escape(b"\\*&amp;&#65;", true), b"*&A");
    }

    #[test]
    fn email_and_url_indexes() {
        assert_eq!(find_email_index(b"a@b.c>"), 5);
        assert_eq!(find_email_index(b"a@-b"), -1);
        assert_eq!(find_email_index(b"@b"), -1);
        assert_eq!(find_email_index(b"a@b-.c"), 3);
        assert_eq!(find_email_index(b"a@b.-c"), 3);
        let long = [b"a@".as_slice(), &[b'x'; 70], b".y"].concat();
        assert_eq!(find_email_index(&long), 65);
        assert_eq!(find_url_index(b"http://x y"), 8);
        assert_eq!(find_url_index(b"h:x"), -1);
        assert_eq!(find_url_index(b"a1+.-:"), 6);
        assert_eq!(find_url_index(b"1a:x"), -1);
    }

    #[test]
    fn link_reference_folding() {
        assert_eq!(to_link_reference(b"  Foo \t Bar  "), b"foo bar");
        assert_eq!(
            to_link_reference("STRASSE \u{1E9E}".as_bytes()),
            "strasse ss".as_bytes()
        );
        assert_eq!(to_link_reference("\u{17f}".as_bytes()), b"s");
    }

    #[test]
    fn indentation_helpers() {
        assert_eq!(indent_width(b"  \tx", 0), (4, 3));
        assert_eq!(indent_width(b"\tx", 2), (2, 1));
        assert_eq!(indent_position(b"\tx", 0, 2), (1, 2));
        assert_eq!(indent_position(b" x", 0, 2), (-1, -1));
        assert_eq!(indent_position_padding(b"  x", 0, 1, 2), (1, 0));
        assert_eq!(first_non_space_position(b" \t\n"), -1);
        assert_eq!(first_non_space_position(b" a"), 1);
    }

    #[test]
    fn rune_classes() {
        assert!(is_punct_rune('\u{2014}'));
        assert!(is_punct_rune('$'));
        assert!(!is_punct_rune('a'));
        assert!(is_space_rune('\u{3000}'));
        assert!(!is_space_rune('\u{200b}'));
        assert_eq!(decode_rune(b"\xe2\x82x"), ('\u{fffd}', 1));
        assert_eq!(decode_rune(b""), ('\u{fffd}', 0));
        assert_eq!(to_rune("a\u{e9}".as_bytes(), 2), '\u{e9}');
    }
}
