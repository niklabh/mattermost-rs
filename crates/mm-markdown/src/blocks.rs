//! Port of `blocks.go` and the block files it dispatches to (`document.go`, `paragraph.go`,
//! `block_quote.go`, `list.go`, `fenced_code.go`, `indented_code.go`): the block types and the
//! line-by-line block parser.
//!
//! Go keeps a slice of open `Block` pointers into a tree it mutates in place. Here the parser
//! builds nodes in an arena indexed by `usize` — the open-block slice becomes a slice of indices
//! and every `AddChild`/`Close`/`Continuation` is a method on the arena — and turns the arena
//! into the owned [`Document`] tree at the end. The control flow of `ParseBlocks` is kept line
//! for line; the arena is only how pointers are spelled.

use crate::inlines::{Inline, parse_inlines};
use crate::lines::parse_lines;
use crate::markdown::{Range, count_indentation, is_blank, trim_left_space, trim_right_space};
use crate::reference_definition::{ReferenceDefinition, parse_reference_definition};

/// Port of `maxNestingDepth` (blocks.go:10, MM-69445): block quotes and lists stop nesting at
/// this depth of open blocks.
pub const MAX_NESTING_DEPTH: usize = 32;

/// Port of the `markdown.Block` interface (blocks.go:19) and its seven implementations.
///
/// `Document` is a variant so that [`crate::inspect`] can hand the root to the callback the way
/// Go's `Inspect` does; `parse` returns the [`Document`] itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Block<'a> {
    Document(Document<'a>),
    Paragraph(Paragraph<'a>),
    List(List<'a>),
    ListItem(ListItem<'a>),
    BlockQuote(BlockQuote<'a>),
    FencedCode(FencedCode<'a>),
    IndentedCode(IndentedCode<'a>),
}

impl<'a> Block<'a> {
    /// The children `InspectBlock` (inspect.go:66) descends into. A `List`'s children are its
    /// items; a paragraph's inlines are not blocks and are not here.
    pub fn children(&self) -> &[Block<'a>] {
        match self {
            Block::Document(d) => &d.children,
            Block::List(l) => &l.children,
            Block::ListItem(l) => &l.children,
            Block::BlockQuote(b) => &b.children,
            Block::Paragraph(_) | Block::FencedCode(_) | Block::IndentedCode(_) => &[],
        }
    }
}

/// Port of `markdown.Document` (document.go:7).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Document<'a> {
    pub children: Vec<Block<'a>>,
}

/// Port of `markdown.Paragraph` (paragraph.go:12). `text` is the line ranges (leading
/// indentation and container prefixes already removed, line endings kept, the last line
/// right-trimmed); `reference_definitions` are the definitions `Close` peeled off its front.
#[derive(Debug, PartialEq, Eq)]
pub struct Paragraph<'a> {
    markdown: &'a str,
    pub text: Vec<Range>,
    pub reference_definitions: Vec<ReferenceDefinition<'a>>,
}

impl<'a> Paragraph<'a> {
    /// Port of `ParseInlines` (paragraph.go:20): the paragraph's inlines, unmerged.
    pub fn parse_inlines(
        &self,
        reference_definitions: &[ReferenceDefinition<'a>],
    ) -> Vec<Inline<'a>> {
        parse_inlines(self.markdown, &self.text, reference_definitions)
    }
}

/// Port of `markdown.BlockQuote` (block_quote.go:6).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BlockQuote<'a> {
    pub children: Vec<Block<'a>>,
}

/// Port of `markdown.List` (list.go:76). `children` are always [`Block::ListItem`]; the Go
/// field is typed `[]*ListItem`, and it is a `Vec<Block>` here only so that the inspector can
/// hand an item to the callback as a block.
#[derive(Debug, PartialEq, Eq)]
pub struct List<'a> {
    pub is_loose: bool,
    pub is_ordered: bool,
    /// The number before the `.` or `)`; 0 for a bullet list.
    pub ordered_start: i64,
    /// `-`, `+` or `*` for a bullet list; `.` or `)` for an ordered one.
    pub bullet_or_delimiter: u8,
    pub children: Vec<Block<'a>>,
}

/// Port of `markdown.ListItem` (list.go:10). `indentation` is the column a continuation line
/// must reach: the marker's own indentation plus its width plus the spaces consumed after it.
#[derive(Debug, PartialEq, Eq)]
pub struct ListItem<'a> {
    pub indentation: usize,
    pub children: Vec<Block<'a>>,
}

/// Port of `markdown.FencedCodeLine` (fenced_code.go:9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FencedCodeLine {
    pub indentation: usize,
    pub range: Range,
}

/// Port of `markdown.FencedCode` (fenced_code.go:14).
#[derive(Debug, PartialEq, Eq)]
pub struct FencedCode<'a> {
    markdown: &'a str,
    pub indentation: usize,
    pub opening_fence: Range,
    pub raw_info: Range,
    pub raw_code: Vec<FencedCodeLine>,
}

impl FencedCode<'_> {
    /// Port of `Code()` (fenced_code.go:25): each line re-indented by `indentation` spaces
    /// (tabs having been counted as four), line endings included.
    pub fn code(&self) -> String {
        let mut result = String::new();
        for line in &self.raw_code {
            result.extend(std::iter::repeat_n(' ', line.indentation));
            result.push_str(line.range.slice(self.markdown));
        }
        result
    }

    /// Port of `Info()` (fenced_code.go:33): the unescaped info string, `""` when absent.
    pub fn info(&self) -> String {
        crate::inlines::unescape(self.raw_info.slice(self.markdown))
    }
}

/// Port of `markdown.IndentedCodeLine` (indented_code.go:9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndentedCodeLine {
    pub indentation: usize,
    pub range: Range,
}

/// Port of `markdown.IndentedCode` (indented_code.go:14).
#[derive(Debug, PartialEq, Eq)]
pub struct IndentedCode<'a> {
    markdown: &'a str,
    pub raw_code: Vec<IndentedCodeLine>,
}

impl IndentedCode<'_> {
    /// Port of `Code()` (indented_code.go:21).
    pub fn code(&self) -> String {
        let mut result = String::new();
        for line in &self.raw_code {
            result.extend(std::iter::repeat_n(' ', line.indentation));
            result.push_str(line.range.slice(self.markdown));
        }
        result
    }
}

/// Port of `continuation` (blocks.go:12).
struct Continuation {
    indentation: usize,
    remaining: Range,
}

/// An open block under construction — the arena form of the Go structs, including their
/// unexported bookkeeping fields.
enum Open<'a> {
    Document {
        children: Vec<usize>,
    },
    Paragraph {
        text: Vec<Range>,
        reference_definitions: Vec<ReferenceDefinition<'a>>,
    },
    BlockQuote {
        children: Vec<usize>,
    },
    List {
        has_trailing_blank_line: bool,
        has_blank_line_between_children: bool,
        is_loose: bool,
        is_ordered: bool,
        ordered_start: i64,
        bullet_or_delimiter: u8,
        children: Vec<usize>,
    },
    ListItem {
        has_trailing_blank_line: bool,
        has_blank_line_between_children: bool,
        indentation: usize,
        children: Vec<usize>,
    },
    FencedCode {
        did_see_closing_fence: bool,
        indentation: usize,
        opening_fence: Range,
        raw_info: Range,
        raw_code: Vec<FencedCodeLine>,
    },
    IndentedCode {
        raw_code: Vec<IndentedCodeLine>,
    },
    /// A node already moved into the output tree.
    Taken,
}

struct BlockParser<'a> {
    markdown: &'a str,
    arena: Vec<Open<'a>>,
    reference_definitions: Vec<ReferenceDefinition<'a>>,
}

impl<'a> BlockParser<'a> {
    fn push(&mut self, node: Open<'a>) -> usize {
        self.arena.push(node);
        self.arena.len() - 1
    }

    fn is_paragraph(&self, idx: usize) -> bool {
        matches!(self.arena[idx], Open::Paragraph { .. })
    }

    fn is_list(&self, idx: usize) -> bool {
        matches!(self.arena[idx], Open::List { .. })
    }

    fn is_list_item(&self, idx: usize) -> bool {
        matches!(self.arena[idx], Open::ListItem { .. })
    }

    /// The `Continuation` method of each block type.
    fn continuation(&self, idx: usize, indentation: usize, r: Range) -> Option<Continuation> {
        let s = r.slice(self.markdown);
        match &self.arena[idx] {
            // document.go:13
            Open::Document { .. } => Some(Continuation {
                indentation,
                remaining: r,
            }),
            // paragraph.go:24
            Open::Paragraph { .. } => {
                if is_blank(s) {
                    return None;
                }
                Some(Continuation {
                    indentation,
                    remaining: r,
                })
            }
            // block_quote.go:13
            Open::BlockQuote { .. } => {
                if indentation > 3 {
                    return None;
                }
                if s.is_empty() || s.as_bytes()[0] != b'>' {
                    return None;
                }
                let remaining = Range::new(r.position + 1, r.end);
                let (indentation, indentation_bytes) = count_indentation(self.markdown, remaining);
                let indentation = indentation.saturating_sub(1);
                Some(Continuation {
                    indentation,
                    remaining: Range::new(remaining.position + indentation_bytes, remaining.end),
                })
            }
            // list.go:88: a list continues on every line; blank ones carry no indentation.
            Open::List { .. } => {
                if is_blank(s) {
                    return Some(Continuation {
                        indentation: 0,
                        remaining: r,
                    });
                }
                Some(Continuation {
                    indentation,
                    remaining: r,
                })
            }
            // list.go:20: a blank line continues an item only if it has content; otherwise the
            // line must be indented to the item's content column.
            Open::ListItem {
                indentation: item_indentation,
                children,
                ..
            } => {
                if is_blank(s) {
                    if children.is_empty() {
                        return None;
                    }
                    return Some(Continuation {
                        indentation: 0,
                        remaining: r,
                    });
                }
                if indentation < *item_indentation {
                    return None;
                }
                Some(Continuation {
                    indentation: indentation - item_indentation,
                    remaining: r,
                })
            }
            // fenced_code.go:37
            Open::FencedCode {
                did_see_closing_fence,
                ..
            } => {
                if *did_see_closing_fence {
                    return None;
                }
                Some(Continuation {
                    indentation,
                    remaining: r,
                })
            }
            // indented_code.go:31
            Open::IndentedCode { .. } => {
                if indentation >= 4 {
                    return Some(Continuation {
                        indentation: indentation - 4,
                        remaining: r,
                    });
                }
                if is_blank(s) {
                    return Some(Continuation {
                        indentation: 0,
                        remaining: r,
                    });
                }
                None
            }
            Open::Taken => None,
        }
    }

    /// The `AddLine` method: only the two code blocks take lines; lists and items only record
    /// that they saw a blank one.
    fn add_line(&mut self, idx: usize, indentation: usize, r: Range) -> bool {
        let markdown = self.markdown;
        let s = r.slice(markdown);
        match &mut self.arena[idx] {
            // list.go:50 and list.go:112
            Open::List {
                has_trailing_blank_line,
                ..
            }
            | Open::ListItem {
                has_trailing_blank_line,
                ..
            } => {
                if is_blank(s) {
                    *has_trailing_blank_line = true;
                }
                false
            }
            // fenced_code.go:47: a closing fence — at most three columns of indentation, the
            // opening fence as a prefix, and nothing but more fence characters after it — closes
            // the block. Anything else is a code line, re-indented relative to the opening fence.
            Open::FencedCode {
                did_see_closing_fence,
                indentation: fence_indentation,
                opening_fence,
                raw_code,
                ..
            } => {
                let fence = opening_fence.slice(markdown);
                if indentation <= 3 && s.starts_with(fence) {
                    let suffix = s[fence.len()..].trim();
                    let fence_char = s.as_bytes()[0] as char;
                    if suffix.chars().all(|c| c == fence_char) {
                        *did_see_closing_fence = true;
                        return true;
                    }
                }
                let indentation = indentation.saturating_sub(*fence_indentation);
                raw_code.push(FencedCodeLine {
                    indentation,
                    range: r,
                });
                true
            }
            // indented_code.go:45
            Open::IndentedCode { raw_code } => {
                raw_code.push(IndentedCodeLine {
                    indentation,
                    range: r,
                });
                true
            }
            _ => false,
        }
    }

    /// The `Close` method (paragraph.go:32, list.go:141, indented_code.go:53).
    fn close(&mut self, idx: usize) {
        let markdown = self.markdown;
        if self.is_list(idx) {
            let is_loose = self.list_is_loose(idx);
            if let Open::List { is_loose: slot, .. } = &mut self.arena[idx] {
                *slot = is_loose;
            }
            return;
        }
        match &mut self.arena[idx] {
            Open::Paragraph {
                text,
                reference_definitions,
            } => {
                loop {
                    for range in text.iter_mut() {
                        *range = trim_left_space(markdown, *range);
                        if range.position < range.end {
                            break;
                        }
                    }

                    let first = text.first().copied();
                    let starts_definition = match first {
                        None => false,
                        Some(f) => !(f.position < f.end && markdown.as_bytes()[f.position] != b'['),
                    };
                    if !starts_definition {
                        break;
                    }

                    match parse_reference_definition(markdown, text) {
                        None => break,
                        Some((definition, remaining)) => {
                            reference_definitions.push(definition);
                            *text = remaining;
                        }
                    }
                }

                for range in text.iter_mut().rev() {
                    *range = trim_right_space(markdown, *range);
                    if range.position < range.end {
                        break;
                    }
                }
            }
            Open::IndentedCode { raw_code } => {
                // Go indexes the last element unguarded; the block always starts with a
                // non-blank line, so the loop stops before emptying the vector.
                while let Some(last) = raw_code.last() {
                    let s = last.range.slice(markdown);
                    if s.trim_end_matches(['\r', '\n']).is_empty() {
                        raw_code.pop();
                    } else {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    /// `AllowsBlockStarts`: false for the two code blocks.
    fn allows_block_starts(&self, idx: usize) -> bool {
        !matches!(
            self.arena[idx],
            Open::FencedCode { .. } | Open::IndentedCode { .. }
        )
    }

    /// `HasTrailingBlankLine` (list.go:57, list.go:119): the block's own flag, or its last
    /// child's. Recursion depth is bounded by `MAX_NESTING_DEPTH`.
    fn has_trailing_blank_line(&self, idx: usize) -> bool {
        match &self.arena[idx] {
            Open::List {
                has_trailing_blank_line,
                children,
                ..
            }
            | Open::ListItem {
                has_trailing_blank_line,
                children,
                ..
            } => {
                *has_trailing_blank_line
                    || children
                        .last()
                        .is_some_and(|&last| self.has_trailing_blank_line(last))
            }
            _ => false,
        }
    }

    /// `ListItem.isLoose` (list.go:61).
    fn item_is_loose(&self, idx: usize) -> bool {
        let Open::ListItem {
            has_blank_line_between_children,
            children,
            ..
        } = &self.arena[idx]
        else {
            return false;
        };
        if *has_blank_line_between_children {
            return true;
        }
        let n = children.len();
        children
            .iter()
            .enumerate()
            .any(|(i, &child)| i + 1 < n && self.has_trailing_blank_line(child))
    }

    /// `List.isLoose` (list.go:129).
    fn list_is_loose(&self, idx: usize) -> bool {
        let Open::List {
            has_blank_line_between_children,
            children,
            ..
        } = &self.arena[idx]
        else {
            return false;
        };
        if *has_blank_line_between_children {
            return true;
        }
        let n = children.len();
        children.iter().enumerate().any(|(i, &child)| {
            self.item_is_loose(child) || (i + 1 < n && self.has_trailing_blank_line(child))
        })
    }

    /// `AddChild` for the container blocks. Returns the offset into `new_blocks` from which the
    /// blocks were adopted (0, or 1 when a one-item list of the same kind was merged into this
    /// list — `List.AddChild`, list.go:99 — and the new list itself dropped), or `None` when
    /// this block does not take that child.
    fn add_child(&mut self, idx: usize, new_blocks: &[usize]) -> Option<usize> {
        let &first = new_blocks.first()?;
        if self.is_list(idx) {
            return self.list_add_child(idx, new_blocks);
        }
        match &mut self.arena[idx] {
            Open::Document { children } | Open::BlockQuote { children } => {
                children.push(first);
                Some(0)
            }
            Open::ListItem {
                children,
                has_trailing_blank_line,
                has_blank_line_between_children,
                ..
            } => {
                children.push(first);
                if *has_trailing_blank_line {
                    *has_blank_line_between_children = true;
                }
                *has_trailing_blank_line = false;
                Some(0)
            }
            _ => None,
        }
    }

    /// `List.AddChild` (list.go:99): takes an item directly, or the single item of a new list
    /// of the same kind (dropping that list), and nothing else.
    fn list_add_child(&mut self, idx: usize, new_blocks: &[usize]) -> Option<usize> {
        let &first = new_blocks.first()?;
        if self.is_list_item(first) {
            if let Open::List {
                children,
                has_trailing_blank_line,
                has_blank_line_between_children,
                ..
            } = &mut self.arena[idx]
            {
                children.push(first);
                if *has_trailing_blank_line {
                    *has_blank_line_between_children = true;
                }
                *has_trailing_blank_line = false;
            }
            return Some(0);
        }
        if self.is_list(first) {
            let (
                Open::List {
                    is_ordered: a_ordered,
                    bullet_or_delimiter: a_bullet,
                    ..
                },
                Open::List {
                    is_ordered: b_ordered,
                    bullet_or_delimiter: b_bullet,
                    children: b_children,
                    ..
                },
            ) = (&self.arena[idx], &self.arena[first])
            else {
                return None;
            };
            if b_children.len() == 1 && a_ordered == b_ordered && a_bullet == b_bullet {
                return self
                    .list_add_child(idx, &new_blocks[1..])
                    .map(|offset| offset + 1);
            }
        }
        None
    }

    fn is_container(&self, idx: usize) -> bool {
        matches!(
            self.arena[idx],
            Open::Document { .. }
                | Open::BlockQuote { .. }
                | Open::List { .. }
                | Open::ListItem { .. }
        )
    }

    /// `closeBlocks` (blocks.go:42): closes each block and collects the reference definitions of
    /// closed paragraphs, in order.
    fn close_blocks(&mut self, blocks: &[usize]) {
        for &idx in blocks {
            self.close(idx);
            if let Open::Paragraph {
                reference_definitions,
                ..
            } = &self.arena[idx]
            {
                if !reference_definitions.is_empty() {
                    // Go appends the same pointers to both lists; a clone here is the same
                    // sharing spelled for values.
                    self.reference_definitions
                        .extend(reference_definitions.iter().cloned());
                }
            }
        }
    }

    /// `newParagraph` (paragraph.go:62): `None` for a blank line.
    fn new_paragraph(&mut self, r: Range) -> Option<usize> {
        if is_blank(r.slice(self.markdown)) {
            return None;
        }
        Some(self.push(Open::Paragraph {
            text: vec![r],
            reference_definitions: Vec::new(),
        }))
    }

    /// `blockStart` (blocks.go:127). `matched_last` and `unmatched_last` are the last elements
    /// of Go's `matchedBlocks` and `unmatchedBlocks` slices — the only parts the starts read.
    fn block_start(
        &mut self,
        indentation: usize,
        r: Range,
        matched_last: Option<usize>,
        unmatched_last: Option<usize>,
        depth: usize,
    ) -> Option<Vec<usize>> {
        if r.position >= r.end {
            return None;
        }
        if let Some(start) = self.block_quote_start(indentation, r, depth) {
            return Some(start);
        }
        if let Some(start) = self.list_start(indentation, r, matched_last, unmatched_last, depth) {
            return Some(start);
        }
        if let Some(start) = self.indented_code_start(indentation, r, matched_last, unmatched_last)
        {
            return Some(start);
        }
        if let Some(start) = self.fenced_code_start(indentation, r) {
            return Some(start);
        }
        None
    }

    /// `blockStartOrParagraph` (blocks.go:146).
    fn block_start_or_paragraph(
        &mut self,
        indentation: usize,
        r: Range,
        depth: usize,
    ) -> Option<Vec<usize>> {
        if let Some(start) = self.block_start(indentation, r, None, None, depth) {
            return Some(start);
        }
        self.new_paragraph(r).map(|p| vec![p])
    }

    /// `blockQuoteStart` (block_quote.go:38). One space after the `>` is consumed; the rest of
    /// the indentation is counted and handed to the child start **as is** (the continuation
    /// path subtracts one column instead — they are not the same arithmetic).
    fn block_quote_start(&mut self, indent: usize, r: Range, depth: usize) -> Option<Vec<usize>> {
        if indent > 3 {
            return None;
        }
        if depth >= MAX_NESTING_DEPTH {
            return None;
        }
        let s = r.slice(self.markdown);
        if s.is_empty() || s.as_bytes()[0] != b'>' {
            return None;
        }

        let block = self.push(Open::BlockQuote {
            children: Vec::new(),
        });
        let mut position = r.position + 1;
        if s.len() > 1 && s.as_bytes()[1] == b' ' {
            position += 1;
        }
        let r = Range::new(position, r.end);
        let (indent, bytes) = count_indentation(self.markdown, r);

        let mut ret = vec![block];
        if let Some(descendants) =
            self.block_start_or_paragraph(indent, Range::new(r.position + bytes, r.end), depth + 1)
        {
            if let Open::BlockQuote { children } = &mut self.arena[block] {
                children.push(descendants[0]);
            }
            ret.extend(descendants);
        }
        Some(ret)
    }

    /// `listStart` (list.go:172).
    fn list_start(
        &mut self,
        indent: usize,
        r: Range,
        matched_last: Option<usize>,
        unmatched_last: Option<usize>,
        depth: usize,
    ) -> Option<Vec<usize>> {
        let after_list = matched_last.is_some_and(|idx| self.is_list(idx));
        if !after_list && indent > 3 {
            return None;
        }
        if depth >= MAX_NESTING_DEPTH {
            return None;
        }

        let marker = parse_list_marker(self.markdown, r)?;
        let remaining = marker.remaining;

        let is_blank_content = is_blank(remaining.slice(self.markdown));
        // A list item cannot interrupt a paragraph with an empty item or an ordered start
        // other than 1.
        if let (Some(last), None) = (matched_last, unmatched_last) {
            if self.is_paragraph(last)
                && (is_blank_content || (marker.is_ordered && marker.ordered_start != 1))
            {
                return None;
            }
        }

        let (indent_after_marker, indent_bytes_after_marker) =
            count_indentation(self.markdown, remaining);
        if !is_blank_content && indent_after_marker < 1 {
            return None;
        }

        let remaining = Range::new(
            remaining.position + indent_bytes_after_marker,
            remaining.end,
        );
        let consumed_indent_after_marker = if is_blank_content || indent_after_marker >= 5 {
            1
        } else {
            indent_after_marker
        };

        let list_item = self.push(Open::ListItem {
            has_trailing_blank_line: false,
            has_blank_line_between_children: false,
            indentation: indent + marker.width + consumed_indent_after_marker,
            children: Vec::new(),
        });
        let list = self.push(Open::List {
            has_trailing_blank_line: false,
            has_blank_line_between_children: false,
            is_loose: false,
            is_ordered: marker.is_ordered,
            ordered_start: marker.ordered_start,
            bullet_or_delimiter: marker.bullet_or_delimiter,
            children: vec![list_item],
        });
        let mut ret = vec![list, list_item];
        // Go's `indentAfterMarker-consumedIndentAfterMarker` is -1 for a blank item with nothing
        // after the marker; every block start refuses a blank line whatever the indentation, so
        // 0 is the same answer without the underflow.
        if let Some(descendants) = self.block_start_or_paragraph(
            indent_after_marker.saturating_sub(consumed_indent_after_marker),
            remaining,
            depth + 1,
        ) {
            if let Open::ListItem { children, .. } = &mut self.arena[list_item] {
                children.push(descendants[0]);
            }
            ret.extend(descendants);
        }
        Some(ret)
    }

    /// `indentedCodeStart` (indented_code.go:79): four columns of indentation, a non-blank
    /// line, and not directly after a paragraph (matched or unmatched — a lazy continuation
    /// wins).
    fn indented_code_start(
        &mut self,
        indentation: usize,
        r: Range,
        matched_last: Option<usize>,
        unmatched_last: Option<usize>,
    ) -> Option<Vec<usize>> {
        if let Some(last) = unmatched_last {
            if self.is_paragraph(last) {
                return None;
            }
        } else if let Some(last) = matched_last {
            if self.is_paragraph(last) {
                return None;
            }
        }

        if indentation < 4 {
            return None;
        }
        if is_blank(r.slice(self.markdown)) {
            return None;
        }

        let block = self.push(Open::IndentedCode {
            raw_code: vec![IndentedCodeLine {
                indentation: indentation - 4,
                range: r,
            }],
        });
        Some(vec![block])
    }

    /// `fencedCodeStart` (fenced_code.go:84): three or more backticks or tildes; a backtick
    /// anywhere in the rest of the line refuses the fence **for both fence characters**.
    fn fenced_code_start(&mut self, indentation: usize, r: Range) -> Option<Vec<usize>> {
        let s = r.slice(self.markdown);
        if !s.starts_with("```") && !s.starts_with("~~~") {
            return None;
        }

        let fence_character = s.as_bytes()[0];
        let mut fence_length = 3;
        for &c in &s.as_bytes()[3..] {
            if c == fence_character {
                fence_length += 1;
            } else {
                break;
            }
        }

        if s.as_bytes()[fence_length..].contains(&b'`') {
            return None;
        }

        let block = self.push(Open::FencedCode {
            did_see_closing_fence: false,
            indentation,
            opening_fence: Range::new(r.position, r.position + fence_length),
            raw_info: trim_right_space(self.markdown, Range::new(r.position + fence_length, r.end)),
            raw_code: Vec::new(),
        });
        Some(vec![block])
    }

    /// `ParseBlocks` (blocks.go:52), line for line.
    fn parse_blocks(&mut self, lines: &[Range]) -> usize {
        let markdown = self.markdown;
        let document = self.push(Open::Document {
            children: Vec::new(),
        });
        let mut open_blocks: Vec<usize> = vec![document];

        for &line in lines {
            let mut r = line;
            let mut last_match_index = 0;

            let (mut indentation, indentation_bytes) = count_indentation(markdown, r);
            r = Range::new(r.position + indentation_bytes, r.end);

            for (i, &open) in open_blocks.iter().enumerate() {
                if let Some(continuation) = self.continuation(open, indentation, r) {
                    indentation = continuation.indentation;
                    r = continuation.remaining;
                    let (additional_indentation, additional_indentation_bytes) =
                        count_indentation(markdown, r);
                    r = Range::new(r.position + additional_indentation_bytes, r.end);
                    indentation += additional_indentation;
                    last_match_index = i;
                } else {
                    break;
                }
            }

            if self.allows_block_starts(open_blocks[last_match_index]) {
                let matched_last = Some(open_blocks[last_match_index]);
                let unmatched_last = open_blocks
                    .get(last_match_index + 1..)
                    .and_then(|u| u.last().copied());
                if let Some(new_blocks) = self.block_start(
                    indentation,
                    r,
                    matched_last,
                    unmatched_last,
                    last_match_index + 1,
                ) {
                    let mut did_add = false;
                    for i in (0..=last_match_index).rev() {
                        if self.is_container(open_blocks[i]) {
                            if let Some(offset) = self.add_child(open_blocks[i], &new_blocks) {
                                let closing: Vec<usize> = open_blocks[i + 1..].to_vec();
                                self.close_blocks(&closing);
                                open_blocks.truncate(i + 1);
                                open_blocks.extend_from_slice(&new_blocks[offset..]);
                                did_add = true;
                                break;
                            }
                        }
                    }
                    if did_add {
                        continue;
                    }
                }
            }

            let line_is_blank = is_blank(r.slice(markdown));
            if let Some(&last) = open_blocks.last() {
                if !line_is_blank {
                    if let Open::Paragraph { text, .. } = &mut self.arena[last] {
                        text.push(r);
                        continue;
                    }
                }
            }

            let closing: Vec<usize> = open_blocks[last_match_index + 1..].to_vec();
            self.close_blocks(&closing);
            open_blocks.truncate(last_match_index + 1);

            if self.add_line(open_blocks[last_match_index], indentation, r) {
                continue;
            }

            if let Some(paragraph) = self.new_paragraph(r) {
                for i in (0..=last_match_index).rev() {
                    if self.is_container(open_blocks[i])
                        && self.add_child(open_blocks[i], &[paragraph]).is_some()
                    {
                        let closing: Vec<usize> = open_blocks[i + 1..].to_vec();
                        self.close_blocks(&closing);
                        open_blocks.truncate(i + 1);
                        open_blocks.push(paragraph);
                        break;
                    }
                }
            }
        }

        self.close_blocks(&open_blocks);
        document
    }

    /// Moves a finished arena node into the owned tree. Recursion depth is the container
    /// nesting depth, bounded by `MAX_NESTING_DEPTH`.
    fn take(&mut self, idx: usize) -> Block<'a> {
        let markdown = self.markdown;
        match std::mem::replace(&mut self.arena[idx], Open::Taken) {
            Open::Document { children } => Block::Document(Document {
                children: self.take_all(children),
            }),
            Open::Paragraph {
                text,
                reference_definitions,
            } => Block::Paragraph(Paragraph {
                markdown,
                text,
                reference_definitions,
            }),
            Open::BlockQuote { children } => Block::BlockQuote(BlockQuote {
                children: self.take_all(children),
            }),
            Open::List {
                is_loose,
                is_ordered,
                ordered_start,
                bullet_or_delimiter,
                children,
                ..
            } => Block::List(List {
                is_loose,
                is_ordered,
                ordered_start,
                bullet_or_delimiter,
                children: self.take_all(children),
            }),
            Open::ListItem {
                indentation,
                children,
                ..
            } => Block::ListItem(ListItem {
                indentation,
                children: self.take_all(children),
            }),
            Open::FencedCode {
                indentation,
                opening_fence,
                raw_info,
                raw_code,
                ..
            } => Block::FencedCode(FencedCode {
                markdown,
                indentation,
                opening_fence,
                raw_info,
                raw_code,
            }),
            Open::IndentedCode { raw_code } => {
                Block::IndentedCode(IndentedCode { markdown, raw_code })
            }
            // Unreachable: a node is taken once, by its single parent.
            Open::Taken => Block::Document(Document::default()),
        }
    }

    fn take_all(&mut self, children: Vec<usize>) -> Vec<Block<'a>> {
        children.into_iter().map(|c| self.take(c)).collect()
    }
}

/// The result of `parseListMarker` (list.go:145).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListMarker {
    pub is_ordered: bool,
    pub ordered_start: i64,
    pub bullet_or_delimiter: u8,
    pub width: usize,
    pub remaining: Range,
}

/// Port of `parseListMarker` (list.go:145): up to nine digits followed by `.` or `)`, or one of
/// `-`, `+`, `*`. Ten digits is not a marker. The space after the marker is the caller's
/// business.
pub(crate) fn parse_list_marker(markdown: &str, r: Range) -> Option<ListMarker> {
    let bytes = markdown.as_bytes();
    let mut digits = 0;
    let mut n: i64 = 0;
    let mut i = r.position;
    while i < r.end && i < bytes.len() && bytes[i].is_ascii_digit() {
        digits += 1;
        n = n * 10 + i64::from(bytes[i] - b'0');
        i += 1;
    }
    if digits > 0 {
        if digits > 9 || r.position + digits >= r.end {
            return None;
        }
        let next = bytes[r.position + digits];
        if next != b'.' && next != b')' {
            return None;
        }
        return Some(ListMarker {
            is_ordered: true,
            ordered_start: n,
            bullet_or_delimiter: next,
            width: digits + 1,
            remaining: Range::new(r.position + digits + 1, r.end),
        });
    }
    if r.position >= r.end || r.position >= bytes.len() {
        return None;
    }
    let next = bytes[r.position];
    if next != b'-' && next != b'+' && next != b'*' {
        return None;
    }
    Some(ListMarker {
        is_ordered: false,
        ordered_start: 0,
        bullet_or_delimiter: next,
        width: 1,
        remaining: Range::new(r.position + 1, r.end),
    })
}

/// Port of `markdown.ParseBlocks` (blocks.go:52): the document and, in the order their
/// paragraphs closed, every reference definition. The document is empty for no lines.
pub fn parse_blocks<'a>(
    markdown: &'a str,
    lines: &[Range],
) -> (Document<'a>, Vec<ReferenceDefinition<'a>>) {
    let mut parser = BlockParser {
        markdown,
        arena: Vec::new(),
        reference_definitions: Vec::new(),
    };
    let document = parser.parse_blocks(lines);
    let document = match parser.take(document) {
        Block::Document(d) => d,
        // `take` of the document index is always a document.
        _ => Document::default(),
    };
    (document, parser.reference_definitions)
}

/// `ParseLines` then `ParseBlocks`, with no length check — [`crate::parse`] adds that.
pub(crate) fn parse_unchecked(markdown: &str) -> (Document<'_>, Vec<ReferenceDefinition<'_>>) {
    let lines = parse_lines(markdown);
    parse_blocks(markdown, &lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(s: &str) -> Option<ListMarker> {
        parse_list_marker(s, Range::new(0, s.len()))
    }

    #[test]
    fn list_markers() {
        assert_eq!(
            marker("- a"),
            Some(ListMarker {
                is_ordered: false,
                ordered_start: 0,
                bullet_or_delimiter: b'-',
                width: 1,
                remaining: Range::new(1, 3)
            })
        );
        assert_eq!(marker("+ a").map(|m| m.bullet_or_delimiter), Some(b'+'));
        assert_eq!(marker("* a").map(|m| m.bullet_or_delimiter), Some(b'*'));
        assert_eq!(
            marker("12) a"),
            Some(ListMarker {
                is_ordered: true,
                ordered_start: 12,
                bullet_or_delimiter: b')',
                width: 3,
                remaining: Range::new(3, 5)
            })
        );
        assert_eq!(marker("0. a").map(|m| m.ordered_start), Some(0));
        assert_eq!(
            marker("123456789. a").map(|m| m.ordered_start),
            Some(123_456_789)
        );
        assert_eq!(marker("1234567890. a"), None);
        assert_eq!(marker("1"), None);
        assert_eq!(marker("1a"), None);
        assert_eq!(marker(""), None);
        assert_eq!(marker("a"), None);
        assert_eq!(
            marker("-"),
            Some(ListMarker {
                is_ordered: false,
                ordered_start: 0,
                bullet_or_delimiter: b'-',
                width: 1,
                remaining: Range::new(1, 1)
            })
        );
    }

    #[test]
    fn document_shape() {
        let (doc, defs) = parse_unchecked("- a\n\n  b\n- c\n\n[x]: /u\n");
        assert!(defs.len() == 1 && defs[0].label() == "x");
        assert_eq!(doc.children.len(), 2);
        let Block::List(list) = &doc.children[0] else {
            panic!("list")
        };
        assert!(list.is_loose);
        assert_eq!(list.children.len(), 2);
        let Block::ListItem(item) = &list.children[0] else {
            panic!("item")
        };
        assert_eq!(item.indentation, 2);
        assert_eq!(item.children.len(), 2);
        let Block::Paragraph(p) = &doc.children[1] else {
            panic!("paragraph")
        };
        assert!(p.text.is_empty());
        assert_eq!(p.reference_definitions.len(), 1);
    }

    #[test]
    fn code_blocks() {
        let (doc, _) = parse_unchecked("  ```rust\n  fn\n   x\n  ```\n\n    \tcode\n\n\n");
        let Block::FencedCode(f) = &doc.children[0] else {
            panic!("fenced")
        };
        assert_eq!(f.info(), "rust");
        assert_eq!(f.code(), "fn\n x\n");
        let Block::IndentedCode(c) = &doc.children[1] else {
            panic!("indented")
        };
        assert_eq!(c.code(), "    code\n");
        assert_eq!(doc.children.len(), 2);
    }

    #[test]
    fn nesting_stops_at_the_cap() {
        let md = format!("{}x", "> ".repeat(40));
        let (doc, _) = parse_unchecked(&md);
        let mut depth = 0;
        let mut block = &doc.children[0];
        while let Block::BlockQuote(b) = block {
            depth += 1;
            block = &b.children[0];
        }
        assert_eq!(depth, MAX_NESTING_DEPTH - 1);
        assert!(matches!(block, Block::Paragraph(_)));
    }
}
