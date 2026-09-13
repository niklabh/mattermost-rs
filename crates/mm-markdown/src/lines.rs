//! Port of `lines.go`: splitting the source into line ranges.

use crate::markdown::Range;

/// Port of `markdown.ParseLines` (lines.go:13). A line ends after `\n` (so `\r\n` stays in one
/// line), or *before* the character that follows a lone `\r`; the line ending is part of the
/// range. A final line with no ending is included; an empty input has no lines.
pub fn parse_lines(markdown: &str) -> Vec<Range> {
    let mut lines = Vec::with_capacity(markdown.matches('\n').count());
    let mut line_start = 0;
    let mut is_after_carriage_return = false;
    for (position, r) in markdown.char_indices() {
        if r == '\n' {
            lines.push(Range::new(line_start, position + 1));
            line_start = position + 1;
        } else if is_after_carriage_return {
            lines.push(Range::new(line_start, position));
            line_start = position;
        }
        is_after_carriage_return = r == '\r';
    }
    if line_start < markdown.len() {
        lines.push(Range::new(line_start, markdown.len()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_every_ending() {
        assert_eq!(parse_lines(""), vec![]);
        assert_eq!(parse_lines("a"), vec![Range::new(0, 1)]);
        assert_eq!(parse_lines("a\n"), vec![Range::new(0, 2)]);
        assert_eq!(
            parse_lines("a\nb"),
            vec![Range::new(0, 2), Range::new(2, 3)]
        );
        assert_eq!(
            parse_lines("a\r\nb"),
            vec![Range::new(0, 3), Range::new(3, 4)]
        );
        assert_eq!(
            parse_lines("a\rb"),
            vec![Range::new(0, 2), Range::new(2, 3)]
        );
        assert_eq!(parse_lines("a\r"), vec![Range::new(0, 2)]);
        assert_eq!(
            parse_lines("a\r\r\nb"),
            vec![Range::new(0, 2), Range::new(2, 4), Range::new(4, 5)]
        );
        assert_eq!(
            parse_lines("\n\n"),
            vec![Range::new(0, 1), Range::new(1, 2)]
        );
        assert_eq!(
            parse_lines("é\rb"),
            vec![Range::new(0, 3), Range::new(3, 4)]
        );
    }
}
