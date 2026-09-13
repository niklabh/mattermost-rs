//! The three pieces of Go's `unicode`/`strings` packages `shared/markdown` leans on, over the
//! tables `go_unicode_generated.rs` carries from the Go toolchain: `unicode.IsPunct`,
//! `unicode.IsSpace` (both used by `isValidHostCharacter`, autolink.go:161) and
//! `strings.EqualFold` (the reference-label comparison, inlines.go:334).

use crate::go_unicode_generated::{IS_PUNCT_RANGES, IS_SPACE_RANGES, SIMPLE_FOLD};

fn in_ranges(table: &[(u32, u32)], c: char) -> bool {
    let c = c as u32;
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `unicode.IsPunct`: general category `P`, per the Go toolchain's tables.
pub(crate) fn is_punct(c: char) -> bool {
    in_ranges(IS_PUNCT_RANGES, c)
}

/// `unicode.IsSpace`, per the Go toolchain's tables. Agrees with `char::is_whitespace` today;
/// the table exists so that stays a fact rather than an assumption.
pub(crate) fn is_space(c: char) -> bool {
    in_ranges(IS_SPACE_RANGES, c)
}

/// `unicode.SimpleFold(r)`: the next code point in `r`'s case orbit, or `r` itself when the
/// orbit is a singleton.
pub(crate) fn simple_fold(r: u32) -> u32 {
    match SIMPLE_FOLD.binary_search_by_key(&r, |&(from, _)| from) {
        Ok(i) => SIMPLE_FOLD[i].1,
        Err(_) => r,
    }
}

/// Port of `strings.EqualFold` (strings.go:1187): Unicode simple case folding, which is neither
/// `eq_ignore_ascii_case` nor `to_lowercase` equality — `K` (U+212A) equals `k`, `ſ` equals `s`,
/// `ß` equals `ẞ` but not `SS`.
pub(crate) fn equal_fold(s: &str, t: &str) -> bool {
    let sb = s.as_bytes();
    let tb = t.as_bytes();
    // ASCII fast path.
    let mut i = 0;
    let n = sb.len().min(tb.len());
    while i < n {
        let (mut sr, mut tr) = (sb[i], tb[i]);
        if sr | tr >= 0x80 {
            break;
        }
        i += 1;
        if tr == sr {
            continue;
        }
        if tr < sr {
            std::mem::swap(&mut sr, &mut tr);
        }
        if sr.is_ascii_uppercase() && tr == sr + (b'a' - b'A') {
            continue;
        }
        return false;
    }
    if i == n {
        return sb.len() == tb.len();
    }

    // `i` is a char boundary in both: every byte before it was ASCII.
    let mut t_chars = t[i..].chars();
    for sr in s[i..].chars() {
        let Some(tr) = t_chars.next() else {
            return false;
        };
        let (mut sr, mut tr) = (sr as u32, tr as u32);
        if tr == sr {
            continue;
        }
        if tr < sr {
            std::mem::swap(&mut sr, &mut tr);
        }
        if tr < 0x80 {
            if (b'A' as u32..=b'Z' as u32).contains(&sr) && tr == sr + (b'a' - b'A') as u32 {
                continue;
            }
            return false;
        }
        // General case: SimpleFold(x) returns the next equivalent rune > x or wraps around.
        let mut r = simple_fold(sr);
        while r != sr && r < tr {
            r = simple_fold(r);
        }
        if r == tr {
            continue;
        }
        return false;
    }
    t_chars.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punct_and_space_agree_with_std_on_ascii() {
        for b in 0u8..0x80 {
            let c = b as char;
            assert_eq!(
                is_punct(c),
                c.is_ascii_punctuation() && !"$+<=>^`|~".contains(c),
                "{c:?}"
            );
            assert_eq!(
                is_space(c),
                matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' '),
                "{c:?}"
            );
        }
        assert!(is_space('\u{85}'));
        assert!(is_space('\u{a0}'));
        assert!(!is_space('\u{200b}'));
        assert!(is_punct('。'));
        assert!(!is_punct('$'));
    }

    #[test]
    fn fold_orbits() {
        assert_eq!(simple_fold('A' as u32), 'a' as u32);
        assert_eq!(simple_fold('a' as u32), 'A' as u32);
        assert_eq!(simple_fold('k' as u32), 0x212A);
        assert_eq!(simple_fold(0x212A), 'K' as u32);
        assert_eq!(simple_fold('1' as u32), '1' as u32);
        assert!(equal_fold("Go", "GO"));
        assert!(equal_fold("k", "\u{212A}"));
        assert!(equal_fold("\u{212A}", "K"));
        assert!(equal_fold("ſ", "S"));
        assert!(equal_fold("σ", "ς"));
        assert!(equal_fold("ß", "ẞ"));
        assert!(!equal_fold("ß", "SS"));
        assert!(!equal_fold("a", "ab"));
        assert!(!equal_fold("ab", "a"));
        assert!(!equal_fold("aé", "a"));
        assert!(!equal_fold("a", "aé"));
        assert!(equal_fold("", ""));
        assert!(!equal_fold("abc", "abd"));
        assert!(!equal_fold("a\u{a0}", "A "));
    }
}
