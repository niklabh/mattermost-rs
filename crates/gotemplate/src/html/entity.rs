//! Port of Go's `html.UnescapeString` (html/escape.go), which `html/template` applies to an
//! attribute value before running its context transitions over it.

use super::entity_table::ENTITIES;

/// `longestEntityWithoutSemicolon` (entity.go:10).
const LONGEST_ENTITY_WITHOUT_SEMICOLON: usize = 6;

/// `replacementTable` (escape.go:14): what `&#128;`..`&#159;` decode to.
const REPLACEMENT_TABLE: [u32; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

fn lookup(name: &[u8]) -> Option<&'static str> {
    let name = std::str::from_utf8(name).ok()?;
    ENTITIES
        .binary_search_by(|(n, _)| n.cmp(&name))
        .ok()
        .map(|i| ENTITIES[i].1)
}

fn push_rune(out: &mut Vec<u8>, r: u32) {
    let c = char::from_u32(r).unwrap_or('\u{fffd}');
    let mut tmp = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
}

/// `unescapeEntity` (escape.go:56): decodes the reference at the start of `s` (which begins with
/// `&`) onto `out`, returning the bytes consumed.
fn unescape_entity(s: &[u8], out: &mut Vec<u8>) -> usize {
    let mut i = 1;
    if s.len() <= 1 {
        out.push(s[0]);
        return 1;
    }
    if s[i] == b'#' {
        if s.len() <= 3 {
            out.push(s[0]);
            return 1;
        }
        i += 1;
        let mut c = s[i];
        let mut hex = false;
        if c == b'x' || c == b'X' {
            hex = true;
            i += 1;
        }
        let mut x: u32 = 0;
        while i < s.len() {
            c = s[i];
            i += 1;
            if hex {
                if c.is_ascii_digit() {
                    x = x.wrapping_mul(16).wrapping_add(u32::from(c - b'0'));
                    continue;
                } else if (b'a'..=b'f').contains(&c) {
                    x = x.wrapping_mul(16).wrapping_add(u32::from(c - b'a') + 10);
                    continue;
                } else if (b'A'..=b'F').contains(&c) {
                    x = x.wrapping_mul(16).wrapping_add(u32::from(c - b'A') + 10);
                    continue;
                }
            } else if c.is_ascii_digit() {
                x = x.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
                continue;
            }
            if c != b';' {
                i -= 1;
            }
            break;
        }
        if i <= 3 {
            out.push(s[0]);
            return 1;
        }
        if (0x80..=0x9f).contains(&x) {
            x = REPLACEMENT_TABLE[(x - 0x80) as usize];
        } else if x == 0 || (0xd800..=0xdfff).contains(&x) || x > 0x10ffff {
            x = 0xfffd;
        }
        push_rune(out, x);
        return i;
    }
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c.is_ascii_alphanumeric() {
            continue;
        }
        if c != b';' {
            i -= 1;
        }
        break;
    }
    let name = &s[1..i];
    if !name.is_empty() {
        if let Some(x) = lookup(name) {
            out.extend_from_slice(x.as_bytes());
            return i;
        }
        let max_len = (name.len() - 1).min(LONGEST_ENTITY_WITHOUT_SEMICOLON);
        for j in (2..=max_len).rev() {
            if let Some(x) = lookup(&name[..j]) {
                out.extend_from_slice(x.as_bytes());
                return j + 1;
            }
        }
    }
    out.extend_from_slice(&s[..i]);
    i
}

/// `html.UnescapeString`.
pub(crate) fn unescape_string(s: &[u8]) -> Vec<u8> {
    let Some(first) = s.iter().position(|&b| b == b'&') else {
        return s.to_vec();
    };
    let mut out = Vec::with_capacity(s.len());
    out.extend_from_slice(&s[..first]);
    let mut src = first;
    while src < s.len() {
        if s[src] == b'&' {
            src += unescape_entity(&s[src..], &mut out);
        } else {
            let next = s[src..]
                .iter()
                .position(|&b| b == b'&')
                .map_or(s.len(), |i| src + i);
            out.extend_from_slice(&s[src..next]);
            src = next;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> String {
        String::from_utf8(unescape_string(s.as_bytes())).unwrap()
    }

    #[test]
    fn named_numeric_and_prefix() {
        assert_eq!(u("a &amp; b"), "a & b");
        assert_eq!(u("&quot;x&quot;"), "\"x\"");
        assert_eq!(u("&#x6a;&#106;&#128;&#0;"), "jj\u{20ac}\u{fffd}");
        assert_eq!(u("&ampx"), "&x");
        assert_eq!(u("&notit;"), "\u{ac}it;");
        // `&#x;` consumes its `;` and so is not "no characters matched": it decodes x = 0.
        assert_eq!(u("&bogus; & &#; &#x;"), "&bogus; & &#; \u{fffd}");
        assert_eq!(u("&NotEqualTilde;"), "\u{2242}\u{338}");
    }
}
