//! Port of `emoji.go`: `:name:` emoji, from the mobile app's commonmark.js extension.

use crate::markdown::is_word_byte;

/// Port of `parseEmoji` (emoji.go:18) for a `:` at `position`: the emoji name and the number of
/// bytes matched, or `None`.
///
/// The regexp is `^:([a-z0-9_\-+]+):\B` (RE2, so ASCII classes and an ASCII `\B`): the byte
/// after the closing colon must not be a word byte, and the end of the text counts as
/// non-word. The class excludes `:`, so the greedy group has no alternative and a hand scan is
/// exactly the regexp. Go additionally refuses a word byte *before* the colon — but only when
/// `position > 1`, so position 1 is never checked (`a:smile:` at 1 matches; at 2 it would not).
pub(crate) fn parse_emoji(raw: &str, position: usize) -> Option<(&str, usize)> {
    let bytes = raw.as_bytes();
    // Only allow emojis after non-word characters
    if position > 1 && is_word_byte(bytes[position - 1]) {
        return None;
    }
    if position >= bytes.len() || bytes[position] != b':' {
        return None;
    }
    let mut j = position + 1;
    while j < bytes.len() && matches!(bytes[j], b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'+') {
        j += 1;
    }
    if j == position + 1 || j >= bytes.len() || bytes[j] != b':' {
        return None;
    }
    // `\B` after the closing colon.
    if j + 1 < bytes.len() && is_word_byte(bytes[j + 1]) {
        return None;
    }
    Some((&raw[position + 1..j], j + 1 - position))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_rules() {
        assert_eq!(parse_emoji(":smile:", 0), Some(("smile", 7)));
        assert_eq!(parse_emoji(":smile: x", 0), Some(("smile", 7)));
        assert_eq!(parse_emoji(":smile:x", 0), None);
        assert_eq!(parse_emoji(":smile:_", 0), None);
        assert_eq!(parse_emoji(":smile:é", 0), Some(("smile", 7)));
        assert_eq!(parse_emoji(":+1:", 0), Some(("+1", 4)));
        assert_eq!(parse_emoji(":Smile:", 0), None);
        assert_eq!(parse_emoji("::", 0), None);
        assert_eq!(parse_emoji(":smile", 0), None);
        assert_eq!(parse_emoji("a:smile:", 1), Some(("smile", 7)));
        assert_eq!(parse_emoji("ab:smile:", 2), None);
        assert_eq!(parse_emoji(" :smile:", 1), Some(("smile", 7)));
        assert_eq!(parse_emoji("x", 0), None);
        assert_eq!(parse_emoji("", 0), None);
    }
}
