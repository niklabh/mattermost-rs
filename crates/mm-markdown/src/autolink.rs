//! Port of `autolink.go`: the bare `scheme://` and `www.` autolinks, based on cmark-gfm's
//! `extensions/autolink.c`. There is no `<...>` autolink in this package; `<` is text.
//!
//! Everything here is byte-indexed except [`trim_trailing_characters_from_link`], which Go
//! runs over `[]rune` — and `check_domain`, which walks bytes but decodes a rune at each one,
//! so a multi-byte host character is "valid" at its lead byte and "invalid" at its continuation
//! bytes. The result is the same either way because the link is then extended to the next
//! ASCII whitespace; the corpus pins that with `http://例え.jp`.

use crate::markdown::{Range, is_alphanumeric_byte, is_whitespace_byte};
use crate::unicode::{equal_fold, is_punct, is_space};

/// Port of `DefaultURLSchemes` (autolink.go:16). The client's custom schemes are not here.
pub const DEFAULT_URL_SCHEMES: &[&str] = &["http", "https", "ftp", "mailto", "tel"];

/// Port of `parseWWWAutolink` (autolink.go:23): a `www.` link starting at `position`, which the
/// caller has already found to be a `w`. Note the `position > 1` guard — the byte before
/// position 1 is never checked, so `awww.a.b` at position 1 links where at position 2 it would
/// not.
pub(crate) fn parse_www_autolink(data: &str, position: usize) -> Option<Range> {
    let bytes = data.as_bytes();
    // Check that this isn't part of another word
    if position > 1 {
        let prev = bytes[position - 1];
        if !is_whitespace_byte(prev) && !is_allowed_before_www_link(prev) {
            return None;
        }
    }

    // Check that this starts with www: `^www\d{0,3}\.`
    if bytes.len() - position < 4 || !matches_www_regex(&bytes[position..]) {
        return None;
    }

    let mut end = check_domain(&data[position..], false);
    if end == 0 {
        return None;
    }
    end += position;

    // Grab all text until the end of the string or the next whitespace character
    while end < bytes.len() && !is_whitespace_byte(bytes[end]) {
        end += 1;
    }

    // Trim trailing punctuation
    end = trim_trailing_characters_from_link(data, position, end);
    if position == end {
        return None;
    }
    Some(Range::new(position, end))
}

/// `wwwAutoLinkRegex`, `^www\d{0,3}\.`, by hand: RE2's `\d` is ASCII, and since a digit is never
/// a `.` the greedy quantifier has no alternative to backtrack to.
fn matches_www_regex(s: &[u8]) -> bool {
    if !s.starts_with(b"www") {
        return false;
    }
    let mut i = 3;
    let mut digits = 0;
    while i < s.len() && digits < 3 && s[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    i < s.len() && s[i] == b'.'
}

/// Port of `isAllowedBeforeWWWLink` (autolink.go:58).
fn is_allowed_before_www_link(c: u8) -> bool {
    matches!(c, b'*' | b'_' | b'~' | b')' | b'<' | b'(' | b'>')
}

/// Port of `parseURLAutolink` (autolink.go:69): a `scheme://` link whose `:` is at `position`.
/// The scheme is the run of ASCII alphanumerics before the colon **plus the byte immediately
/// before the colon whatever it is** (Go starts at `position-1` and only checks `start-1`), so
/// `x ://a.b` fails on scheme `"x "` rather than on the space. The returned range starts at the
/// scheme; the caller rewinds the text node it has already emitted.
pub(crate) fn parse_url_autolink(data: &str, position: usize) -> Option<Range> {
    let bytes = data.as_bytes();
    // Check that a :// exists. This doesn't match the clients that treat the slashes as optional.
    if bytes.len() - position < 4 || bytes[position + 1] != b'/' || bytes[position + 2] != b'/' {
        return None;
    }
    // Go: `start := position - 1`, and `start < 0` fails — so a colon at 0 is never a link.
    if position == 0 {
        return None;
    }
    let mut start = position - 1;
    while start > 0 && is_alphanumeric_byte(bytes[start - 1]) {
        start -= 1;
    }

    // Ensure that the URL scheme is allowed and that at least one character after the scheme is valid.
    let scheme = &data[start..position];
    if !is_scheme_allowed(scheme) || !is_valid_host_character_at(data, position + 3) {
        return None;
    }

    // Go adds `position`, not `position + 3`, so this lands inside the `//`; the whitespace
    // extension below makes that harmless, but it is why the domain length is not the link
    // length.
    let mut end = check_domain(&data[position + 3..], true);
    if end == 0 {
        return None;
    }
    end += position;

    // Grab all text until the end of the string or the next whitespace character
    while end < bytes.len() && !is_whitespace_byte(bytes[end]) {
        end += 1;
    }

    // Trim trailing punctuation
    end = trim_trailing_characters_from_link(data, start, end);
    if start == end {
        return None;
    }
    Some(Range::new(start, end))
}

/// Port of `isSchemeAllowed` (autolink.go:110): `strings.EqualFold` against the default list.
fn is_scheme_allowed(scheme: &str) -> bool {
    DEFAULT_URL_SCHEMES
        .iter()
        .any(|allowed| equal_fold(allowed, scheme))
}

/// Port of `checkDomain` (autolink.go:126): the number of bytes of `data` that make up the
/// domain, or 0 when it is not one. Starts at byte 1 and stops before the **last** byte
/// (`i < len(data)-1`), so a one- or two-byte input is accepted unexamined. An underscore
/// anywhere in the scanned prefix rejects the whole domain; without `allow_short` a period is
/// required.
fn check_domain(data: &str, allow_short: bool) -> usize {
    let bytes = data.as_bytes();
    let mut found_underscore = false;
    let mut found_period = false;

    let mut i = 1;
    while i < bytes.len().saturating_sub(1) {
        if bytes[i] == b'_' {
            found_underscore = true;
            break;
        } else if bytes[i] == b'.' {
            found_period = true;
        } else if !is_valid_host_character_at(data, i) && bytes[i] != b'-' {
            break;
        }
        i += 1;
    }

    if found_underscore {
        return 0;
    }
    if allow_short {
        // If allowShort is set, accept any string of valid domain characters
        return i;
    }
    // Otherwise a valid domain just requires at least a single period.
    if found_period { i } else { 0 }
}

/// Port of `isValidHostCharacter` (autolink.go:158) applied at byte offset `i`: decodes the rune
/// there and requires it to be neither space nor punctuation. Go's `DecodeRuneInString` yields
/// `RuneError` for an empty string, for a continuation byte, and for a literal U+FFFD — all three
/// are "not valid" here.
fn is_valid_host_character_at(data: &str, i: usize) -> bool {
    match data.get(i..).and_then(|s| s.chars().next()) {
        Some('\u{FFFD}') | None => false,
        Some(c) => !is_space(c) && !is_punct(c),
    }
}

/// Port of `trimTrailingCharactersFromLink` (autolink.go:169), cmark-gfm's `autolink_delim`.
/// Works in runes: cuts at the first `<` or `>`, then strips trailing characters that cannot end
/// a link, a trailing `&name;` entity (or a lone `;`), and unbalanced `)` — one at a time, from
/// the end, until something that can end a link is reached. Returns the new byte end.
pub(crate) fn trim_trailing_characters_from_link(
    markdown: &str,
    start: usize,
    end: usize,
) -> usize {
    let runes: Vec<char> = markdown.get(start..end).unwrap_or("").chars().collect();
    let mut link_end = runes.len();

    // Cut off the link before an angle bracket if it contains one
    if let Some(i) = runes.iter().position(|&c| c == '<' || c == '>') {
        link_end = i;
    }

    let mut num_closing = 0usize;
    let mut num_opening = 0usize;
    for &c in &runes[..link_end] {
        if c == '(' {
            num_opening += 1;
        } else if c == ')' {
            num_closing += 1;
        }
    }

    let trim_link_end =
        |link_end: &mut usize, num_opening: &mut usize, num_closing: &mut usize, new_end: usize| {
            for &c in &runes[new_end..*link_end] {
                if c == '(' {
                    *num_opening -= 1;
                } else if c == ')' {
                    *num_closing -= 1;
                }
            }
            *link_end = new_end;
        };

    while link_end > 0 {
        let c = runes[link_end - 1];

        if !can_end_autolink(c) {
            // Trim trailing quotes, periods, etc
            let new_end = link_end - 1;
            trim_link_end(&mut link_end, &mut num_opening, &mut num_closing, new_end);
        } else if c == ';' {
            // Trim a trailing HTML entity. Go's `newEnd := linkEnd - 2` can be -1 when the `;`
            // is the first rune; the loop then does not run and `newEnd < linkEnd-2` is false, so
            // the semicolon alone is trimmed. Signed arithmetic keeps that branch honest.
            let mut new_end = link_end as isize - 2;
            while new_end > 0 && runes[new_end as usize].is_ascii_alphabetic() {
                new_end -= 1;
            }
            if new_end < link_end as isize - 2 && new_end >= 0 && runes[new_end as usize] == '&' {
                trim_link_end(
                    &mut link_end,
                    &mut num_opening,
                    &mut num_closing,
                    new_end as usize,
                );
            } else {
                // This isn't actually an HTML entity, so just trim the semicolon
                let new_end = link_end - 1;
                trim_link_end(&mut link_end, &mut num_opening, &mut num_closing, new_end);
            }
        } else if c == ')' {
            // Only allow an autolink ending with a bracket if that bracket is part of a matching
            // pair. If there are more closing brackets than opening ones, remove the extra one.
            if num_closing <= num_opening {
                break;
            }
            let new_end = link_end - 1;
            trim_link_end(&mut link_end, &mut num_opening, &mut num_closing, new_end);
        } else {
            // There's no special characters at the end of the link, so we're at the end
            break;
        }
    }

    start
        + runes[..link_end]
            .iter()
            .map(|c| c.len_utf8())
            .sum::<usize>()
}

/// Port of `canEndAutolink` (autolink.go:255).
fn can_end_autolink(c: char) -> bool {
    !matches!(
        c,
        '?' | '!' | '.' | ',' | ':' | '*' | '_' | '~' | '\'' | '"'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn www_regex_is_three_digits_at_most() {
        assert!(matches_www_regex(b"www."));
        assert!(matches_www_regex(b"www1."));
        assert!(matches_www_regex(b"www123."));
        assert!(!matches_www_regex(b"www1234."));
        assert!(!matches_www_regex(b"www"));
        assert!(!matches_www_regex(b"wwww."));
        assert!(!matches_www_regex(b"WWW."));
    }

    #[test]
    fn domain_scan_stops_before_the_last_byte() {
        assert_eq!(check_domain("a.b", false), 2);
        assert_eq!(check_domain("ab", false), 0);
        assert_eq!(check_domain("ab", true), 1);
        assert_eq!(check_domain("a", true), 1);
        assert_eq!(check_domain("", true), 1);
        assert_eq!(check_domain("a_b.c", false), 0);
        assert_eq!(check_domain("a-b.c", false), 4);
        assert_eq!(check_domain("a.b/c", false), 3);
        assert_eq!(check_domain("aé.b", true), 2);
        assert_eq!(check_domain("a\u{FFFD}b", true), 1);
    }

    #[test]
    fn trailing_trim_rules() {
        let t = |s: &str| trim_trailing_characters_from_link(s, 0, s.len());
        assert_eq!(t("http://a.b."), 10);
        assert_eq!(t("http://a.b.,!?:*_~'\""), 10);
        assert_eq!(t("http://a.b;"), 10);
        assert_eq!(t("http://a.b&amp;"), 10);
        assert_eq!(t("http://a.b&amp"), 14);
        assert_eq!(t("http://a.b&;"), 11);
        assert_eq!(t("http://a.b/x)"), 12);
        assert_eq!(t("http://a.b/(x)"), 14);
        assert_eq!(t("http://a.b/(x))"), 14);
        assert_eq!(t("http://a.b/((x))"), 16);
        assert_eq!(t("http://a.b/<x>"), 11);
        assert_eq!(t("http://a.b/x>y"), 12);
        assert_eq!(t("http://a.b/日本。"), 20);
        assert_eq!(t("http://a.b/日本."), 17);
        assert_eq!(t(";"), 0);
        assert_eq!(t("a;"), 1);
        assert_eq!(t("&a;"), 0);
        assert_eq!(t(""), 0);
        assert_eq!(t("..."), 0);
    }

    #[test]
    fn url_autolink_scheme_rules() {
        assert_eq!(parse_url_autolink("http://a.b", 4), Some(Range::new(0, 10)));
        assert_eq!(parse_url_autolink("HTTP://a.b", 4), Some(Range::new(0, 10)));
        assert_eq!(parse_url_autolink("xhttp://a.b", 5), None);
        assert_eq!(
            parse_url_autolink(" http://a.b", 5),
            Some(Range::new(1, 11))
        );
        assert_eq!(parse_url_autolink("http:/a.b", 4), None);
        assert_eq!(parse_url_autolink("http://", 4), None);
        assert_eq!(parse_url_autolink("http:// a", 4), None);
        assert_eq!(parse_url_autolink("://a.b", 0), None);
        assert_eq!(parse_url_autolink("x ://a.b", 2), None);
        assert_eq!(parse_url_autolink("http://a_b.c", 4), None);
        assert_eq!(parse_url_autolink("http://...", 4), None);
    }

    #[test]
    fn www_autolink_rules() {
        assert_eq!(parse_www_autolink("www.a.b", 0), Some(Range::new(0, 7)));
        assert_eq!(parse_www_autolink("awww.a.b", 1), Some(Range::new(1, 8)));
        assert_eq!(parse_www_autolink("xawww.a.b", 2), None);
        assert_eq!(parse_www_autolink("(www.a.b)", 1), Some(Range::new(1, 8)));
        assert_eq!(parse_www_autolink("www.", 0), None);
        assert_eq!(parse_www_autolink("www", 0), None);
        assert_eq!(parse_www_autolink("www.x", 0), Some(Range::new(0, 5)));
        // Trimmed back to `www`, which is still a non-empty link.
        assert_eq!(parse_www_autolink("www.:", 0), Some(Range::new(0, 3)));
    }
}
