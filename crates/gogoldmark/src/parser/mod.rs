//! Port of goldmark's `parser` package: the block phase (`parser.go` `parseBlocks`,
//! `openBlocks`, `closeBlocks`) and every default block parser, plus the paragraph and AST
//! transformers. The inline phase is in [`inline`], the HTML patterns in [`html`], the GFM table
//! transformer in [`table`].
//!
//! goldmark dispatches through interfaces registered by trigger byte and priority; the set here
//! is closed (the default parsers and the four GFM extensions), so each family is an enum and
//! the registration tables are built exactly as `parser.Parse`'s `initSync` builds them: stable
//! ascending priority sort (Go's `sort.Slice` is an insertion sort, hence stable, below twelve
//! elements, and no configuration here reaches twelve), per-trigger lists, free parsers appended.

pub(crate) mod html;
pub(crate) mod inline;
pub(crate) mod table;

use std::collections::HashMap;

use crate::Extension;
use crate::ast::{Ast, NodeData, NodeId, NodeKind};
use crate::text::{BlockReader, Reader, Segment, Segments};
use crate::util;

pub(crate) const STATE_CONTINUE: u8 = 1 << 1;
pub(crate) const STATE_CLOSE: u8 = 1 << 2;
pub(crate) const STATE_HAS_CHILDREN: u8 = 1 << 3;
pub(crate) const STATE_NO_CHILDREN: u8 = 1 << 4;
pub(crate) const STATE_REQUIRE_PARAGRAPH: u8 = 1 << 5;

/// The default block parsers (`parser.DefaultBlockParsers`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockParserKind {
    SetextHeading,
    ThematicBreak,
    List,
    ListItem,
    CodeBlock,
    AtxHeading,
    FencedCodeBlock,
    Blockquote,
    HtmlBlock,
    Paragraph,
}

impl BlockParserKind {
    fn trigger(self) -> Option<&'static [u8]> {
        match self {
            BlockParserKind::SetextHeading => Some(b"-="),
            BlockParserKind::ThematicBreak => Some(b"-*_"),
            BlockParserKind::List | BlockParserKind::ListItem => Some(b"-+*0123456789"),
            BlockParserKind::CodeBlock | BlockParserKind::Paragraph => None,
            BlockParserKind::AtxHeading => Some(b"#"),
            BlockParserKind::FencedCodeBlock => Some(b"~`"),
            BlockParserKind::Blockquote => Some(b">"),
            BlockParserKind::HtmlBlock => Some(b"<"),
        }
    }
    fn can_interrupt_paragraph(self) -> bool {
        !matches!(
            self,
            BlockParserKind::CodeBlock | BlockParserKind::Paragraph
        )
    }
    fn can_accept_indented_line(self) -> bool {
        self == BlockParserKind::CodeBlock
    }
}

/// The inline parsers of the default set and the GFM extensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InlineParserKind {
    TaskCheckBox,
    CodeSpan,
    Link,
    AutoLink,
    RawHtml,
    Emphasis,
    Strikethrough,
    Linkify,
}

impl InlineParserKind {
    fn trigger(self) -> &'static [u8] {
        match self {
            InlineParserKind::TaskCheckBox => b"[",
            InlineParserKind::CodeSpan => b"`",
            InlineParserKind::Link => b"![]",
            InlineParserKind::AutoLink | InlineParserKind::RawHtml => b"<",
            InlineParserKind::Emphasis => b"*_",
            InlineParserKind::Strikethrough => b"~",
            InlineParserKind::Linkify => b" *_~(",
        }
    }
    /// Only the link parser implements `parser.CloseBlocker` (the extensions' `CloseBlock`
    /// methods have the wrong arity and are never called).
    fn is_close_blocker(self) -> bool {
        self == InlineParserKind::Link
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParagraphTransformerKind {
    LinkReference,
    Table,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AstTransformerKind {
    Table,
}

/// Port of `parser.Block`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Block {
    pub node: NodeId,
    pub parser: BlockParserKind,
}

/// Port of `parser.reference` / `astReference`.
#[derive(Clone, Debug)]
pub(crate) struct Reference {
    pub destination: Vec<u8>,
    pub title: Option<Vec<u8>>,
}

/// Port of `fenceData` (fcode_block.go:22).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FenceData {
    ch: u8,
    indent: isize,
    length: isize,
    node: NodeId,
}

/// Port of `escapedPipeCell` (table.go:21).
#[derive(Clone, Debug)]
pub(crate) struct EscapedPipeCell {
    pub cell: NodeId,
    pub pos: Vec<isize>,
    pub transformed: bool,
}

/// The value under `linkBottom` (link.go:160). Go stores `pc.LastDelimiter()` — a
/// `*Delimiter` — in an `any`, so an empty stack is an **untyped** nil but a pushed "no
/// delimiter" is a **typed** nil that compares non-nil. [`inline`]'s `process_delimiters`
/// takes the difference into account, because goldmark's does.
#[derive(Clone, Debug, Default)]
pub(crate) enum LinkBottom {
    #[default]
    Nil,
    One(Option<NodeId>),
    Many(Vec<Option<NodeId>>),
}

/// Tracks the aliasing of Go's `openedBlocks` snapshot in `parseBlocks` (see
/// [`Parser::parse_blocks`]).
#[derive(Clone, Copy, Debug)]
struct SnapshotWatch {
    index: usize,
    popped: bool,
    written: Option<NodeId>,
}

/// Port of `parser.parseContext` with the context-store keys as named fields.
pub(crate) struct Context {
    refs: HashMap<Vec<u8>, Reference>,
    block_offset: isize,
    block_indent: isize,
    pub delimiters: Option<NodeId>,
    pub last_delimiter: Option<NodeId>,
    opened_blocks: Vec<Block>,
    watch: Option<SnapshotWatch>,
    temporary_paragraph: Option<NodeId>,
    skip_list_parser: bool,
    empty_list_item_with_blank_lines: bool,
    fenced_code_block_info: Option<FenceData>,
    pub link_label_state: Option<NodeId>,
    pub link_bottom: LinkBottom,
    pub escaped_pipe_cells: Option<Vec<EscapedPipeCell>>,
}

impl Context {
    fn new() -> Self {
        Context {
            refs: HashMap::new(),
            block_offset: -1,
            block_indent: -1,
            delimiters: None,
            last_delimiter: None,
            opened_blocks: Vec::new(),
            watch: None,
            temporary_paragraph: None,
            skip_list_parser: false,
            empty_list_item_with_blank_lines: false,
            fenced_code_block_info: None,
            link_label_state: None,
            link_bottom: LinkBottom::Nil,
            escaped_pipe_cells: None,
        }
    }

    /// Port of `AddReference`: the first definition of a label wins.
    fn add_reference(&mut self, label: &[u8], r: Reference) {
        let key = util::to_link_reference(label);
        self.refs.entry(key).or_insert(r);
    }

    pub(crate) fn reference(&self, label: &[u8]) -> Option<&Reference> {
        self.refs.get(label)
    }

    fn last_opened_block(&self) -> Option<Block> {
        self.opened_blocks.last().copied()
    }

    fn push_opened_block(&mut self, b: Block) {
        if let Some(w) = self.watch.as_mut()
            && w.popped
            && self.opened_blocks.len() == w.index
        {
            w.written = Some(b.node);
        }
        self.opened_blocks.push(b);
    }

    fn truncate_opened_blocks(&mut self, len: usize) {
        if let Some(w) = self.watch.as_mut()
            && len <= w.index
        {
            w.popped = true;
        }
        self.opened_blocks.truncate(len);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockOpenResult {
    ParagraphContinuation,
    NewBlocksOpened,
    NoBlocksOpened,
}

#[derive(Clone, Copy, Debug)]
struct LineStat {
    line_num: isize,
    level: usize,
    is_blank: bool,
}

/// Port of `isBlankLine` (parser.go:1013).
fn is_blank_line(line_num: isize, level: usize, stats: &[LineStat]) -> bool {
    let l = stats.len() as isize;
    if l == 0 {
        return true;
    }
    let mut i = l - 1 - level as isize;
    while i >= 0 {
        let s = stats[i as usize];
        if s.line_num == line_num && s.level <= level {
            return s.is_blank;
        } else if s.line_num < line_num {
            break;
        }
        i -= 1;
    }
    false
}

/// A configured goldmark parser (`parser.NewParser` with the default parsers and the given
/// extensions' parser options).
#[derive(Clone, Debug)]
pub struct Parser {
    block_parsers: Vec<Vec<BlockParserKind>>,
    free_block_parsers: Vec<BlockParserKind>,
    inline_parsers: Vec<Vec<InlineParserKind>>,
    close_blockers: Vec<InlineParserKind>,
    paragraph_transformers: Vec<ParagraphTransformerKind>,
    ast_transformers: Vec<AstTransformerKind>,
}

fn stable_sorted<T: Copy>(mut v: Vec<(T, i32)>) -> Vec<T> {
    v.sort_by_key(|&(_, p)| p);
    v.into_iter().map(|(t, _)| t).collect()
}

impl Parser {
    /// `goldmark.DefaultParser()` extended by `extensions` (each extension's `Extend` adds its
    /// parser options in the order goldmark's `GFM.Extend` calls them).
    pub fn new(extensions: &[Extension]) -> Self {
        let blocks = vec![
            (BlockParserKind::SetextHeading, 100),
            (BlockParserKind::ThematicBreak, 200),
            (BlockParserKind::List, 300),
            (BlockParserKind::ListItem, 400),
            (BlockParserKind::CodeBlock, 500),
            (BlockParserKind::AtxHeading, 600),
            (BlockParserKind::FencedCodeBlock, 700),
            (BlockParserKind::Blockquote, 800),
            (BlockParserKind::HtmlBlock, 900),
            (BlockParserKind::Paragraph, 1000),
        ];
        let mut inlines = vec![
            (InlineParserKind::CodeSpan, 100),
            (InlineParserKind::Link, 200),
            (InlineParserKind::AutoLink, 300),
            (InlineParserKind::RawHtml, 400),
            (InlineParserKind::Emphasis, 500),
        ];
        let mut paragraph = vec![(ParagraphTransformerKind::LinkReference, 100)];
        let mut ast_t: Vec<(AstTransformerKind, i32)> = Vec::new();
        let mut add = |e: Extension| match e {
            Extension::Linkify => inlines.push((InlineParserKind::Linkify, 999)),
            Extension::Table => {
                paragraph.push((ParagraphTransformerKind::Table, 200));
                ast_t.push((AstTransformerKind::Table, 0));
            }
            Extension::Strikethrough => inlines.push((InlineParserKind::Strikethrough, 500)),
            Extension::TaskList => inlines.push((InlineParserKind::TaskCheckBox, 0)),
            Extension::Gfm => {}
        };
        for &e in extensions {
            if e == Extension::Gfm {
                add(Extension::Linkify);
                add(Extension::Table);
                add(Extension::Strikethrough);
                add(Extension::TaskList);
            } else {
                add(e);
            }
        }
        // Block parsers need no extension hook (none of the four adds one).

        let mut block_parsers: Vec<Vec<BlockParserKind>> = vec![Vec::new(); 256];
        let mut free = Vec::new();
        for bp in stable_sorted(blocks) {
            match bp.trigger() {
                None => free.push(bp),
                Some(tcs) => {
                    for &tc in tcs {
                        block_parsers[tc as usize].push(bp);
                    }
                }
            }
        }
        for list in block_parsers.iter_mut() {
            if !list.is_empty() {
                list.extend_from_slice(&free);
            }
        }
        let mut inline_parsers: Vec<Vec<InlineParserKind>> = vec![Vec::new(); 256];
        let mut close_blockers = Vec::new();
        for ip in stable_sorted(inlines) {
            if ip.is_close_blocker() {
                close_blockers.push(ip);
            }
            for &tc in ip.trigger() {
                inline_parsers[tc as usize].push(ip);
            }
        }
        Parser {
            block_parsers,
            free_block_parsers: free,
            inline_parsers,
            close_blockers,
            paragraph_transformers: stable_sorted(paragraph),
            ast_transformers: stable_sorted(ast_t),
        }
    }

    /// Port of `parser.Parse`: the block phase, then every block's inlines (children first),
    /// then the AST transformers.
    pub fn parse(&self, source: &[u8]) -> Ast {
        let mut ast = Ast::new();
        let root = ast.root();
        let mut pc = Context::new();
        let mut reader = Reader::new(source);
        self.parse_blocks(&mut ast, root, &mut reader, &mut pc);

        let mut block_reader = BlockReader::new(source, &[]);
        // walkBlock: post-order, NextSibling read after the child's subtree.
        enum Frame {
            Visit { node: NodeId, next: Option<NodeId> },
            After { node: NodeId, child: NodeId },
        }
        let first = ast.first_child(root);
        let mut stack = vec![Frame::Visit {
            node: root,
            next: first,
        }];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Visit { node, next } => match next {
                    Some(c) => {
                        stack.push(Frame::After { node, child: c });
                        let cf = ast.first_child(c);
                        stack.push(Frame::Visit { node: c, next: cf });
                    }
                    None => self.parse_block(&mut ast, &mut block_reader, node, &mut pc),
                },
                Frame::After { node, child } => {
                    let next = ast.next_sibling(child);
                    stack.push(Frame::Visit { node, next });
                }
            }
        }
        for at in &self.ast_transformers {
            match at {
                AstTransformerKind::Table => table::transform_ast(&mut ast, &mut pc),
            }
        }
        ast
    }

    /// Port of `transformParagraph`: true when a transformer removed the paragraph.
    fn transform_paragraph(
        &self,
        ast: &mut Ast,
        node: NodeId,
        reader: &Reader<'_>,
        pc: &mut Context,
    ) -> bool {
        for pt in &self.paragraph_transformers {
            match pt {
                ParagraphTransformerKind::LinkReference => {
                    transform_link_reference(ast, node, reader.source(), pc)
                }
                ParagraphTransformerKind::Table => {
                    table::transform_paragraph(ast, node, reader.source(), pc)
                }
            }
            if ast.parent(node).is_none() {
                return true;
            }
        }
        false
    }

    /// Port of `closeBlocks` (parser.go:870). `from`/`to` are signed because `parseBlocks` can
    /// hand in `lastIndex - 1 == -1`.
    fn close_blocks(
        &self,
        ast: &mut Ast,
        from: isize,
        to: isize,
        reader: &mut Reader<'_>,
        pc: &mut Context,
    ) {
        let blocks = pc.opened_blocks.clone();
        let mut i = from;
        while i >= to && i >= 0 {
            let Some(b) = blocks.get(i as usize).copied() else {
                break;
            };
            if ast.kind(b.node) == NodeKind::Paragraph && ast.parent(b.node).is_some() {
                self.transform_paragraph(ast, b.node, reader, pc);
            }
            if ast.parent(b.node).is_some() {
                self.close_block(b.parser, ast, b.node, reader, pc);
            }
            i -= 1;
        }
        let len = blocks.len() as isize;
        let to_u = to.clamp(0, len) as usize;
        if from == len - 1 {
            pc.truncate_opened_blocks(to_u);
        } else {
            let tail_start = (from + 1).clamp(0, len) as usize;
            let mut nb: Vec<Block> = blocks[..to_u].to_vec();
            nb.extend_from_slice(&blocks[tail_start..]);
            if nb.len() < blocks.len() {
                pc.truncate_opened_blocks(to_u);
            }
            pc.opened_blocks = nb;
        }
    }

    /// Port of `openBlocks` (parser.go:900).
    fn open_blocks(
        &self,
        ast: &mut Ast,
        mut parent: NodeId,
        blank_line: bool,
        reader: &mut Reader<'_>,
        pc: &mut Context,
    ) -> BlockOpenResult {
        let mut result = BlockOpenResult::NoBlocksOpened;
        let mut continuable = false;
        let mut last_block = pc.last_opened_block();
        if let Some(lb) = last_block {
            continuable = ast.kind(lb.node) == NodeKind::Paragraph;
        }
        'retry: loop {
            let (line, _) = reader.peek_line();
            let line_offset = reader.line_offset();
            let line_bytes: &[u8] = line.as_deref().unwrap_or(&[]);
            let (w, pos) = util::indent_width(line_bytes, line_offset);
            if w >= line_bytes.len() as isize {
                pc.block_offset = -1;
                pc.block_indent = -1;
            } else {
                pc.block_offset = pos;
                pc.block_indent = w;
            }
            if line.is_none() || line_bytes.first() == Some(&b'\n') {
                break 'retry;
            }
            let mut bps: &[BlockParserKind] = &self.free_block_parsers;
            if (pos as usize) < line_bytes.len() {
                bps = &self.block_parsers[line_bytes[pos as usize] as usize];
                if bps.is_empty() {
                    bps = &self.free_block_parsers;
                }
            }
            for &bp in bps {
                if continuable
                    && result == BlockOpenResult::NoBlocksOpened
                    && !bp.can_interrupt_paragraph()
                {
                    continue;
                }
                if w > 3 && !bp.can_accept_indented_line() {
                    continue;
                }
                last_block = pc.last_opened_block();
                let last = last_block.map(|b| b.node);
                let (_, block_pos) = reader.position();
                let Some((node, state)) = self.open_block(bp, ast, parent, reader, pc) else {
                    continue;
                };
                ast.set_pos(node, block_pos.start + pc.block_offset.max(0));
                if state & STATE_REQUIRE_PARAGRAPH != 0
                    && last == ast.last_child(parent)
                    && let (Some(lb), Some(l)) = (last_block, last)
                {
                    self.close_block(lb.parser, ast, l, reader, pc);
                    let n = pc.opened_blocks.len();
                    pc.truncate_opened_blocks(n.saturating_sub(1));
                    if self.transform_paragraph(ast, l, reader, pc) {
                        continuable = false;
                        continue 'retry;
                    }
                }
                ast.set_blank_previous_lines(node, blank_line);
                if let Some(l) = last
                    && ast.parent(l).is_none()
                {
                    let last_pos = pc.opened_blocks.len() as isize - 1;
                    self.close_blocks(ast, last_pos, last_pos, reader, pc);
                }
                ast.append_child(parent, node);
                result = BlockOpenResult::NewBlocksOpened;
                pc.push_opened_block(Block { node, parser: bp });
                if state & STATE_HAS_CHILDREN != 0 {
                    parent = node;
                    continue 'retry;
                }
                break;
            }
            break 'retry;
        }
        if result == BlockOpenResult::NoBlocksOpened
            && continuable
            && let Some(lb) = last_block
        {
            let state = self.continue_block(lb.parser, ast, lb.node, reader, pc);
            if state & STATE_CONTINUE != 0 {
                result = BlockOpenResult::ParagraphContinuation;
            }
        }
        result
    }

    /// Port of `parseBlocks` (parser.go:1031).
    ///
    /// Go iterates a *snapshot* slice of the opened blocks while `openBlocks` pops and pushes
    /// the context's slice, and the two share a backing array: when `openBlocks` pops the last
    /// block (a setext heading consuming its paragraph) and pushes a new one, the push writes
    /// into the snapshot's last slot. `parseBlocks` detects exactly that with
    /// `openedBlocks[lastIndex].Node != lastNode`. [`SnapshotWatch`] reproduces the write.
    fn parse_blocks(
        &self,
        ast: &mut Ast,
        parent: NodeId,
        reader: &mut Reader<'_>,
        pc: &mut Context,
    ) {
        pc.opened_blocks.clear();
        let mut blank_lines: Vec<LineStat> = Vec::with_capacity(128);
        loop {
            let (_, _, ok) = reader.skip_blank_lines();
            if !ok {
                return;
            }
            if self.open_blocks(ast, parent, true, reader, pc) != BlockOpenResult::NewBlocksOpened {
                return;
            }
            reader.advance_line();
            blank_lines.clear();
            loop {
                let opened = pc.opened_blocks.clone();
                let l = opened.len();
                if l == 0 {
                    break;
                }
                let mut last_index = l as isize - 1;
                for i in 0..l {
                    let be = opened[i];
                    let (line, _) = reader.peek_line();
                    let Some(line) = line else {
                        self.close_blocks(ast, last_index, 0, reader, pc);
                        reader.advance_line();
                        return;
                    };
                    let (line_num, _) = reader.position();
                    blank_lines.push(LineStat {
                        line_num,
                        level: i,
                        is_blank: util::is_blank(&line),
                    });
                    if ast.kind(be.node) != NodeKind::Paragraph {
                        let state = self.continue_block(be.parser, ast, be.node, reader, pc);
                        if state & STATE_CONTINUE != 0 {
                            if state & STATE_HAS_CHILDREN != 0 && i as isize == last_index {
                                let is_blank = is_blank_line(line_num - 1, i + 1, &blank_lines);
                                self.open_blocks(ast, be.node, is_blank, reader, pc);
                                break;
                            }
                            continue;
                        }
                    }
                    let is_blank = is_blank_line(line_num - 1, i, &blank_lines);
                    let this_parent = if i != 0 { opened[i - 1].node } else { parent };
                    let last_node = opened[last_index as usize].node;
                    pc.watch = Some(SnapshotWatch {
                        index: last_index as usize,
                        popped: false,
                        written: None,
                    });
                    let result = self.open_blocks(ast, this_parent, is_blank, reader, pc);
                    let watch = pc.watch.take();
                    if result != BlockOpenResult::ParagraphContinuation {
                        let seen = watch
                            .and_then(|w| w.written)
                            .unwrap_or(opened[last_index as usize].node);
                        if seen != last_node {
                            last_index -= 1;
                        }
                        self.close_blocks(ast, last_index, i as isize, reader, pc);
                    }
                    break;
                }
                reader.advance_line();
            }
        }
    }

    // ---- block parser dispatch ----

    fn open_block(
        &self,
        bp: BlockParserKind,
        ast: &mut Ast,
        parent: NodeId,
        reader: &mut Reader<'_>,
        pc: &mut Context,
    ) -> Option<(NodeId, u8)> {
        match bp {
            BlockParserKind::SetextHeading => setext_open(ast, parent, reader, pc),
            BlockParserKind::ThematicBreak => thematic_break_open(ast, reader),
            BlockParserKind::List => list_open(ast, parent, reader, pc),
            BlockParserKind::ListItem => list_item_open(ast, parent, reader, pc),
            BlockParserKind::CodeBlock => code_block_open(ast, reader),
            BlockParserKind::AtxHeading => atx_heading_open(ast, reader, pc),
            BlockParserKind::FencedCodeBlock => fenced_code_block_open(ast, reader, pc),
            BlockParserKind::Blockquote => {
                if blockquote_process(reader) {
                    Some((ast.new_node(NodeData::Blockquote), STATE_HAS_CHILDREN))
                } else {
                    None
                }
            }
            BlockParserKind::HtmlBlock => html::html_block_open(ast, reader, pc),
            BlockParserKind::Paragraph => paragraph_open(ast, reader),
        }
    }

    fn continue_block(
        &self,
        bp: BlockParserKind,
        ast: &mut Ast,
        node: NodeId,
        reader: &mut Reader<'_>,
        pc: &mut Context,
    ) -> u8 {
        match bp {
            BlockParserKind::SetextHeading
            | BlockParserKind::ThematicBreak
            | BlockParserKind::AtxHeading => STATE_CLOSE,
            BlockParserKind::List => list_continue(ast, node, reader, pc),
            BlockParserKind::ListItem => list_item_continue(ast, node, reader, pc),
            BlockParserKind::CodeBlock => code_block_continue(ast, node, reader),
            BlockParserKind::FencedCodeBlock => fenced_code_block_continue(ast, node, reader, pc),
            BlockParserKind::Blockquote => {
                if blockquote_process(reader) {
                    STATE_CONTINUE | STATE_HAS_CHILDREN
                } else {
                    STATE_CLOSE
                }
            }
            BlockParserKind::HtmlBlock => html::html_block_continue(ast, node, reader),
            BlockParserKind::Paragraph => paragraph_continue(ast, node, reader),
        }
    }

    fn close_block(
        &self,
        bp: BlockParserKind,
        ast: &mut Ast,
        node: NodeId,
        reader: &mut Reader<'_>,
        pc: &mut Context,
    ) {
        match bp {
            BlockParserKind::SetextHeading => setext_close(ast, node, reader, pc),
            BlockParserKind::List => list_close(ast, node),
            BlockParserKind::CodeBlock => code_block_close(ast, node, reader),
            BlockParserKind::FencedCodeBlock => {
                if let Some(f) = pc.fenced_code_block_info
                    && f.node == node
                {
                    pc.fenced_code_block_info = None;
                }
            }
            BlockParserKind::Paragraph => paragraph_close(ast, node, reader),
            BlockParserKind::ThematicBreak
            | BlockParserKind::ListItem
            | BlockParserKind::AtxHeading
            | BlockParserKind::Blockquote
            | BlockParserKind::HtmlBlock => {}
        }
    }
}

// ---- setext_headings.go ----

/// Port of `matchesSetextHeadingBar`.
pub(crate) fn matches_setext_heading_bar(line: &[u8]) -> Option<u8> {
    let mut start = 0usize;
    let mut end = line.len();
    let space = util::trim_left_length(line, b" ");
    if space > 3 {
        return None;
    }
    start += space;
    let level1 = util::trim_left_length(&line[start..end], b"=");
    let mut c = b'=';
    let mut level2 = 0;
    if level1 == 0 {
        level2 = util::trim_left_length(&line[start..end], b"-");
        c = b'-';
    }
    if end > 0 && util::is_space(line[end - 1]) {
        end -= util::trim_right_space_length(&line[start..end]);
    }
    if !((level1 > 0 && start + level1 == end) || (level2 > 0 && start + level2 == end)) {
        return None;
    }
    Some(c)
}

fn setext_open(
    ast: &mut Ast,
    parent: NodeId,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> Option<(NodeId, u8)> {
    let last = pc.last_opened_block()?.node;
    if ast.kind(last) != NodeKind::Paragraph || ast.parent(last) != Some(parent) {
        return None;
    }
    let (line, segment) = reader.peek_line();
    let c = matches_setext_heading_bar(line.as_deref()?)?;
    let level = if c == b'-' { 2 } else { 1 };
    let node = ast.new_node(NodeData::Heading { level });
    ast.lines_mut(node).append(segment);
    pc.temporary_paragraph = Some(last);
    Some((node, STATE_NO_CHILDREN | STATE_REQUIRE_PARAGRAPH))
}

fn setext_close(ast: &mut Ast, node: NodeId, reader: &Reader<'_>, pc: &mut Context) {
    let lines = ast.lines(node);
    let mut segment = if lines.is_empty() {
        Segment::default()
    } else {
        lines.at(0)
    };
    ast.lines_mut(node).clear();
    // Go type-asserts a nil here and panics; no reachable input gets there.
    let Some(tmp) = pc.temporary_paragraph.take() else {
        return;
    };
    if ast.lines(tmp).is_empty() {
        let next = ast.next_sibling(node);
        segment = segment.trim_left_space(reader.source());
        let Some(hp) = ast.parent(node) else {
            return;
        };
        match next {
            Some(nx) if ast.kind(nx) == NodeKind::Paragraph => {
                ast.lines_mut(nx).unshift(segment);
            }
            _ => {
                let para = ast.new_node(NodeData::Paragraph);
                ast.lines_mut(para).append(segment);
                ast.insert_after(hp, node, para);
            }
        }
        ast.remove_child(hp, node);
    } else {
        let first = ast.lines(tmp).at(0).start;
        ast.set_pos(node, first);
        let tl = ast.lines(tmp).clone();
        ast.set_lines(node, tl);
        let blank = ast.has_blank_previous_lines(tmp);
        ast.set_blank_previous_lines(node, blank);
        if let Some(tp) = ast.parent(tmp) {
            ast.remove_child(tp, tmp);
        }
    }
}

// ---- thematic_break.go ----

/// Port of `isThematicBreak`.
pub(crate) fn is_thematic_break(line: &[u8], offset: isize) -> bool {
    let (w, pos) = util::indent_width(line, offset);
    if w > 3 {
        return false;
    }
    let mut mark = 0u8;
    let mut count = 0;
    for &c in &line[pos as usize..] {
        if util::is_space(c) {
            continue;
        }
        if mark == 0 {
            mark = c;
            count = 1;
            if mark == b'*' || mark == b'-' || mark == b'_' {
                continue;
            }
            return false;
        }
        if c != mark {
            return false;
        }
        count += 1;
    }
    count > 2
}

fn thematic_break_open(ast: &mut Ast, reader: &mut Reader<'_>) -> Option<(NodeId, u8)> {
    let (line, _) = reader.peek_line();
    let offset = reader.line_offset();
    if is_thematic_break(line.as_deref().unwrap_or(&[]), offset) {
        reader.advance_to_eol();
        return Some((ast.new_node(NodeData::ThematicBreak), STATE_NO_CHILDREN));
    }
    None
}

// ---- list.go ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListItemType {
    NotList,
    Bullet,
    Ordered,
}

/// Port of `parseListItem`.
fn parse_list_item(line: &[u8]) -> ([isize; 6], ListItemType) {
    let mut i = 0usize;
    let l = line.len();
    let mut ret = [0isize; 6];
    while i < l && line[i] == b' ' {
        i += 1;
    }
    if i > 3 {
        return (ret, ListItemType::NotList);
    }
    ret[1] = i as isize;
    ret[2] = i as isize;
    let typ = if i < l && matches!(line[i], b'-' | b'*' | b'+') {
        i += 1;
        ret[3] = i as isize;
        ListItemType::Bullet
    } else if i < l {
        while i < l && util::is_numeric(line[i]) {
            i += 1;
        }
        ret[3] = i as isize;
        if ret[3] == ret[2] || ret[3] - ret[2] > 9 {
            return (ret, ListItemType::NotList);
        }
        if i < l && (line[i] == b'.' || line[i] == b')') {
            i += 1;
            ret[3] = i as isize;
        } else {
            return (ret, ListItemType::NotList);
        }
        ListItemType::Ordered
    } else {
        return (ret, ListItemType::NotList);
    };
    if i < l && line[i] != b'\n' {
        let (w, _) = util::indent_width(&line[i..], 0);
        if w == 0 {
            return (ret, ListItemType::NotList);
        }
    }
    if i >= l {
        ret[4] = -1;
        ret[5] = -1;
        return (ret, typ);
    }
    ret[4] = i as isize;
    ret[5] = line.len() as isize;
    if line[(ret[5] - 1) as usize] == b'\n' && line[i] != b'\n' {
        ret[5] -= 1;
    }
    (ret, typ)
}

/// Port of `calcListOffset`.
fn calc_list_offset(source: &[u8], m: [isize; 6]) -> isize {
    if m[4] < 0 || util::is_blank(&source[m[4] as usize..]) {
        1
    } else {
        let (offset, _) = util::indent_width(&source[m[4] as usize..], m[4]);
        if offset > 4 { 1 } else { offset }
    }
}

/// Port of `lastOffset`.
fn last_offset(ast: &Ast, node: NodeId) -> isize {
    match ast.last_child(node).map(|c| ast.data(c)) {
        Some(NodeData::ListItem { offset }) => *offset,
        _ => 0,
    }
}

fn list_is_ordered(marker: u8) -> bool {
    marker == b'.' || marker == b')'
}

fn list_open(
    ast: &mut Ast,
    parent: NodeId,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> Option<(NodeId, u8)> {
    let last = pc.last_opened_block().map(|b| b.node);
    if last.is_some_and(|l| ast.kind(l) == NodeKind::List) || pc.skip_list_parser {
        pc.skip_list_parser = false;
        return None;
    }
    let (line, _) = reader.peek_line();
    let line = line?;
    let (m, typ) = parse_list_item(&line);
    if typ == ListItemType::NotList {
        return None;
    }
    let mut start: i64 = -1;
    if typ == ListItemType::Ordered {
        let number = &line[m[2] as usize..(m[3] - 1) as usize];
        start = std::str::from_utf8(number)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    if let Some(l) = last
        && ast.kind(l) == NodeKind::Paragraph
        && ast.parent(l) == Some(parent)
    {
        if typ == ListItemType::Ordered && start != 1 {
            return None;
        }
        if m[4] < 0 || util::is_blank(&line[m[4] as usize..m[5] as usize]) {
            return None;
        }
    }
    let marker = line[(m[3] - 1) as usize];
    let node = ast.new_node(NodeData::List {
        marker,
        is_tight: true,
        start: if start > -1 { start } else { 0 },
    });
    pc.empty_list_item_with_blank_lines = false;
    Some((node, STATE_HAS_CHILDREN))
}

fn list_continue(ast: &mut Ast, node: NodeId, reader: &mut Reader<'_>, pc: &mut Context) -> u8 {
    let (line, _) = reader.peek_line();
    let line = line.unwrap_or_default();
    let last_child_count = ast.last_child(node).map_or(0, |c| ast.child_count(c));
    if util::is_blank(&line) {
        if last_child_count == 0 {
            pc.empty_list_item_with_blank_lines = true;
        }
        return STATE_CONTINUE | STATE_HAS_CHILDREN;
    }
    let offset = last_offset(ast, node);
    let last_is_empty = last_child_count == 0;
    let line_offset = reader.line_offset();
    let (indent, _) = util::indent_width(&line, line_offset);

    if indent < offset || last_is_empty {
        if indent < 4 {
            let (m, typ) = parse_list_item(&line);
            if typ != ListItemType::NotList && m[1] - offset < 4 {
                let marker = line[(m[3] - 1) as usize];
                let list_marker = match ast.data(node) {
                    NodeData::List { marker, .. } => *marker,
                    _ => 0,
                };
                let can_continue = marker == list_marker
                    && (typ == ListItemType::Ordered) == list_is_ordered(list_marker);
                if !can_continue {
                    return STATE_CLOSE;
                }
                let rest = &line[(m[3] - 1) as usize..];
                if is_thematic_break(rest, 0) {
                    let mut is_heading = false;
                    if let Some(last) = pc.last_opened_block()
                        && ast.kind(last.node) == NodeKind::Paragraph
                        && matches_setext_heading_bar(rest) == Some(b'-')
                    {
                        is_heading = true;
                    }
                    if !is_heading {
                        return STATE_CLOSE;
                    }
                }
                return STATE_CONTINUE | STATE_HAS_CHILDREN;
            }
        }
        if !last_is_empty {
            return STATE_CLOSE;
        }
    }
    if last_is_empty && indent < offset {
        return STATE_CLOSE;
    }
    if pc.empty_list_item_with_blank_lines {
        return STATE_CLOSE;
    }
    STATE_CONTINUE | STATE_HAS_CHILDREN
}

/// Port of `listParser.Close`: tightness, then paragraphs of a tight list become text blocks.
fn list_close(ast: &mut Ast, node: NodeId) {
    let mut tight = matches!(ast.data(node), NodeData::List { is_tight: true, .. });
    let first = ast.first_child(node);
    let mut c = first;
    while let Some(cn) = c {
        if !tight {
            break;
        }
        if let (Some(fc), lc) = (ast.first_child(cn), ast.last_child(cn))
            && Some(fc) != lc
        {
            let mut c1 = ast.next_sibling(fc);
            while let Some(x) = c1 {
                if ast.has_blank_previous_lines(x) {
                    tight = false;
                    break;
                }
                c1 = ast.next_sibling(x);
            }
        }
        if Some(cn) != first && ast.has_blank_previous_lines(cn) {
            tight = false;
        }
        c = ast.next_sibling(cn);
    }
    if let NodeData::List { is_tight, .. } = ast.data_mut(node) {
        *is_tight = tight;
    }
    if tight {
        let mut child = ast.first_child(node);
        while let Some(ch) = child {
            let mut gc = ast.first_child(ch);
            while let Some(g) = gc {
                gc = ast.next_sibling(g);
                if ast.kind(g) == NodeKind::Paragraph {
                    let tb = ast.new_node(NodeData::TextBlock);
                    let lines = ast.lines(g).clone();
                    ast.set_lines(tb, lines);
                    ast.replace_child(ch, g, tb);
                }
            }
            child = ast.next_sibling(ch);
        }
    }
}

// ---- list_item.go ----

fn list_item_open(
    ast: &mut Ast,
    parent: NodeId,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> Option<(NodeId, u8)> {
    if ast.kind(parent) != NodeKind::List {
        return None;
    }
    let offset = last_offset(ast, parent);
    let (line, _) = reader.peek_line();
    let line = line?;
    let (m, typ) = parse_list_item(&line);
    if typ == ListItemType::NotList {
        return None;
    }
    if m[1] - offset > 3 {
        return None;
    }
    pc.empty_list_item_with_blank_lines = false;
    let item_offset = calc_list_offset(&line, m);
    let node = ast.new_node(NodeData::ListItem {
        offset: m[3] + item_offset,
    });
    if m[4] < 0 || util::is_blank(&line[m[4] as usize..m[5] as usize]) {
        return Some((node, STATE_NO_CHILDREN));
    }
    let (pos, padding) = util::indent_position(&line[m[4] as usize..], m[4], item_offset);
    let child = m[3] + pos;
    reader.advance_and_set_padding(child, padding);
    Some((node, STATE_HAS_CHILDREN))
}

fn list_item_continue(
    ast: &mut Ast,
    node: NodeId,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> u8 {
    let (line, _) = reader.peek_line();
    let line = line.unwrap_or_default();
    if util::is_blank(&line) {
        reader.advance_to_eol();
        return STATE_CONTINUE | STATE_HAS_CHILDREN;
    }
    let offset = ast.parent(node).map_or(0, |p| last_offset(ast, p));
    let is_empty = ast.child_count(node) == 0 && pc.empty_list_item_with_blank_lines;
    let line_offset = reader.line_offset();
    let (indent, _) = util::indent_width(&line, line_offset);
    if (is_empty || indent < offset) && indent < 4 {
        let (_, typ) = parse_list_item(&line);
        if typ != ListItemType::NotList {
            pc.skip_list_parser = true;
            return STATE_CLOSE;
        }
        if !is_empty {
            return STATE_CLOSE;
        }
    }
    let line_offset = reader.line_offset();
    let (pos, padding) = util::indent_position(&line, line_offset, offset);
    reader.advance_and_set_padding(pos, padding);
    STATE_CONTINUE | STATE_HAS_CHILDREN
}

// ---- code_block.go ----

/// Port of `preserveLeadingTabInCodeBlock`.
fn preserve_leading_tab_in_code_block(
    segment: &mut Segment,
    reader: &mut Reader<'_>,
    indent: isize,
) {
    let offset_with_padding = reader.line_offset() + indent;
    let (sl, ss) = reader.position();
    reader.set_position(sl, Segment::new(ss.start - 1, ss.stop));
    if offset_with_padding == reader.line_offset() {
        segment.padding = 0;
        segment.start -= 1;
    }
    reader.set_position(sl, ss);
}

fn code_block_open(ast: &mut Ast, reader: &mut Reader<'_>) -> Option<(NodeId, u8)> {
    let (line, _) = reader.peek_line();
    let line = line?;
    let line_offset = reader.line_offset();
    let (pos, padding) = util::indent_position(&line, line_offset, 4);
    if pos < 0 || util::is_blank(&line) {
        return None;
    }
    let node = ast.new_node(NodeData::CodeBlock);
    reader.advance_and_set_padding(pos, padding);
    let (_, mut segment) = reader.peek_line();
    if segment.padding != 0 {
        preserve_leading_tab_in_code_block(&mut segment, reader, 0);
    }
    segment.force_newline = true;
    ast.lines_mut(node).append(segment);
    reader.advance_to_eol();
    Some((node, STATE_NO_CHILDREN))
}

fn code_block_continue(ast: &mut Ast, node: NodeId, reader: &mut Reader<'_>) -> u8 {
    let (line, segment) = reader.peek_line();
    let line = line.unwrap_or_default();
    if util::is_blank(&line) {
        let s = segment.trim_left_space_width(4, reader.source());
        ast.lines_mut(node).append(s);
        return STATE_CONTINUE | STATE_NO_CHILDREN;
    }
    let line_offset = reader.line_offset();
    let (pos, padding) = util::indent_position(&line, line_offset, 4);
    if pos < 0 {
        return STATE_CLOSE;
    }
    reader.advance_and_set_padding(pos, padding);
    let (_, mut segment) = reader.peek_line();
    if segment.padding != 0 {
        preserve_leading_tab_in_code_block(&mut segment, reader, 0);
    }
    segment.force_newline = true;
    ast.lines_mut(node).append(segment);
    reader.advance_to_eol();
    STATE_CONTINUE | STATE_NO_CHILDREN
}

fn code_block_close(ast: &mut Ast, node: NodeId, reader: &Reader<'_>) {
    let source = reader.source();
    let lines = ast.lines(node);
    let mut length = lines.len() as isize - 1;
    while length >= 0 {
        let line = lines.at(length as usize);
        if util::is_blank(&line.value(source)) {
            length -= 1;
        } else {
            break;
        }
    }
    ast.lines_mut(node).set_sliced(0, (length + 1) as usize);
}

// ---- atx_heading.go ----

fn atx_heading_open(
    ast: &mut Ast,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> Option<(NodeId, u8)> {
    let (line, segment) = reader.peek_line();
    let line = line?;
    let pos = pc.block_offset;
    if pos < 0 {
        return None;
    }
    let pos = pos as usize;
    let mut i = pos;
    while i < line.len() && line[i] == b'#' {
        i += 1;
    }
    let level = i - pos;
    if i == pos || level > 6 {
        return None;
    }
    if i == line.len() {
        return Some((
            ast.new_node(NodeData::Heading { level: level as u8 }),
            STATE_NO_CHILDREN,
        ));
    }
    let l = util::trim_left_space_length(&line[i..]);
    if l == 0 {
        return None;
    }
    let start = (i + l).min(line.len() - 1) as isize;
    let node = ast.new_node(NodeData::Heading { level: level as u8 });
    let source = reader.source();
    let mut hl = Segment::new(
        segment.start + start - segment.padding,
        segment.start + line.len() as isize - segment.padding,
    );
    hl = hl.trim_right_space(source);
    // `hl.Len() == 0`; Segment::len counts padding, which is zero here.
    if hl.is_empty() {
        reader.advance_to_eol();
        return Some((node, STATE_NO_CHILDREN));
    }
    let hv = hl.value(source);
    let mut stop = hv.len();
    if stop != 0 {
        let mut i = stop - 1;
        while hv[i] == b'#' && i > 0 {
            i -= 1;
        }
        if i == 0 && hv[0] == b'#' {
            reader.advance_to_eol();
            return Some((node, STATE_NO_CHILDREN));
        }
        if i != stop - 1 && util::is_space(hv[i]) {
            stop = i;
            stop -= util::trim_right_space_length(&hv[..stop]);
        }
    }
    hl.stop = hl.start + stop as isize;
    ast.lines_mut(node).append(hl);
    reader.advance_to_eol();
    Some((node, STATE_NO_CHILDREN))
}

// ---- fcode_block.go ----

fn fenced_code_block_open(
    ast: &mut Ast,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> Option<(NodeId, u8)> {
    let (line, segment) = reader.peek_line();
    let line = line?;
    let pos = pc.block_indent;
    if pos < 0 || pos as usize >= line.len() {
        return None;
    }
    let findent = pos;
    let pos = pos as usize;
    let fence_char = line[pos];
    let mut i = pos;
    while i < line.len() && line[i] == fence_char {
        i += 1;
    }
    let o_fence_length = (i - pos) as isize;
    if o_fence_length < 3 {
        return None;
    }
    let mut info = None;
    if i + 1 < line.len() {
        let rest = &line[i..];
        let left = util::trim_left_space_length(rest);
        let right = util::trim_right_space_length(rest);
        if left < rest.len().saturating_sub(right) {
            let info_start = segment.start - segment.padding + (i + left) as isize;
            let info_stop = segment.stop - right as isize;
            let value = &rest[left..rest.len() - right];
            if fence_char == b'`' && value.contains(&b'`') {
                return None;
            } else if info_start != info_stop {
                info = Some(Segment::new(info_start, info_stop));
            }
        }
    }
    let node = ast.new_node(NodeData::FencedCodeBlock { info });
    pc.fenced_code_block_info = Some(FenceData {
        ch: fence_char,
        indent: findent,
        length: o_fence_length,
        node,
    });
    Some((node, STATE_NO_CHILDREN))
}

fn fenced_code_block_continue(
    ast: &mut Ast,
    node: NodeId,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> u8 {
    let (line, segment) = reader.peek_line();
    let line = line.unwrap_or_default();
    // Go type-asserts a nil here and panics; no reachable input gets there.
    let Some(fdata) = pc.fenced_code_block_info else {
        return STATE_CLOSE;
    };
    let line_offset = reader.line_offset();
    let (w, pos) = util::indent_width(&line, line_offset);
    if w < 4 {
        let mut i = pos as usize;
        while i < line.len() && line[i] == fdata.ch {
            i += 1;
        }
        let length = i as isize - pos;
        if length >= fdata.length && util::is_blank(&line[i..]) {
            reader.advance_to_eol();
            return STATE_CLOSE;
        }
    }
    let line_offset = reader.line_offset();
    let (mut pos, mut padding) =
        util::indent_position_padding(&line, line_offset, segment.padding, fdata.indent);
    if pos < 0 {
        pos = util::first_non_space_position(&line).max(0) - segment.padding;
        padding = 0;
    }
    let mut seg = Segment::new_padding(segment.start + pos, segment.stop, padding);
    if padding != 0 {
        preserve_leading_tab_in_code_block(&mut seg, reader, fdata.indent);
    }
    seg.force_newline = true;
    ast.lines_mut(node).append(seg);
    reader.advance_to_eol();
    STATE_CONTINUE | STATE_NO_CHILDREN
}

// ---- blockquote.go ----

fn blockquote_process(reader: &mut Reader<'_>) -> bool {
    let (line, _) = reader.peek_line();
    let line = line.unwrap_or_default();
    let line_offset = reader.line_offset();
    let (w, pos) = util::indent_width(&line, line_offset);
    let mut pos = pos as usize;
    if w > 3 || pos >= line.len() || line[pos] != b'>' {
        return false;
    }
    pos += 1;
    if pos >= line.len() || line[pos] == b'\n' {
        reader.advance(pos as isize);
        return true;
    }
    reader.advance(pos as isize);
    if line[pos] == b' ' || line[pos] == b'\t' {
        let mut padding = 0;
        if line[pos] == b'\t' {
            padding = util::tab_width(reader.line_offset()) - 1;
        }
        reader.advance_and_set_padding(1, padding);
    }
    true
}

// ---- paragraph.go ----

fn paragraph_open(ast: &mut Ast, reader: &mut Reader<'_>) -> Option<(NodeId, u8)> {
    let (line, segment) = reader.peek_line();
    if util::is_blank(line.as_deref().unwrap_or(&[])) {
        return None;
    }
    let node = ast.new_node(NodeData::Paragraph);
    ast.lines_mut(node).append(segment);
    reader.advance_to_eol();
    Some((node, STATE_NO_CHILDREN))
}

fn paragraph_continue(ast: &mut Ast, node: NodeId, reader: &mut Reader<'_>) -> u8 {
    let (line, segment) = reader.peek_line();
    if util::is_blank(line.as_deref().unwrap_or(&[])) {
        return STATE_CLOSE;
    }
    ast.lines_mut(node).append(segment);
    reader.advance_to_eol();
    STATE_CONTINUE | STATE_NO_CHILDREN
}

fn paragraph_close(ast: &mut Ast, node: NodeId, reader: &Reader<'_>) {
    let source = reader.source();
    let n = ast.lines(node).len();
    if n != 0 {
        for i in 0..n {
            let l = ast.lines(node).at(i);
            ast.lines_mut(node).set(i, l.trim_left_space(source));
        }
        let last = ast.lines(node).at(n - 1);
        ast.lines_mut(node)
            .set(n - 1, last.trim_right_space(source));
    }
    if ast.lines(node).is_empty()
        && let Some(p) = ast.parent(node)
    {
        ast.remove_child(p, node);
    }
}

// ---- link_ref.go ----

fn transform_link_reference(ast: &mut Ast, node: NodeId, source: &[u8], pc: &mut Context) {
    let mut lines: Segments = ast.lines(node).clone();
    let mut block = BlockReader::new(source, lines.as_slice());
    let mut removes: Vec<(usize, usize)> = Vec::new();
    while let Some((reference, start, mut end)) =
        parse_link_reference_definition(ast, &mut block, pc)
    {
        if start == 0 {
            let blank = ast.has_blank_previous_lines(node);
            ast.set_blank_previous_lines(reference, blank);
        }
        if let Some(p) = ast.parent(node) {
            ast.insert_before(p, Some(node), reference);
        }
        for i in start + 1..end {
            if i < lines.len() {
                let l = lines.at(i);
                ast.lines_mut(reference).append(l);
            }
        }
        let rl = ast.lines(reference).len();
        if rl > 0 {
            let seg = ast.lines(reference).at(rl - 1);
            ast.lines_mut(reference)
                .set(rl - 1, seg.trim_right_space(source));
        }
        if start == end {
            end += 1;
        }
        removes.push((start, end));
    }
    let mut offset = 0usize;
    for (r0, r1) in removes {
        if lines.is_empty() {
            break;
        }
        let len = lines.len();
        let lo = r1.saturating_sub(offset).min(len);
        let s: Vec<Segment> = lines.sliced(lo, len).to_vec();
        let hi = r0.saturating_sub(offset).min(len);
        lines.set_sliced(0, hi);
        lines.append_all(&s);
        offset = r1;
    }
    if lines.is_empty() {
        if let Some(p) = ast.parent(node) {
            ast.remove_child(p, node);
        }
        return;
    }
    ast.set_lines(node, lines);
}

const LINK_FIND_CLOSURE_OPTIONS: crate::text::FindClosureOptions =
    crate::text::FindClosureOptions {
        code_span: false,
        nesting: false,
        newline: true,
        advance: true,
    };

fn segments_value(block: &BlockReader<'_>, segments: &Segments) -> Option<Vec<u8>> {
    if segments.len() == 1 {
        return Some(block.value(segments.at(0)));
    }
    // `append(nil, empty...)` stays nil in Go.
    let mut out: Option<Vec<u8>> = None;
    for s in segments.as_slice() {
        let v = block.value(*s);
        if !v.is_empty() || out.is_some() {
            out.get_or_insert_with(Vec::new).extend_from_slice(&v);
        }
    }
    out
}

/// Port of `parseLinkReferenceDefinition`: `(node, startLine, endLine)`, or `None` for Go's
/// `(nil, -1, -1)`.
fn parse_link_reference_definition(
    ast: &mut Ast,
    block: &mut BlockReader<'_>,
    pc: &mut Context,
) -> Option<(NodeId, usize, usize)> {
    block.skip_spaces();
    let (line, _) = block.peek_line();
    let line = line?;
    let (start_line, _) = block.position();
    let (width, mut pos) = util::indent_width(&line, 0);
    if width > 3 {
        return None;
    }
    if width != 0 {
        pos += 1;
    }
    if line.get(pos as usize) != Some(&b'[') {
        return None;
    }
    let (_, start_pos) = block.position();
    block.advance(pos + 1);
    let segments = block.find_closure(b'[', b']', LINK_FIND_CLOSURE_OPTIONS)?;
    let label = segments_value(block, &segments).unwrap_or_default();
    if util::is_blank(&label) {
        return None;
    }
    if block.peek() != b':' {
        return None;
    }
    block.advance(1);
    block.skip_spaces();
    let destination = inline::parse_link_destination(block)?;
    let (line, _) = block.peek_line();
    let is_new_line = line.as_deref().is_none_or(util::is_blank);

    let (end_line, _) = block.position();
    let (_, spaces, _) = block.skip_spaces();
    let opener = block.peek();
    let make = |ast: &mut Ast, pc: &mut Context, title: Option<Vec<u8>>| {
        pc.add_reference(
            &label,
            Reference {
                destination: destination.clone(),
                title: title.clone(),
            },
        );
        let r = ast.new_node(NodeData::LinkReferenceDefinition {
            label: label.clone(),
            destination: destination.clone(),
            title,
        });
        ast.lines_mut(r).append(start_pos);
        r
    };
    if opener != b'"' && opener != b'\'' && opener != b'(' {
        if !is_new_line {
            return None;
        }
        let r = make(ast, pc, None);
        return Some((r, start_line as usize, (end_line + 1) as usize));
    }
    if spaces == 0 {
        return None;
    }
    block.advance(1);
    let closer = if opener == b'(' { b')' } else { opener };
    let Some(segments) = block.find_closure(opener, closer, LINK_FIND_CLOSURE_OPTIONS) else {
        if !is_new_line {
            return None;
        }
        let r = make(ast, pc, None);
        block.advance_line();
        return Some((r, start_line as usize, (end_line + 1) as usize));
    };
    let title = segments_value(block, &segments);
    let (line, _) = block.peek_line();
    if let Some(line) = line
        && !util::is_blank(&line)
    {
        if !is_new_line {
            return None;
        }
        let r = make(ast, pc, title);
        return Some((r, start_line as usize, end_line as usize));
    }
    let (end_line, _) = block.position();
    let r = make(ast, pc, title);
    Some((r, start_line as usize, (end_line + 1) as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setext_bar_rules() {
        assert_eq!(matches_setext_heading_bar(b"===\n"), Some(b'='));
        assert_eq!(matches_setext_heading_bar(b"   ---  \n"), Some(b'-'));
        assert_eq!(matches_setext_heading_bar(b"    ---\n"), None);
        assert_eq!(matches_setext_heading_bar(b"=-=\n"), None);
        assert_eq!(matches_setext_heading_bar(b"-- -\n"), None);
    }

    #[test]
    fn thematic_break_rules() {
        assert!(is_thematic_break(b"***\n", 0));
        assert!(is_thematic_break(b" - - -\n", 0));
        assert!(!is_thematic_break(b"--\n", 0));
        assert!(!is_thematic_break(b"    ***\n", 0));
        assert!(!is_thematic_break(b"*-*\n", 0));
        assert!(!is_thematic_break(b"+++\n", 0));
    }

    #[test]
    fn list_item_rules() {
        assert_eq!(parse_list_item(b"- a\n").1, ListItemType::Bullet);
        assert_eq!(parse_list_item(b"1. a\n").1, ListItemType::Ordered);
        assert_eq!(parse_list_item(b"1234567890. a\n").1, ListItemType::NotList);
        assert_eq!(parse_list_item(b"-a\n").1, ListItemType::NotList);
        assert_eq!(parse_list_item(b"    - a\n").1, ListItemType::NotList);
        let (m, t) = parse_list_item(b"-");
        assert_eq!(t, ListItemType::Bullet);
        assert_eq!((m[4], m[5]), (-1, -1));
        let (m, _) = parse_list_item(b"12) x\n");
        assert_eq!(m, [0, 0, 0, 3, 3, 5]);
    }

    #[test]
    fn blank_line_lookup() {
        let stats = [
            LineStat {
                line_num: 0,
                level: 0,
                is_blank: false,
            },
            LineStat {
                line_num: 1,
                level: 0,
                is_blank: true,
            },
            LineStat {
                line_num: 1,
                level: 1,
                is_blank: false,
            },
        ];
        assert!(is_blank_line(5, 0, &[]));
        assert!(is_blank_line(1, 1, &stats));
        assert!(is_blank_line(1, 0, &stats));
        assert!(!is_blank_line(0, 2, &stats));
    }
}
