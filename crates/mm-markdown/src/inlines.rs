//! Port of `inlines.go`: the inline node types, the inline parser, `MergeInlineText`,
//! `CharacterReference` and `Unescape`.
//!
//! The parser works on `raw`, the concatenation of a paragraph's line ranges, and maps positions
//! back to the source with `relativeToAbsolutePosition` exactly where Go does — including the
//! places where the resulting `Text.range` is not the text's real extent (an entity's range is
//! its *decoded* length; a run cut short by end-of-input keeps its untrimmed length). Those
//! ranges are what `MergeInlineText` compares, so they are part of the behaviour.

use std::borrow::Cow;

use crate::autolink::{parse_url_autolink, parse_www_autolink};
use crate::emoji::parse_emoji;
use crate::html_entities;
use crate::links::{
    parse_image_dimensions, parse_link_destination, parse_link_label, parse_link_title,
};
use crate::markdown::{
    Range, cow_from_raw, is_escapable_byte, is_whitespace, is_whitespace_byte, next_non_whitespace,
    relative_to_absolute_position,
};
use crate::reference_definition::ReferenceDefinition;
use crate::unicode::equal_fold;

/// Port of `markdown.Text` (inlines.go:21). `text` is what the node says; `range` is where Go
/// says it came from, which is not always the same length (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text<'a> {
    pub text: Cow<'a, str>,
    pub range: Range,
}

/// Port of `markdown.CodeSpan` (inlines.go:28): the code with its whitespace runs collapsed to
/// single spaces (`strings.Fields` joined by `" "`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeSpan {
    pub code: String,
}

/// Port of `markdown.InlineLinkOrImage` (inlines.go:42), the body of both `InlineLink` and
/// `InlineImage`.
#[derive(Debug, PartialEq, Eq)]
pub struct InlineLinkOrImage<'a> {
    markdown: &'a str,
    pub children: Vec<Inline<'a>>,
    /// The destination's byte range in the source; read through
    /// [`destination`](Self::destination), which unescapes it.
    pub raw_destination: Range,
    raw_title: Cow<'a, str>,
}

impl InlineLinkOrImage<'_> {
    /// Port of `Destination()` (inlines.go:52).
    pub fn destination(&self) -> String {
        unescape(self.raw_destination.slice(self.markdown))
    }

    /// Port of `Title()` (inlines.go:56); `""` without a title.
    pub fn title(&self) -> String {
        unescape(&self.raw_title)
    }
}

impl Drop for InlineLinkOrImage<'_> {
    // Images nest without bound (`![![![…`), and 130 KiB of input is enough nesting to overflow
    // a thread's stack in the compiler-generated recursive drop. Flatten the children first.
    fn drop(&mut self) {
        drain_nested_children(&mut self.children);
    }
}

/// Port of `markdown.ReferenceLinkOrImage` (inlines.go:68), the body of `ReferenceLink` and
/// `ReferenceImage`. Go embeds a pointer to the definition; this holds a copy of it.
#[derive(Debug, PartialEq, Eq)]
pub struct ReferenceLinkOrImage<'a> {
    pub definition: ReferenceDefinition<'a>,
    pub children: Vec<Inline<'a>>,
}

impl ReferenceLinkOrImage<'_> {
    /// The embedded definition's `Destination()`.
    pub fn destination(&self) -> String {
        self.definition.destination()
    }

    /// The embedded definition's `Title()`.
    pub fn title(&self) -> String {
        self.definition.title()
    }

    /// The embedded definition's `Label()`: the label of the *definition*, not the text the
    /// link was written with.
    pub fn label(&self) -> &str {
        self.definition.label()
    }
}

impl Drop for ReferenceLinkOrImage<'_> {
    fn drop(&mut self) {
        drain_nested_children(&mut self.children);
    }
}

/// Moves every descendant out into a flat worklist so each node is dropped with no children of
/// its own — an iterative drop for an unboundedly deep tree.
fn drain_nested_children(children: &mut Vec<Inline<'_>>) {
    let mut stack = std::mem::take(children);
    while let Some(mut inline) = stack.pop() {
        match &mut inline {
            Inline::InlineLink(l) | Inline::InlineImage(l) => stack.append(&mut l.children),
            Inline::ReferenceLink(l) | Inline::ReferenceImage(l) => stack.append(&mut l.children),
            _ => {}
        }
    }
}

/// Port of `markdown.Autolink` (inlines.go:84): a bare `scheme://` or `www.` link. Its single
/// child is the link text; `InspectInline` does **not** descend into it, and neither does
/// [`crate::inspect`].
#[derive(Debug, PartialEq, Eq)]
pub struct Autolink<'a> {
    markdown: &'a str,
    pub children: Vec<Inline<'a>>,
    pub raw_destination: Range,
}

impl Autolink<'_> {
    /// Port of `Destination()` (inlines.go:94): unescaped, and prefixed with `http://` when it
    /// starts with `www`.
    pub fn destination(&self) -> String {
        let destination = unescape(self.raw_destination.slice(self.markdown));
        if destination.starts_with("www") {
            return format!("http://{destination}");
        }
        destination
    }
}

/// Port of `markdown.Emoji` (inlines.go:104). The name is whatever sat between the colons;
/// nothing checks that an emoji of that name exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emoji<'a> {
    pub name: Cow<'a, str>,
}

/// Port of the `markdown.Inline` interface (inlines.go:13) and its ten implementations.
#[derive(Debug, PartialEq, Eq)]
pub enum Inline<'a> {
    Text(Text<'a>),
    CodeSpan(CodeSpan),
    HardLineBreak,
    SoftLineBreak,
    InlineLink(InlineLinkOrImage<'a>),
    InlineImage(InlineLinkOrImage<'a>),
    ReferenceLink(ReferenceLinkOrImage<'a>),
    ReferenceImage(ReferenceLinkOrImage<'a>),
    Autolink(Autolink<'a>),
    Emoji(Emoji<'a>),
}

impl<'a> Inline<'a> {
    /// The children `InspectInline` (inspect.go:117) descends into: the four link and image
    /// kinds. An `Autolink`'s child is not among them.
    pub fn inspected_children(&self) -> &[Inline<'a>] {
        match self {
            Inline::InlineLink(l) | Inline::InlineImage(l) => &l.children,
            Inline::ReferenceLink(l) | Inline::ReferenceImage(l) => &l.children,
            _ => &[],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DelimiterType {
    LinkOpening,
    ImageOpening,
}

/// Port of `delimiter` (inlines.go:117). `range` is in `raw` coordinates.
#[derive(Clone, Copy)]
struct Delimiter {
    kind: DelimiterType,
    is_inactive: bool,
    text_node: usize,
    range: Range,
}

/// Port of `inlineParser` (inlines.go:124).
struct InlineParser<'a, 'r> {
    markdown: &'a str,
    ranges: &'r [Range],
    reference_definitions: &'r [ReferenceDefinition<'a>],

    raw: String,
    position: usize,
    inlines: Vec<Inline<'a>>,
    delimiter_stack: Vec<Delimiter>,
}

impl<'a, 'r> InlineParser<'a, 'r> {
    fn new(
        markdown: &'a str,
        ranges: &'r [Range],
        reference_definitions: &'r [ReferenceDefinition<'a>],
    ) -> Self {
        let mut raw = String::new();
        for r in ranges {
            raw.push_str(r.slice(markdown));
        }
        InlineParser {
            markdown,
            ranges,
            reference_definitions,
            raw,
            position: 0,
            inlines: Vec::new(),
            delimiter_stack: Vec::new(),
        }
    }

    fn abs(&self, position: usize) -> usize {
        relative_to_absolute_position(self.ranges, position)
    }

    fn cow(&self, start: usize, end: usize) -> Cow<'a, str> {
        cow_from_raw(self.markdown, self.ranges, &self.raw, start, end)
    }

    fn push_text(&mut self, text: Cow<'a, str>, range: Range) {
        self.inlines.push(Inline::Text(Text { text, range }));
    }

    /// Port of `parseBackticks` (inlines.go:146).
    fn parse_backticks(&mut self) {
        let bytes = self.raw.as_bytes();
        let len = bytes.len();
        let mut count = 1;
        let mut i = self.position + 1;
        while i < len && bytes[i] == b'`' {
            count += 1;
            i += 1;
        }
        let opening_start = self.position;
        let opening_end = self.position + count;
        let mut search = opening_end;
        while search < len {
            let Some(end) = self.raw[search..].find(&self.raw[opening_start..opening_end]) else {
                break;
            };
            if search + end + count < len && bytes[search + end + count] == b'`' {
                search += end + count;
                while search < len && bytes[search] == b'`' {
                    search += 1;
                }
                continue;
            }
            let code = self.raw[opening_end..search + end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            self.position = search + end + count;
            self.inlines.push(Inline::CodeSpan(CodeSpan { code }));
            return;
        }
        self.position += count;
        let abs = self.abs(opening_start);
        self.push_text(
            self.cow(opening_start, opening_end),
            Range::new(abs, abs + count),
        );
    }

    /// Port of `parseLineEnding` (inlines.go:171). A tab before the ending is a hard break, and
    /// so is **one** space — the second clause of Go's condition re-tests what its first
    /// conjunct already established, so the two-space rule never applies.
    fn parse_line_ending(&mut self) {
        let bytes = self.raw.as_bytes();
        let p = self.position;
        // Go's two `if` arms, joined: `raw[p-1] == '\t'`, or `p >= 2 && raw[p-1] == ' ' &&
        // (raw[p-2] == '\t' || raw[p-1] == ' ')` — whose parenthesised half is always true.
        let tab_before = p >= 1 && bytes[p - 1] == b'\t';
        let space_before =
            p >= 2 && bytes[p - 1] == b' ' && (bytes[p - 2] == b'\t' || bytes[p - 1] == b' ');
        if tab_before || space_before {
            self.inlines.push(Inline::HardLineBreak);
        } else {
            self.inlines.push(Inline::SoftLineBreak);
        }
        self.position += 1;
        if self.position < bytes.len() && bytes[self.position] == b'\n' {
            self.position += 1;
        }
    }

    /// Port of `parseEscapeCharacter` (inlines.go:185).
    fn parse_escape_character(&mut self) {
        let bytes = self.raw.as_bytes();
        let p = self.position;
        if p + 1 < bytes.len() && is_escapable_byte(bytes[p + 1]) {
            let abs = self.abs(p + 1);
            self.push_text(self.cow(p + 1, p + 2), Range::new(abs, abs + 1));
            self.position += 2;
        } else {
            let abs = self.abs(p);
            self.push_text(Cow::Borrowed("\\"), Range::new(abs, abs + 1));
            self.position += 1;
        }
    }

    /// Port of `parseText` (inlines.go:203): a run up to the next byte another parser might
    /// want. At least one byte is always consumed, since `w`, `W` and `:` reach here when they
    /// did not start anything.
    fn parse_text(&mut self) {
        let p = self.position;
        let Some(rest) = self.raw.get(p..) else {
            // Not a char boundary — unreachable, every advance is by ASCII or a whole run — but
            // the loop must still make progress.
            self.position += 1;
            return;
        };
        let abs = self.abs(p);
        match rest.bytes().position(|b| b"\r\n\\`&![]wW:".contains(&b)) {
            None => {
                let text = rest.trim_end_matches(is_whitespace);
                let text_len = text.len();
                let rest_len = rest.len();
                self.push_text(self.cow(p, p + text_len), Range::new(abs, abs + rest_len));
                self.position = self.raw.len();
            }
            Some(next) => {
                let c = rest.as_bytes()[next];
                if c == b'\r' || c == b'\n' {
                    let s_len = rest[..next].trim_end_matches(is_whitespace).len();
                    self.push_text(self.cow(p, p + s_len), Range::new(abs, abs + s_len));
                    self.position += next;
                } else {
                    // Always read at least one character since 'w', 'W', and ':' may not
                    // actually match another type of node.
                    let next = if next == 0 { 1 } else { next };
                    self.push_text(self.cow(p, p + next), Range::new(abs, abs + next));
                    self.position += next;
                }
            }
        }
    }

    /// Port of `parseLinkOrImageDelimiter` (inlines.go:235).
    fn parse_link_or_image_delimiter(&mut self) {
        let bytes = self.raw.as_bytes();
        let p = self.position;
        let abs = self.abs(p);
        if bytes[p] == b'[' {
            self.push_text(Cow::Borrowed("["), Range::new(abs, abs + 1));
            self.delimiter_stack.push(Delimiter {
                kind: DelimiterType::LinkOpening,
                is_inactive: false,
                text_node: self.inlines.len() - 1,
                range: Range::new(p, p + 1),
            });
            self.position += 1;
        } else if bytes[p] == b'!' && p + 1 < bytes.len() && bytes[p + 1] == b'[' {
            self.push_text(Cow::Borrowed("!["), Range::new(abs, abs + 2));
            self.delimiter_stack.push(Delimiter {
                kind: DelimiterType::ImageOpening,
                is_inactive: false,
                text_node: self.inlines.len() - 1,
                range: Range::new(p, p + 2),
            });
            self.position += 2;
        } else {
            self.push_text(Cow::Borrowed("!"), Range::new(abs, abs + 1));
            self.position += 1;
        }
    }

    /// Port of `peekAtInlineLinkDestinationAndTitle` (inlines.go:271): `(destination, title,
    /// end)` for a `(...)` starting at `position`, or `None`. Both ranges may be empty.
    fn peek_at_inline_link_destination_and_title(
        &self,
        position: usize,
        is_image: bool,
    ) -> Option<(Range, Range, usize)> {
        let raw = self.raw.as_str();
        let bytes = raw.as_bytes();
        if position >= bytes.len() || bytes[position] != b'(' {
            return None;
        }
        let mut position = position + 1;

        let destination_start = next_non_whitespace(raw, position);
        if destination_start >= bytes.len() {
            return None;
        } else if bytes[destination_start] == b')' {
            return Some((
                Range::new(destination_start, destination_start),
                Range::new(destination_start, destination_start),
                destination_start + 1,
            ));
        }

        let (destination, end) = parse_link_destination(raw, destination_start)?;
        position = end;

        if is_image && position < bytes.len() && is_whitespace_byte(bytes[position]) {
            let dimensions_start = next_non_whitespace(raw, position);
            if dimensions_start >= bytes.len() {
                return None;
            }
            if bytes[dimensions_start] == b'=' {
                // Read optional image dimensions even if we don't use them
                let (_, end) = parse_image_dimensions(raw, dimensions_start)?;
                position = end;
            }
        }

        let mut title = Range::default();
        if position < bytes.len() && is_whitespace_byte(bytes[position]) {
            let title_start = next_non_whitespace(raw, position);
            if title_start >= bytes.len() {
                return None;
            } else if bytes[title_start] == b')' {
                return Some((
                    destination,
                    Range::new(title_start, title_start),
                    title_start + 1,
                ));
            }

            if matches!(bytes[title_start], b'"' | b'\'' | b'(') {
                let (t, end) = parse_link_title(raw, title_start)?;
                title = t;
                position = end;
            }
        }

        let closing_position = next_non_whitespace(raw, position);
        if closing_position >= bytes.len() || bytes[closing_position] != b')' {
            return None;
        }

        Some((destination, title, closing_position + 1))
    }

    /// Port of `referenceDefinition` (inlines.go:331): the first definition whose label, with
    /// whitespace runs collapsed, is `strings.EqualFold`-equal to the lookup label likewise
    /// collapsed. First definition wins on duplicates.
    fn reference_definition(&self, label: &str) -> Option<&'r ReferenceDefinition<'a>> {
        let clean = label.split_whitespace().collect::<Vec<_>>().join(" ");
        self.reference_definitions.iter().find(|d| {
            let candidate = d.label().split_whitespace().collect::<Vec<_>>().join(" ");
            equal_fold(&clean, &candidate)
        })
    }

    /// Port of `lookForLinkOrImage` (inlines.go:340), run at a `]`. Only the most recent
    /// delimiter is ever considered: every branch of Go's loop removes it and stops. An inactive
    /// one is discarded and the `]` is text; otherwise an inline link/image is tried, then a
    /// reference; on success the inlines since the opener become the children and, for a link,
    /// every earlier link opener is deactivated (links do not nest; images do).
    fn look_for_link_or_image(&mut self) {
        if let Some(idx) = self.delimiter_stack.len().checked_sub(1) {
            let d = self.delimiter_stack[idx];
            if d.is_inactive {
                self.delimiter_stack.remove(idx);
            } else {
                let is_image = d.kind == DelimiterType::ImageOpening;
                let mut inline = None;

                if let Some((destination, title, next)) =
                    self.peek_at_inline_link_destination_and_title(self.position + 1, is_image)
                {
                    let destination_markdown_position = self.abs(destination.position);
                    let raw_title = self.cow(title.position, title.end);
                    let link_or_image = InlineLinkOrImage {
                        markdown: self.markdown,
                        children: self.inlines.split_off(d.text_node + 1),
                        raw_destination: Range::new(
                            destination_markdown_position,
                            destination_markdown_position + destination.end - destination.position,
                        ),
                        raw_title,
                    };
                    inline = Some(if is_image {
                        Inline::InlineImage(link_or_image)
                    } else {
                        Inline::InlineLink(link_or_image)
                    });
                    self.position = next;
                } else {
                    let label = parse_link_label(&self.raw, self.position + 1);
                    let (reference_label, next) = match label {
                        Some((label, next)) if label.end > label.position => {
                            (self.raw[label.position..label.end].to_owned(), next)
                        }
                        Some((_, next)) => (self.raw[d.range.end..self.position].to_owned(), next),
                        None => (
                            self.raw[d.range.end..self.position].to_owned(),
                            self.position + 1,
                        ),
                    };
                    if !reference_label.is_empty() {
                        if let Some(reference) =
                            self.reference_definition(&reference_label).cloned()
                        {
                            let link_or_image = ReferenceLinkOrImage {
                                definition: reference,
                                children: self.inlines.split_off(d.text_node + 1),
                            };
                            inline = Some(if is_image {
                                Inline::ReferenceImage(link_or_image)
                            } else {
                                Inline::ReferenceLink(link_or_image)
                            });
                            self.position = next;
                        }
                    }
                }

                if let Some(inline) = inline {
                    self.inlines.truncate(d.text_node);
                    self.inlines.push(inline);
                    if !is_image {
                        for earlier in &mut self.delimiter_stack[..idx] {
                            if earlier.kind == DelimiterType::LinkOpening {
                                earlier.is_inactive = true;
                            }
                        }
                    }
                    self.delimiter_stack.remove(idx);
                    return;
                }
                self.delimiter_stack.remove(idx);
            }
        }
        let abs = self.abs(self.position);
        self.push_text(Cow::Borrowed("]"), Range::new(abs, abs + 1));
        self.position += 1;
    }

    /// Port of `parseCharacterReference` (inlines.go:412). The emitted `Text.range` spans the
    /// *decoded* length, not the source `&...;`.
    fn parse_character_reference(&mut self) {
        let abs = self.abs(self.position);
        self.position += 1;
        let p = self.position;
        match self.raw[p..].find(';') {
            None => self.push_text(Cow::Borrowed("&"), Range::new(abs, abs + 1)),
            Some(semicolon) => {
                let s = character_reference(&self.raw[p..p + semicolon]);
                if !s.is_empty() {
                    self.position += semicolon + 1;
                    let len = s.len();
                    self.push_text(Cow::Owned(s), Range::new(abs, abs + len));
                } else {
                    self.push_text(Cow::Borrowed("&"), Range::new(abs, abs + 1));
                }
            }
        }
    }

    /// Port of `parseAutolink` (inlines.go:436) for the `:` or `w`/`W` at the current position.
    /// Refused outright while any delimiter on the stack is active (no autolinks inside an open
    /// `[`). For `:`, the scheme has already been emitted as text, so the previous text node is
    /// cut back by the scheme's length and the position rewound.
    fn parse_autolink(&mut self, c: u8) -> bool {
        if self.delimiter_stack.iter().any(|d| !d.is_inactive) {
            return false;
        }

        let link = match c {
            b':' => {
                let Some(link) = parse_url_autolink(&self.raw, self.position) else {
                    return false;
                };
                // Since the current position is at the colon, rewind so the scheme is not
                // duplicated.
                if let Some(rewind) = self.raw[link.position..link.end].find(':') {
                    let Some(Inline::Text(last_text)) = self.inlines.last() else {
                        // Go: never occurs, since the scheme was emitted by parseText.
                        return false;
                    };
                    let keep = match last_text.text.len().checked_sub(rewind) {
                        Some(keep) => keep,
                        None => return false, // Go would panic; unreachable, see above.
                    };
                    let text = match &last_text.text {
                        Cow::Borrowed(s) => match s.get(..keep) {
                            Some(prefix) => Cow::Borrowed(prefix),
                            None => Cow::Owned(String::new()),
                        },
                        Cow::Owned(s) => Cow::Owned(s.get(..keep).unwrap_or("").to_owned()),
                    };
                    let range = Range::new(
                        last_text.range.position,
                        last_text.range.end.saturating_sub(rewind),
                    );
                    self.inlines.pop();
                    self.push_text(text, range);
                    self.position -= rewind;
                }
                link
            }
            b'w' | b'W' => {
                let Some(link) = parse_www_autolink(&self.raw, self.position) else {
                    return false;
                };
                link
            }
            _ => return false,
        };

        let link_markdown_position = self.abs(link.position);
        let link_range = Range::new(
            link_markdown_position,
            link_markdown_position + link.end - link.position,
        );
        let text = self.cow(link.position, link.end);
        self.inlines.push(Inline::Autolink(Autolink {
            markdown: self.markdown,
            children: vec![Inline::Text(Text {
                text,
                range: link_range,
            })],
            raw_destination: link_range,
        }));
        self.position += link.end - link.position;
        true
    }

    /// Port of `parseEmoji` (emoji.go:18) at the parser level: emit the node on a match.
    fn parse_emoji_node(&mut self) -> bool {
        let Some((name, matched)) = parse_emoji(&self.raw, self.position) else {
            return false;
        };
        let start = self.position + 1;
        let end = start + name.len();
        let name = self.cow(start, end);
        self.inlines.push(Inline::Emoji(Emoji { name }));
        self.position += matched;
        true
    }

    /// Port of `Parse` (inlines.go:512). Dispatches on the byte at the position; every special
    /// character is ASCII, so a multi-byte character always falls to `parse_text`.
    fn parse(mut self) -> Vec<Inline<'a>> {
        while self.position < self.raw.len() {
            let c = self.raw.as_bytes()[self.position];
            match c {
                b'\r' | b'\n' => self.parse_line_ending(),
                b'\\' => self.parse_escape_character(),
                b'`' => self.parse_backticks(),
                b'&' => self.parse_character_reference(),
                b'!' | b'[' => self.parse_link_or_image_delimiter(),
                b']' => self.look_for_link_or_image(),
                b'w' | b'W' => {
                    if !self.parse_autolink(c) {
                        self.parse_text();
                    }
                }
                b':' => {
                    if self.parse_autolink(c) {
                        continue;
                    }
                    if self.parse_emoji_node() {
                        continue;
                    }
                    self.parse_text();
                }
                _ => self.parse_text(),
            }
        }
        self.inlines
    }
}

/// Port of `markdown.ParseInlines` (inlines.go:552): the inlines of the text in `ranges`,
/// resolving reference links against `reference_definitions`. Not merged; see
/// [`merge_inline_text`].
pub fn parse_inlines<'a>(
    markdown: &'a str,
    ranges: &[Range],
    reference_definitions: &[ReferenceDefinition<'a>],
) -> Vec<Inline<'a>> {
    InlineParser::new(markdown, ranges, reference_definitions).parse()
}

/// Port of `markdown.MergeInlineText` (inlines.go:556): joins consecutive top-level `Text`
/// nodes whose ranges are adjacent (`prev.range.end == next.range.position`). Nothing below the
/// top level is touched, and two texts with a gap between their ranges — an entity's decoded
/// range, an escaped character — stay separate.
pub fn merge_inline_text(inlines: Vec<Inline<'_>>) -> Vec<Inline<'_>> {
    let mut ret: Vec<Inline<'_>> = Vec::with_capacity(inlines.len());
    for (i, v) in inlines.into_iter().enumerate() {
        // always add first node
        if i == 0 {
            ret.push(v);
            continue;
        }
        // not a text node? nothing to merge
        let text = match v {
            Inline::Text(text) => text,
            other => {
                ret.push(other);
                continue;
            }
        };
        // previous node is not a text node, or not right before this one? nothing to merge
        match ret.last_mut() {
            Some(Inline::Text(prev)) if prev.range.end == text.range.position => {
                prev.text.to_mut().push_str(&text.text);
                prev.range.end = text.range.end;
            }
            _ => ret.push(Inline::Text(text)),
        }
    }
    ret
}

/// Port of `markdown.CharacterReference` (inlines.go:365): the replacement for the text between
/// `&` and `;`, or `""` when it is not a reference. Numeric forms take at most eight digits;
/// zero, surrogates and anything past U+10FFFF become U+FFFD. Names are case-sensitive.
pub fn character_reference(reference: &str) -> String {
    let bytes = reference.as_bytes();
    if bytes.is_empty() {
        return String::new();
    }
    if bytes[0] == b'#' {
        if bytes.len() < 2 {
            return String::new();
        }
        let mut n: u64 = 0;
        if bytes[1] == b'X' || bytes[1] == b'x' {
            if bytes.len() < 3 {
                return String::new();
            }
            for (i, &d) in bytes.iter().enumerate().skip(2) {
                if i > 9 {
                    return String::new();
                }
                let v = match d {
                    b'0'..=b'9' => d - b'0',
                    b'a'..=b'f' => d - b'a' + 10,
                    b'A'..=b'F' => d - b'A' + 10,
                    _ => return String::new(),
                };
                n = n * 16 + u64::from(v);
            }
        } else {
            for (i, &d) in bytes.iter().enumerate().skip(1) {
                if i > 8 || !d.is_ascii_digit() {
                    return String::new();
                }
                n = n * 10 + u64::from(d - b'0');
            }
        }
        // Go: `c := rune(n)` truncates to int32, then `c == 0 || !utf8.ValidRune(c)` yields the
        // replacement character. Every value >= 0x80000000 is negative as int32 and invalid; so
        // is anything above U+10FFFF or in the surrogate range; `char::from_u32` refuses all of
        // them, and `n` never exceeds eight hex digits so the u32 conversion is lossless.
        return match u32::try_from(n).ok().and_then(char::from_u32) {
            Some(c) if c != '\0' => c.to_string(),
            _ => '\u{FFFD}'.to_string(),
        };
    }
    html_entities::lookup(reference).unwrap_or("").to_owned()
}

/// Port of `markdown.Unescape` (inlines.go:585): resolves backslash escapes of escapable bytes
/// and `&...;` character references; everything else passes through.
pub fn unescape(markdown: &str) -> String {
    let bytes = markdown.as_bytes();
    let mut ret = String::with_capacity(markdown.len());
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'\\' => {
                if position + 1 < bytes.len() && is_escapable_byte(bytes[position + 1]) {
                    ret.push(bytes[position + 1] as char);
                    position += 2;
                } else {
                    ret.push('\\');
                    position += 1;
                }
            }
            b'&' => {
                position += 1;
                match markdown[position..].find(';') {
                    None => ret.push('&'),
                    Some(semicolon) => {
                        let s = character_reference(&markdown[position..position + semicolon]);
                        if !s.is_empty() {
                            position += semicolon + 1;
                            ret.push_str(&s);
                        } else {
                            ret.push('&');
                        }
                    }
                }
            }
            _ => {
                let Some(c) = markdown[position..].chars().next() else {
                    break;
                };
                ret.push(c);
                position += c.len_utf8();
            }
        }
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(md: &str) -> Vec<Inline<'_>> {
        parse_inlines(md, &[Range::new(0, md.len())], &[])
    }

    fn text<'a>(inline: &'a Inline<'_>) -> &'a Text<'a> {
        match inline {
            Inline::Text(t) => t,
            other => panic!("not text: {other:?}"),
        }
    }

    #[test]
    fn character_reference_branches() {
        assert_eq!(character_reference(""), "");
        assert_eq!(character_reference("#"), "");
        assert_eq!(character_reference("#x"), "");
        assert_eq!(character_reference("#65"), "A");
        assert_eq!(character_reference("#x41"), "A");
        assert_eq!(character_reference("#X41"), "A");
        assert_eq!(character_reference("#0"), "\u{FFFD}");
        assert_eq!(character_reference("#xD800"), "\u{FFFD}");
        assert_eq!(character_reference("#x110000"), "\u{FFFD}");
        assert_eq!(character_reference("#x10FFFF"), "\u{10FFFF}");
        assert_eq!(character_reference("#99999999"), "\u{FFFD}");
        assert_eq!(character_reference("#123456789"), "");
        assert_eq!(character_reference("#x123456789"), "");
        assert_eq!(character_reference("#xFFFFFFFF"), "\u{FFFD}");
        assert_eq!(character_reference("#12a"), "");
        assert_eq!(character_reference("#xg"), "");
        assert_eq!(character_reference("amp"), "&");
        assert_eq!(character_reference("AMP"), "&");
        assert_eq!(character_reference("Amp"), "");
        assert_eq!(character_reference("nosuch"), "");
        assert_eq!(character_reference("fjlig"), "fj");
    }

    #[test]
    fn unescape_branches() {
        assert_eq!(unescape("a\\*b"), "a*b");
        assert_eq!(unescape("a\\ab"), "a\\ab");
        assert_eq!(unescape("a\\"), "a\\");
        assert_eq!(unescape("&amp;&"), "&&");
        assert_eq!(unescape("&nosuch;"), "&nosuch;");
        assert_eq!(unescape("&#65;é&"), "Aé&");
        assert_eq!(unescape("&amp"), "&amp");
    }

    #[test]
    fn text_ranges_are_go_ranges() {
        // The final run keeps its untrimmed length in the range but not in the text.
        let inlines = parse("ab  ");
        assert_eq!(inlines.len(), 1);
        assert_eq!(text(&inlines[0]).text, "ab");
        assert_eq!(text(&inlines[0]).range, Range::new(0, 4));

        // A run before a newline is trimmed in both.
        let inlines = parse("ab  \ncd");
        assert_eq!(text(&inlines[0]).range, Range::new(0, 2));
        assert!(matches!(inlines[1], Inline::HardLineBreak));
        assert_eq!(text(&inlines[2]).range, Range::new(5, 7));

        // An entity's range is its decoded length.
        let inlines = parse("&#65;&#66;");
        assert_eq!(text(&inlines[0]).range, Range::new(0, 1));
        assert_eq!(text(&inlines[1]).range, Range::new(5, 6));
        assert_eq!(merge_inline_text(inlines).len(), 2);
    }

    #[test]
    fn line_endings() {
        assert!(matches!(parse("a\nb")[1], Inline::SoftLineBreak));
        assert!(matches!(parse("a \nb")[1], Inline::HardLineBreak));
        assert!(matches!(parse("a\t\nb")[1], Inline::HardLineBreak));
        assert!(matches!(parse(" \nb")[1], Inline::SoftLineBreak));
        let crlf = parse("a\r\nb");
        assert_eq!(crlf.len(), 3);
        assert_eq!(text(&crlf[2]).range, Range::new(3, 4));
    }

    #[test]
    fn merge_joins_only_adjacent_top_level_text() {
        // The escaped `*` is at range 2..3, so it is not adjacent to "a" (0..1) and stays
        // separate; it is adjacent to "b" (3..4) and joins it.
        let inlines = parse("a\\*b");
        assert_eq!(inlines.len(), 3);
        let merged = merge_inline_text(inlines);
        assert_eq!(merged.len(), 2);
        assert_eq!(text(&merged[0]).text, "a");
        assert_eq!(text(&merged[1]).text, "*b");
        assert_eq!(text(&merged[1]).range, Range::new(2, 4));

        let inlines = parse("a]b");
        let merged = merge_inline_text(inlines);
        assert_eq!(merged.len(), 1);
        assert_eq!(text(&merged[0]).text, "a]b");
        assert_eq!(text(&merged[0]).range, Range::new(0, 3));

        assert!(merge_inline_text(Vec::new()).is_empty());
    }

    #[test]
    fn backticks() {
        let inlines = parse("``a`b`` c");
        assert!(matches!(&inlines[0], Inline::CodeSpan(c) if c.code == "a`b"));
        let inlines = parse("`unbalanced");
        assert_eq!(text(&inlines[0]).text, "`");
        let inlines = parse("` a  b `");
        assert!(matches!(&inlines[0], Inline::CodeSpan(c) if c.code == "a b"));
    }

    #[test]
    fn links_nest_by_kind() {
        // "[", "a ", the link, " d", "]", "(e)": the outer opener was deactivated by the inner link.
        let inlines = parse("[a [b](c) d](e)");
        assert_eq!(inlines.len(), 6);
        assert!(matches!(inlines[2], Inline::InlineLink(_)));
        let inlines = parse("[![a](b)](c)");
        assert!(
            matches!(&inlines[0], Inline::InlineLink(l) if matches!(l.children[0], Inline::InlineImage(_)))
        );
        let inlines = parse("[a](b \"t\")");
        assert!(
            matches!(&inlines[0], Inline::InlineLink(l) if l.destination() == "b" && l.title() == "t")
        );
    }

    #[test]
    fn autolink_rewinds_the_scheme() {
        let inlines = parse("see http://a.b/x then");
        assert_eq!(inlines.len(), 3);
        assert_eq!(text(&inlines[0]).text, "see ");
        assert_eq!(text(&inlines[0]).range, Range::new(0, 4));
        assert!(matches!(&inlines[1], Inline::Autolink(a) if a.destination() == "http://a.b/x"));
        assert_eq!(text(&inlines[2]).text, " then");
        // A `w` in plain text is read one byte at a time when it starts no link.
        let inlines = parse("a www");
        assert_eq!(inlines.len(), 4);
        let inlines = parse("www.a.b");
        assert!(matches!(&inlines[0], Inline::Autolink(a) if a.destination() == "http://www.a.b"));
        // Inside an open bracket, nothing links.
        let inlines = parse("[www.a.b");
        assert!(inlines.iter().all(|i| matches!(i, Inline::Text(_))));
    }

    #[test]
    fn deep_image_nesting_drops_without_recursion() {
        let depth = 50_000;
        let md = format!("{}a{}", "![".repeat(depth), "](u)".repeat(depth));
        let inlines = parse(&md);
        assert_eq!(inlines.len(), 1);
        drop(inlines);
    }
}
