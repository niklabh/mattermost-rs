//! Port of `links.go`: the destination, title, label and image-dimension scanners shared by
//! inline links and reference definitions. All four work on bytes, as Go's do, and return
//! `(range, next_position)` on success.

use crate::markdown::{Range, is_escapable_byte, is_numeric_byte, is_whitespace_byte};

/// Port of `parseLinkDestination` (links.go:9). Tries the `<...>` form first; if that form does
/// not close before whitespace or a second `<`, Go falls through and parses from the `<` as a
/// bare destination, and so does this. A bare destination ends at whitespace or at an unbalanced
/// `)`, or at the end of the string — it never fails, only the `position >= len` guard does.
pub(crate) fn parse_link_destination(markdown: &str, position: usize) -> Option<(Range, usize)> {
    let bytes = markdown.as_bytes();
    if position >= bytes.len() {
        return None;
    }

    if bytes[position] == b'<' {
        let mut is_escaped = false;
        for (offset, &c) in bytes[position + 1..].iter().enumerate() {
            if is_escaped {
                is_escaped = false;
                if is_escapable_byte(c) {
                    continue;
                }
            }
            if c == b'\\' {
                is_escaped = true;
            } else if c == b'<' {
                break;
            } else if c == b'>' {
                return Some((
                    Range::new(position + 1, position + 1 + offset),
                    position + 1 + offset + 1,
                ));
            } else if is_whitespace_byte(c) {
                break;
            }
        }
    }

    let mut open_count = 0usize;
    let mut is_escaped = false;
    for (offset, &c) in bytes[position..].iter().enumerate() {
        if is_escaped {
            is_escaped = false;
            if is_escapable_byte(c) {
                continue;
            }
        }
        match c {
            b'\\' => is_escaped = true,
            b'(' => open_count += 1,
            b')' => {
                if open_count < 1 {
                    return Some((Range::new(position, position + offset), position + offset));
                }
                open_count -= 1;
            }
            _ => {
                if is_whitespace_byte(c) {
                    return Some((Range::new(position, position + offset), position + offset));
                }
            }
        }
    }
    Some((Range::new(position, bytes.len()), bytes.len()))
}

/// Port of `parseLinkTitle` (links.go:68): `"..."`, `'...'` or `(...)`, with backslash escapes
/// skipped. The range excludes the delimiters and may span line endings.
pub(crate) fn parse_link_title(markdown: &str, position: usize) -> Option<(Range, usize)> {
    let bytes = markdown.as_bytes();
    if position >= bytes.len() {
        return None;
    }
    let original_position = position;
    let closer = match bytes[position] {
        c @ (b'"' | b'\'') => c,
        b'(' => b')',
        _ => return None,
    };
    let mut position = position + 1;
    while position < bytes.len() {
        let c = bytes[position];
        if c == b'\\' {
            position += 1;
            if position < bytes.len() && is_escapable_byte(bytes[position]) {
                position += 1;
            }
        } else if c == closer {
            return Some((Range::new(original_position + 1, position), position + 1));
        } else {
            position += 1;
        }
    }
    None
}

/// Port of `parseLinkLabel` (links.go:100): `[...]` with no unescaped `[` inside. Refused when
/// the label spans 1000 or more bytes **and** 1000 or more runes — both, so a label of 600
/// two-byte characters (1200 bytes) is accepted.
pub(crate) fn parse_link_label(markdown: &str, position: usize) -> Option<(Range, usize)> {
    let bytes = markdown.as_bytes();
    if position >= bytes.len() || bytes[position] != b'[' {
        return None;
    }
    let original_position = position;
    let mut position = position + 1;
    while position < bytes.len() {
        match bytes[position] {
            b'\\' => {
                position += 1;
                if position < bytes.len() && is_escapable_byte(bytes[position]) {
                    position += 1;
                }
            }
            b'[' => return None,
            b']' => {
                if position - original_position >= 1000
                    && markdown
                        .get(original_position..position)
                        .map(|s| s.chars().count())
                        .unwrap_or(0)
                        >= 1000
                {
                    return None;
                }
                return Some((Range::new(original_position + 1, position), position + 1));
            }
            _ => position += 1,
        }
    }
    None
}

/// Port of `parseImageDimensions` (links.go:132): the non-standard `=WIDTHxHEIGHT` after an
/// image destination. Width and height are each optional but one is required; the returned
/// range is never used by the caller, which only needs `next`.
pub(crate) fn parse_image_dimensions(markdown: &str, position: usize) -> Option<(Range, usize)> {
    let bytes = markdown.as_bytes();
    if position >= bytes.len() {
        return None;
    }
    let original_position = position;

    // Read =
    let mut position = position + 1;
    if position >= bytes.len() {
        return None;
    }

    // Read width
    let mut has_width = false;
    while position < bytes.len() - 1 && is_numeric_byte(bytes[position]) {
        has_width = true;
        position += 1;
    }

    // Look for early end of dimensions
    if is_whitespace_byte(bytes[position]) || bytes[position] == b')' {
        return Some((Range::new(original_position, position - 1), position));
    }

    // Read the x
    if (bytes[position] != b'x' && bytes[position] != b'X') || position == bytes.len() - 1 {
        return None;
    }
    position += 1;

    // Read height
    let mut has_height = false;
    while position < bytes.len() - 1 && is_numeric_byte(bytes[position]) {
        has_height = true;
        position += 1;
    }

    // Make sure there are no trailing characters
    if !is_whitespace_byte(bytes[position]) && bytes[position] != b')' {
        return None;
    }

    if !has_width && !has_height {
        return None;
    }

    Some((Range::new(original_position, position - 1), position))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_forms() {
        assert_eq!(
            parse_link_destination("<ab>)", 0),
            Some((Range::new(1, 3), 4))
        );
        // Whitespace inside `<...>` abandons the angle form (unlike CommonMark), and the bare
        // form then stops at that same space, `<` included.
        assert_eq!(
            parse_link_destination("<a b>)", 0),
            Some((Range::new(0, 2), 2))
        );
        assert_eq!(
            parse_link_destination("<a b", 0),
            Some((Range::new(0, 2), 2))
        );
        // A second `<` also abandons the angle form.
        assert_eq!(
            parse_link_destination("<a<b>", 0),
            Some((Range::new(0, 5), 5))
        );
        assert_eq!(
            parse_link_destination("a(b)c) d", 0),
            Some((Range::new(0, 5), 5))
        );
        assert_eq!(
            parse_link_destination("a\\)b)", 0),
            Some((Range::new(0, 4), 4))
        );
        assert_eq!(
            parse_link_destination("abc", 0),
            Some((Range::new(0, 3), 3))
        );
        assert_eq!(parse_link_destination(")", 0), Some((Range::new(0, 0), 0)));
        assert_eq!(
            parse_link_destination("<\\>x>", 0),
            Some((Range::new(1, 4), 5))
        );
        assert_eq!(parse_link_destination("", 0), None);
        assert_eq!(parse_link_destination("x", 1), None);
    }

    #[test]
    fn title_forms() {
        assert_eq!(parse_link_title("\"a\"", 0), Some((Range::new(1, 2), 3)));
        assert_eq!(parse_link_title("'a'", 0), Some((Range::new(1, 2), 3)));
        assert_eq!(parse_link_title("(a)", 0), Some((Range::new(1, 2), 3)));
        assert_eq!(parse_link_title("(a(b))", 0), Some((Range::new(1, 4), 5)));
        assert_eq!(
            parse_link_title("\"a\\\"b\"", 0),
            Some((Range::new(1, 5), 6))
        );
        assert_eq!(parse_link_title("\"a\\é\"", 0), Some((Range::new(1, 5), 6)));
        assert_eq!(parse_link_title("\"a\nb\"", 0), Some((Range::new(1, 4), 5)));
        assert_eq!(parse_link_title("\"a", 0), None);
        assert_eq!(parse_link_title("a\"", 0), None);
        assert_eq!(parse_link_title("", 0), None);
        assert_eq!(parse_link_title("\"\\", 0), None);
    }

    #[test]
    fn label_forms() {
        assert_eq!(parse_link_label("[a]", 0), Some((Range::new(1, 2), 3)));
        assert_eq!(parse_link_label("[]", 0), Some((Range::new(1, 1), 2)));
        assert_eq!(parse_link_label("[a\\]b]", 0), Some((Range::new(1, 5), 6)));
        assert_eq!(parse_link_label("[a[b]]", 0), None);
        assert_eq!(parse_link_label("[a", 0), None);
        assert_eq!(parse_link_label("a]", 0), None);
        // `position - originalPosition` counts the opening bracket, so 999 content bytes is
        // already 1000 and refused; 998 is the longest ASCII label.
        let long = format!("[{}]", "a".repeat(998));
        assert_eq!(parse_link_label(&long, 0), Some((Range::new(1, 999), 1000)));
        let too_long = format!("[{}]", "a".repeat(999));
        assert_eq!(parse_link_label(&too_long, 0), None);
        let wide = format!("[{}]", "é".repeat(600));
        assert_eq!(
            parse_link_label(&wide, 0),
            Some((Range::new(1, 1201), 1202))
        );
    }

    #[test]
    fn image_dimensions() {
        assert_eq!(
            parse_image_dimensions("=100x200)", 0),
            Some((Range::new(0, 7), 8))
        );
        assert_eq!(
            parse_image_dimensions("=100x)", 0),
            Some((Range::new(0, 4), 5))
        );
        assert_eq!(
            parse_image_dimensions("=x200 ", 0),
            Some((Range::new(0, 4), 5))
        );
        assert_eq!(
            parse_image_dimensions("=100 ", 0),
            Some((Range::new(0, 3), 4))
        );
        assert_eq!(parse_image_dimensions("=x)", 0), None);
        assert_eq!(parse_image_dimensions("=abc)", 0), None);
        assert_eq!(parse_image_dimensions("=100x200z)", 0), None);
        assert_eq!(parse_image_dimensions("=", 0), None);
        assert_eq!(parse_image_dimensions("=100x200", 0), None);
        assert_eq!(parse_image_dimensions("=100x", 0), None);
        assert_eq!(parse_image_dimensions("=)", 0), Some((Range::new(0, 0), 1)));
        assert_eq!(parse_image_dimensions("", 0), None);
    }
}
