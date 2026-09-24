//! The handful of Go `strings` functions the tree builder calls, with Go's semantics rather than
//! Rust's where the two differ.

/// `whitespace` (parse.go:504): the five ASCII characters HTML calls whitespace.
pub(crate) const WHITESPACE: &[char] = &[' ', '\t', '\r', '\n', '\u{0c}'];

/// `strings.TrimLeft(s, whitespace)`.
pub(crate) fn trim_left_ws(s: &str) -> &str {
    s.trim_start_matches(WHITESPACE)
}

/// `strings.ToLower`: `unicode.ToLower` rune by rune — the **simple** case mapping. Rust's
/// `char::to_lowercase` is the full mapping, which differs for exactly one code point:
/// U+0130 (`İ`) lowers to `i` in Go and to `i̇` (two code points) in Rust.
pub(crate) fn to_lower(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\u{130}' {
                return 'i';
            }
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(x), None) => x,
                _ => c,
            }
        })
        .collect()
}

/// A representative of `c`'s `unicode.SimpleFold` orbit: two runes are in one orbit iff their
/// representatives are equal. Upper-casing then lower-casing (each only where the mapping is a
/// single code point, as Go's simple mappings are) lands every orbit on one rune; `ı` and `İ`
/// are the two runes whose case mappings leave their Go orbit (each is alone in it).
fn fold_class(c: char) -> char {
    if matches!(c, '\u{130}' | '\u{131}') {
        return c;
    }
    fn single(mut it: impl Iterator<Item = char>, c: char) -> char {
        match (it.next(), it.next()) {
            (Some(x), None) => x,
            _ => c,
        }
    }
    let u = single(c.to_uppercase(), c);
    single(u.to_lowercase(), u)
}

/// `strings.EqualFold`: equality under Unicode simple case folding.
pub(crate) fn equal_fold(s: &str, t: &str) -> bool {
    let mut a = s.chars();
    let mut b = t.chars();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                if x != y && fold_class(x) != fold_class(y) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_lower_is_the_simple_mapping() {
        assert_eq!(to_lower("HTML"), "html");
        assert_eq!(to_lower("İ"), "i");
        assert_eq!(to_lower("ΣΑΣ"), "σασ");
        assert_eq!(to_lower("K"), "k");
    }

    #[test]
    fn equal_fold_follows_go_orbits() {
        assert!(equal_fold("Hidden", "hIDDEN"));
        assert!(equal_fold("ſ", "S"));
        assert!(equal_fold("K", "k"));
        assert!(equal_fold("ς", "Σ"));
        assert!(equal_fold("ß", "ẞ"));
        assert!(!equal_fold("ı", "i"));
        assert!(!equal_fold("ı", "I"));
        assert!(!equal_fold("İ", "i"));
        assert!(!equal_fold("ab", "a"));
        assert!(!equal_fold("a", "b"));
    }

    #[test]
    fn trim_left_ws_is_ascii_only() {
        assert_eq!(trim_left_ws(" \t\r\n\u{c}x "), "x ");
        assert_eq!(trim_left_ws("\u{a0}x"), "\u{a0}x");
        assert_eq!(trim_left_ws("\u{b}x"), "\u{b}x");
    }
}
