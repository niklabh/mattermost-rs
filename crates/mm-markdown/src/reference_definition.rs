//! Port of `reference_definition.go`: `[label]: destination "title"` definitions, which
//! `Paragraph.Close` peels off the front of a paragraph and `Parse` returns beside the document.

use std::borrow::Cow;

use crate::inlines::unescape;
use crate::links::{parse_link_destination, parse_link_label, parse_link_title};
use crate::markdown::{
    Range, cow_from_raw, is_whitespace_byte, next_line, next_non_whitespace,
    relative_to_absolute_position, trim_bytes_from_ranges,
};

/// Port of `markdown.ReferenceDefinition` (reference_definition.go:8).
///
/// Go hands out pointers to one definition from both the paragraph that declared it and the
/// `Parse` result; here it is a small value that is cloned into each place, so it is `Clone` and
/// compares by content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceDefinition<'a> {
    markdown: &'a str,
    /// The destination's byte range in the source. Public in Go; read through
    /// [`destination`](Self::destination), which unescapes it.
    pub raw_destination: Range,
    raw_label: Cow<'a, str>,
    raw_title: Cow<'a, str>,
}

impl<'a> ReferenceDefinition<'a> {
    /// Port of `Destination()` (reference_definition.go:16): the raw destination, unescaped.
    pub fn destination(&self) -> String {
        unescape(self.raw_destination.slice(self.markdown))
    }

    /// Port of `Label()` (reference_definition.go:20): the label exactly as written, **not**
    /// normalised — whitespace collapsing and case folding happen at lookup time.
    pub fn label(&self) -> &str {
        &self.raw_label
    }

    /// Port of `Title()` (reference_definition.go:24): the raw title, unescaped; `""` when the
    /// definition had none.
    pub fn title(&self) -> String {
        unescape(&self.raw_title)
    }
}

/// Port of `parseReferenceDefinition` (reference_definition.go:28) over the paragraph's
/// concatenated ranges. On success returns the definition and the ranges that remain after the
/// line the definition ended on.
///
/// The title handling has three outcomes, all pinned by the corpus: a well-formed title followed
/// by only whitespace to the end of its line is taken; a malformed title on the *same* line as
/// the destination fails the whole definition; a malformed title on the *next* line is simply
/// not a title, and the definition ends at the destination's line.
pub(crate) fn parse_reference_definition<'a>(
    markdown: &'a str,
    ranges: &[Range],
) -> Option<(ReferenceDefinition<'a>, Vec<Range>)> {
    let mut raw = String::new();
    for r in ranges {
        raw.push_str(r.slice(markdown));
    }
    let bytes = raw.as_bytes();

    let (label, next) = parse_link_label(&raw, 0)?;
    let mut position = next;

    if position >= bytes.len() || bytes[position] != b':' {
        return None;
    }
    position += 1;

    let (destination, next) = parse_link_destination(&raw, next_non_whitespace(&raw, position))?;
    position = next;

    let absolute_destination = relative_to_absolute_position(ranges, destination.position);
    let mut ret = ReferenceDefinition {
        markdown,
        raw_destination: Range::new(
            absolute_destination,
            absolute_destination + destination.end - destination.position,
        ),
        raw_label: cow_from_raw(markdown, ranges, &raw, label.position, label.end),
        raw_title: Cow::Borrowed(""),
    };

    if position < bytes.len() && is_whitespace_byte(bytes[position]) {
        match parse_link_title(&raw, next_non_whitespace(&raw, position)) {
            None => {
                let (nl, skipped_non_whitespace) = next_line(&raw, position);
                if !skipped_non_whitespace {
                    return Some((ret, trim_bytes_from_ranges(ranges, nl)));
                }
                return None;
            }
            Some((title, next)) => {
                let (nl, skipped_non_whitespace) = next_line(&raw, next);
                if !skipped_non_whitespace {
                    ret.raw_title = cow_from_raw(markdown, ranges, &raw, title.position, title.end);
                    return Some((ret, trim_bytes_from_ranges(ranges, nl)));
                }
            }
        }
    }

    let (nl, skipped_non_whitespace) = next_line(&raw, position);
    if !skipped_non_whitespace {
        return Some((ret, trim_bytes_from_ranges(ranges, nl)));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(markdown: &str) -> Vec<Range> {
        vec![Range::new(0, markdown.len())]
    }

    #[test]
    fn definition_forms() {
        let md = "[foo]: /url \"title\"\nrest";
        let (d, rest) = parse_reference_definition(md, &whole(md)).unwrap();
        assert_eq!(d.label(), "foo");
        assert_eq!(d.destination(), "/url");
        assert_eq!(d.title(), "title");
        assert_eq!(rest, vec![Range::new(20, 24)]);

        let md = "[foo]: /url\nrest";
        let (d, rest) = parse_reference_definition(md, &whole(md)).unwrap();
        assert_eq!(d.title(), "");
        assert_eq!(rest, vec![Range::new(12, 16)]);

        // Malformed title on the same line: not a definition at all.
        let md = "[foo]: /url 'bad\n";
        assert!(parse_reference_definition(md, &whole(md)).is_none());

        // Malformed title on the next line: a definition without a title.
        let md = "[foo]: /url\n'bad\n";
        let (d, rest) = parse_reference_definition(md, &whole(md)).unwrap();
        assert_eq!(d.title(), "");
        assert_eq!(rest, vec![Range::new(12, 17)]);

        // Junk after the destination.
        let md = "[foo]: /url junk\n";
        assert!(parse_reference_definition(md, &whole(md)).is_none());

        // No colon, no destination.
        assert!(parse_reference_definition("[foo] /url", &whole("[foo] /url")).is_none());
        assert!(parse_reference_definition("[foo]: \n", &whole("[foo]: \n")).is_none());
        assert!(parse_reference_definition("[foo]:", &whole("[foo]:")).is_none());
    }

    #[test]
    fn label_and_title_may_span_ranges() {
        // Two lines, the label broken across them; the title borrows nothing.
        let md = "[foo\nbar]: /u \"t\"";
        let ranges = vec![Range::new(0, 5), Range::new(5, md.len())];
        let (d, rest) = parse_reference_definition(md, &ranges).unwrap();
        assert_eq!(d.label(), "foo\nbar");
        assert_eq!(d.destination(), "/u");
        assert_eq!(d.title(), "t");
        assert!(rest.is_empty());
        assert!(matches!(d.raw_label, Cow::Borrowed(_)));

        // The same label with a prefix stripped from the second line cannot borrow.
        let md = "[foo\n> bar]: /u";
        let ranges = vec![Range::new(0, 5), Range::new(7, md.len())];
        let (d, _) = parse_reference_definition(md, &ranges).unwrap();
        assert_eq!(d.label(), "foo\nbar");
        assert!(matches!(d.raw_label, Cow::Owned(_)));
    }

    #[test]
    fn destination_and_title_are_unescaped() {
        let md = "[a]: /u\\*rl&amp; \"t\\\"&copy;\"";
        let (d, _) = parse_reference_definition(md, &whole(md)).unwrap();
        assert_eq!(d.destination(), "/u*rl&");
        assert_eq!(d.title(), "t\"©");
    }
}
