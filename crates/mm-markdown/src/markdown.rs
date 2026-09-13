//! Port of `markdown.go`: the byte-classification helpers, `Range`, and the range arithmetic the
//! block and inline parsers share.
//!
//! Go indexes strings by byte and these helpers are called on bytes almost everywhere; the rune
//! forms exist for the few `range`-over-string call sites. Both are kept so each call site can
//! cite the form Go uses.

/// Port of `markdown.Range` (blocks.go:37): a half-open byte range into the source markdown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub position: usize,
    pub end: usize,
}

impl Range {
    pub(crate) const fn new(position: usize, end: usize) -> Self {
        Range { position, end }
    }

    /// The slice of `markdown` this range covers.
    ///
    /// Every range the parser builds lies on char boundaries (they are cut at ASCII bytes), but
    /// this never indexes past the string, so a malformed range yields `""` rather than a panic.
    pub(crate) fn slice<'a>(&self, markdown: &'a str) -> &'a str {
        markdown.get(self.position..self.end).unwrap_or("")
    }
}

/// Port of `isEscapable` (markdown.go:14): ASCII punctuation, and nothing else.
pub(crate) fn is_escapable(c: char) -> bool {
    c > ' '
        && (c < '0' || (c > '9' && (c < 'A' || (c > 'Z' && (c < 'a' || (c > 'z' && c <= '~'))))))
}

/// Port of `isEscapableByte` (markdown.go:18). Go widens the byte to a rune, so a byte above
/// 0x7F becomes U+0080..U+00FF, which the predicate rejects; `as char` does the same.
pub(crate) fn is_escapable_byte(c: u8) -> bool {
    is_escapable(c as char)
}

/// Port of `isWhitespace` (markdown.go:22): the six ASCII whitespace characters. Not
/// `char::is_whitespace`, which is the Unicode property — `strings.TrimSpace` uses that one, and
/// the two are deliberately different predicates in this package.
pub(crate) fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r')
}

/// Port of `isWhitespaceByte` (markdown.go:30).
pub(crate) fn is_whitespace_byte(c: u8) -> bool {
    is_whitespace(c as char)
}

/// Port of `isNumeric` (markdown.go:34).
pub(crate) fn is_numeric(c: char) -> bool {
    c.is_ascii_digit()
}

/// Port of `isNumericByte` (markdown.go:38).
pub(crate) fn is_numeric_byte(c: u8) -> bool {
    is_numeric(c as char)
}

/// Port of `isHexByte` (markdown.go:46).
pub(crate) fn is_hex_byte(c: u8) -> bool {
    (c as char).is_ascii_hexdigit()
}

/// Port of `isAlphanumeric` (markdown.go:50).
pub(crate) fn is_alphanumeric(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// Port of `isAlphanumericByte` (markdown.go:54).
pub(crate) fn is_alphanumeric_byte(c: u8) -> bool {
    is_alphanumeric(c as char)
}

/// Port of `isWordByte` (markdown.go:63): the `\w` class of RE2, which is ASCII.
pub(crate) fn is_word_byte(c: u8) -> bool {
    is_alphanumeric_byte(c) || c == b'_'
}

/// Port of `nextNonWhitespace` (markdown.go:67).
pub(crate) fn next_non_whitespace(markdown: &str, position: usize) -> usize {
    let bytes = markdown.as_bytes();
    let mut i = position;
    while i < bytes.len() {
        if !is_whitespace_byte(bytes[i]) {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

/// Port of `nextLine` (markdown.go:76): the byte position after the next line ending (`\r\n`,
/// `\r` or `\n`), or the end of the string, and whether any non-whitespace byte was skipped on
/// the way.
pub(crate) fn next_line(markdown: &str, position: usize) -> (usize, bool) {
    let bytes = markdown.as_bytes();
    let mut skipped_non_whitespace = false;
    let mut i = position;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\r' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                return (i + 2, skipped_non_whitespace);
            }
            return (i + 1, skipped_non_whitespace);
        } else if c == b'\n' {
            return (i + 1, skipped_non_whitespace);
        } else if !is_whitespace_byte(c) {
            skipped_non_whitespace = true;
        }
        i += 1;
    }
    (bytes.len(), skipped_non_whitespace)
}

/// Port of `countIndentation` (markdown.go:93): `(columns, bytes)` of the leading spaces and
/// tabs of the range. A tab is always four columns — not a tab stop — which is Go's choice.
pub(crate) fn count_indentation(markdown: &str, r: Range) -> (usize, usize) {
    let bytes = markdown.as_bytes();
    let mut spaces = 0;
    let mut count = 0;
    let mut i = r.position;
    while i < r.end && i < bytes.len() {
        match bytes[i] {
            b' ' => {
                spaces += 1;
                count += 1;
            }
            b'\t' => {
                spaces += 4;
                count += 1;
            }
            _ => break,
        }
        i += 1;
    }
    (spaces, count)
}

/// Port of `trimLeftSpace` (markdown.go:107) **including its bug**: Go computes the number of
/// leading whitespace bytes and subtracts it from `End` instead of adding it to `Position`, so
/// the range keeps its leading whitespace and loses the same number of bytes from its tail.
/// Reachable through `Paragraph.Close` when a paragraph's first line begins with `\f` or `\v`
/// (the two whitespace bytes line indentation does not strip); the corpus pins it.
pub(crate) fn trim_left_space(markdown: &str, r: Range) -> Range {
    let s = r.slice(markdown);
    let trimmed = s.trim_start_matches(is_whitespace);
    Range::new(r.position, r.end - (s.len() - trimmed.len()))
}

/// Port of `trimRightSpace` (markdown.go:113).
pub(crate) fn trim_right_space(markdown: &str, r: Range) -> Range {
    let s = r.slice(markdown);
    let trimmed = s.trim_end_matches(is_whitespace);
    Range::new(r.position, r.end - (s.len() - trimmed.len()))
}

/// Port of `relativeToAbsolutePosition` (markdown.go:119): maps an offset into the concatenation
/// of `ranges` back to a position in the source. An offset past the end maps to the last range's
/// end; no ranges at all maps to 0.
pub(crate) fn relative_to_absolute_position(ranges: &[Range], position: usize) -> usize {
    let mut rem = position;
    for r in ranges {
        let l = r.end - r.position;
        if rem < l {
            return r.position + rem;
        }
        rem -= l;
    }
    match ranges.last() {
        Some(r) => r.end,
        None => 0,
    }
}

/// Port of `trimBytesFromRanges` (markdown.go:134): drops `bytes` bytes from the front of the
/// concatenation. A range consumed exactly is dropped, never left empty.
pub(crate) fn trim_bytes_from_ranges(ranges: &[Range], bytes: usize) -> Vec<Range> {
    let mut result = Vec::new();
    let mut rem = bytes;
    for r in ranges {
        if rem == 0 {
            result.push(*r);
            continue;
        }
        let l = r.end - r.position;
        if rem < l {
            result.push(Range::new(r.position + rem, r.end));
            rem = 0;
            continue;
        }
        rem -= l;
    }
    result
}

/// A slice `raw[start..end]` of a paragraph's concatenated text as a `Cow` over the *source*:
/// borrowed when the same bytes sit contiguously in `markdown` at the mapped position (a text
/// run never crosses a line ending, so this is the common case), owned when they do not (a link
/// title or a reference label spanning lines). Go copies into `p.raw` and slices that; the
/// result is the same string either way.
pub(crate) fn cow_from_raw<'a>(
    markdown: &'a str,
    ranges: &[Range],
    raw: &str,
    start: usize,
    end: usize,
) -> std::borrow::Cow<'a, str> {
    let piece = raw.get(start..end).unwrap_or("");
    let abs = relative_to_absolute_position(ranges, start);
    match markdown.get(abs..abs + piece.len()) {
        Some(same) if same == piece => std::borrow::Cow::Borrowed(same),
        _ => std::borrow::Cow::Owned(piece.to_owned()),
    }
}

/// Go's `strings.TrimSpace(s) == ""`: blank under the *Unicode* White_Space property, which is
/// what `char::is_whitespace` implements. Distinct from [`is_whitespace`] on purpose.
pub(crate) fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapable_is_ascii_punctuation_only() {
        for b in 0u8..=255 {
            let expected = (0x21..=0x7e).contains(&b) && !(b as char).is_ascii_alphanumeric();
            assert_eq!(is_escapable_byte(b), expected, "byte {b:#x}");
        }
        assert!(!is_escapable('é'));
        assert!(!is_escapable('\u{a0}'));
    }

    #[test]
    fn whitespace_is_the_six_ascii_bytes() {
        let ws: Vec<u8> = (0u8..=255).filter(|b| is_whitespace_byte(*b)).collect();
        assert_eq!(ws, vec![b'\t', b'\n', 0x0b, 0x0c, b'\r', b' ']);
        assert!(!is_whitespace('\u{a0}'));
        assert!(is_blank("\u{a0}"));
    }

    #[test]
    fn next_line_handles_every_ending() {
        assert_eq!(next_line("ab\ncd", 0), (3, true));
        assert_eq!(next_line("ab\r\ncd", 0), (4, true));
        assert_eq!(next_line("ab\rcd", 0), (3, true));
        assert_eq!(next_line("  \ncd", 0), (3, false));
        assert_eq!(next_line("  ", 0), (2, false));
        assert_eq!(next_line("x", 1), (1, false));
        assert_eq!(next_line("\r", 0), (1, false));
        assert_eq!(next_line(" \x0b\n", 0), (3, false));
    }

    #[test]
    fn indentation_counts_tabs_as_four_columns() {
        assert_eq!(count_indentation("  \tx", Range::new(0, 4)), (6, 3));
        assert_eq!(count_indentation("\t\t", Range::new(0, 2)), (8, 2));
        assert_eq!(count_indentation("x  ", Range::new(0, 3)), (0, 0));
        assert_eq!(count_indentation("   x", Range::new(1, 4)), (2, 2));
        assert_eq!(count_indentation("\x0cx", Range::new(0, 2)), (0, 0));
    }

    #[test]
    fn trim_left_space_shrinks_the_end() {
        // Go's bug, preserved: two leading spaces cost two bytes off the END.
        assert_eq!(
            trim_left_space("  abcd", Range::new(0, 6)),
            Range::new(0, 4)
        );
        assert_eq!(trim_left_space("abcd", Range::new(0, 4)), Range::new(0, 4));
        assert_eq!(
            trim_right_space("ab  \n", Range::new(0, 5)),
            Range::new(0, 2)
        );
        assert_eq!(trim_right_space("   ", Range::new(0, 3)), Range::new(0, 0));
    }

    #[test]
    fn relative_positions_walk_the_ranges() {
        let ranges = [Range::new(2, 5), Range::new(10, 12)];
        assert_eq!(relative_to_absolute_position(&ranges, 0), 2);
        assert_eq!(relative_to_absolute_position(&ranges, 2), 4);
        assert_eq!(relative_to_absolute_position(&ranges, 3), 10);
        assert_eq!(relative_to_absolute_position(&ranges, 4), 11);
        assert_eq!(relative_to_absolute_position(&ranges, 5), 12);
        assert_eq!(relative_to_absolute_position(&ranges, 99), 12);
        assert_eq!(relative_to_absolute_position(&[], 3), 0);
    }

    #[test]
    fn trim_bytes_drops_consumed_ranges() {
        let ranges = [Range::new(0, 3), Range::new(5, 8), Range::new(9, 10)];
        assert_eq!(trim_bytes_from_ranges(&ranges, 0), ranges.to_vec());
        assert_eq!(
            trim_bytes_from_ranges(&ranges, 1),
            vec![Range::new(1, 3), Range::new(5, 8), Range::new(9, 10)]
        );
        assert_eq!(
            trim_bytes_from_ranges(&ranges, 3),
            vec![Range::new(5, 8), Range::new(9, 10)]
        );
        assert_eq!(
            trim_bytes_from_ranges(&ranges, 4),
            vec![Range::new(6, 8), Range::new(9, 10)]
        );
        assert_eq!(trim_bytes_from_ranges(&ranges, 7), Vec::<Range>::new());
        assert_eq!(trim_bytes_from_ranges(&ranges, 70), Vec::<Range>::new());
    }

    #[test]
    fn next_non_whitespace_stops_at_the_end() {
        assert_eq!(next_non_whitespace("  x", 0), 2);
        assert_eq!(next_non_whitespace("  ", 0), 2);
        assert_eq!(next_non_whitespace("x", 5), 1);
    }
}
