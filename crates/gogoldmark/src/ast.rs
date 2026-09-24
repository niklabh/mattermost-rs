//! Port of goldmark's `ast` package (`ast/ast.go`, `ast/block.go`, `ast/inline.go`) and of the
//! GFM node types in `extension/ast`.
//!
//! Go's nodes are heap objects linked by interface pointers; here they live in one arena
//! ([`Ast`]) and link by [`NodeId`]. Every tree mutation keeps Go's exact bookkeeping, including
//! its quirks, because the parser reads it back:
//!
//! - `ChildCount` is a counter, not a count. `InsertBefore` increments it **and then**, when
//!   the reference node is nil, calls `AppendChild`, which increments it again; when the
//!   reference node is not a child it increments without inserting anything. The list parsers
//!   test `ChildCount() == 0`, so the drift is observable and is reproduced.
//! - `Walk` re-reads `FirstChild` after the entering callback and `NextSibling` after each
//!   child's walk, so a callback may restructure what is still ahead of it. [`Ast::walk`] is
//!   iterative (goldmark recurses; a Go stack grows, a Rust one does not) but reads the links at
//!   the same moments.

use crate::text::{Segment, Segments};

/// Index of a node in its [`Ast`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) u32);

/// Port of `ast.NodeType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeType {
    /// `ast.TypeBlock`.
    Block,
    /// `ast.TypeInline`.
    Inline,
    /// `ast.TypeDocument`.
    Document,
}

/// Port of `ast.NodeKind`: every kind the default parser and the GFM extensions produce, plus
/// the two parser-internal kinds (`Delimiter`, `LinkLabelState`) that exist in the tree only
/// while a block's inlines are being parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// `ast.KindDocument`.
    Document,
    /// `ast.KindTextBlock`.
    TextBlock,
    /// `ast.KindParagraph`.
    Paragraph,
    /// `ast.KindHeading`.
    Heading,
    /// `ast.KindThematicBreak`.
    ThematicBreak,
    /// `ast.KindCodeBlock`.
    CodeBlock,
    /// `ast.KindFencedCodeBlock`.
    FencedCodeBlock,
    /// `ast.KindBlockquote`.
    Blockquote,
    /// `ast.KindList`.
    List,
    /// `ast.KindListItem`.
    ListItem,
    /// `ast.KindHTMLBlock`.
    HTMLBlock,
    /// `ast.KindLinkReferenceDefinition`.
    LinkReferenceDefinition,
    /// `ast.KindText`.
    Text,
    /// `ast.KindString`.
    String,
    /// `ast.KindCodeSpan`.
    CodeSpan,
    /// `ast.KindEmphasis`.
    Emphasis,
    /// `ast.KindLink`.
    Link,
    /// `ast.KindImage`.
    Image,
    /// `ast.KindAutoLink`.
    AutoLink,
    /// `ast.KindRawHTML`.
    RawHTML,
    /// `extension/ast.KindStrikethrough`.
    Strikethrough,
    /// `extension/ast.KindTable`.
    Table,
    /// `extension/ast.KindTableHeader`.
    TableHeader,
    /// `extension/ast.KindTableRow`.
    TableRow,
    /// `extension/ast.KindTableCell`.
    TableCell,
    /// `extension/ast.KindTaskCheckBox`.
    TaskCheckBox,
    /// `parser.kindDelimiter` (parser-internal).
    Delimiter,
    /// `parser.kindLinkLabelState` (parser-internal).
    LinkLabelState,
}

impl NodeKind {
    /// Port of `NodeKind.String` — the names Go registers with `NewNodeKind`.
    pub fn name(self) -> &'static str {
        match self {
            NodeKind::Document => "Document",
            NodeKind::TextBlock => "TextBlock",
            NodeKind::Paragraph => "Paragraph",
            NodeKind::Heading => "Heading",
            NodeKind::ThematicBreak => "ThematicBreak",
            NodeKind::CodeBlock => "CodeBlock",
            NodeKind::FencedCodeBlock => "FencedCodeBlock",
            NodeKind::Blockquote => "Blockquote",
            NodeKind::List => "List",
            NodeKind::ListItem => "ListItem",
            NodeKind::HTMLBlock => "HTMLBlock",
            NodeKind::LinkReferenceDefinition => "LinkReferenceDefinition",
            NodeKind::Text => "Text",
            NodeKind::String => "String",
            NodeKind::CodeSpan => "CodeSpan",
            NodeKind::Emphasis => "Emphasis",
            NodeKind::Link => "Link",
            NodeKind::Image => "Image",
            NodeKind::AutoLink => "AutoLink",
            NodeKind::RawHTML => "RawHTML",
            NodeKind::Strikethrough => "Strikethrough",
            NodeKind::Table => "Table",
            NodeKind::TableHeader => "TableHeader",
            NodeKind::TableRow => "TableRow",
            NodeKind::TableCell => "TableCell",
            NodeKind::TaskCheckBox => "TaskCheckBox",
            NodeKind::Delimiter => "Delimiter",
            NodeKind::LinkLabelState => "LinkLabelState",
        }
    }
}

/// Port of `ast.WalkStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkStatus {
    /// `ast.WalkStop`.
    Stop,
    /// `ast.WalkSkipChildren`.
    SkipChildren,
    /// `ast.WalkContinue`.
    Continue,
}

/// Port of `ast.HTMLBlockType` (1–7).
pub type HtmlBlockType = u8;

/// Port of `ast.AutoLinkType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoLinkType {
    /// `ast.AutoLinkEmail` (1).
    Email,
    /// `ast.AutoLinkURL` (2).
    Url,
}

/// Port of `ast.ReferenceLinkType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceLinkType {
    /// `ast.ReferenceLinkFull`.
    Full,
    /// `ast.ReferenceLinkCollapsed`.
    Collapsed,
    /// `ast.ReferenceLinkShortcut`.
    Shortcut,
}

/// Port of `ast.ReferenceLink`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceLink {
    /// Which reference form resolved the link.
    pub typ: ReferenceLinkType,
    /// The label as written.
    pub value: Vec<u8>,
}

/// Port of `extension/ast.Alignment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    /// `AlignLeft`.
    Left,
    /// `AlignRight`.
    Right,
    /// `AlignCenter`.
    Center,
    /// `AlignNone`.
    None,
}

impl Alignment {
    /// Port of `Alignment.String`.
    pub fn as_str(self) -> &'static str {
        match self {
            Alignment::Left => "left",
            Alignment::Right => "right",
            Alignment::Center => "center",
            Alignment::None => "none",
        }
    }
}

/// An attribute value: goldmark stores `any`, and the renderer only prints `[]byte` and
/// `string` (anything else prints empty).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeValue {
    /// A `[]byte` or `string` value.
    Bytes(Vec<u8>),
}

/// Port of `ast.Attribute`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    /// Attribute name.
    pub name: Vec<u8>,
    /// Attribute value.
    pub value: AttributeValue,
}

pub(crate) const TEXT_SOFT_LINE_BREAK: u8 = 1;
pub(crate) const TEXT_HARD_LINE_BREAK: u8 = 2;
pub(crate) const TEXT_RAW: u8 = 4;
pub(crate) const TEXT_CODE: u8 = 8;

/// The per-kind payload of a node.
#[derive(Clone, Debug)]
pub enum NodeData {
    /// `ast.Document`.
    Document,
    /// `ast.TextBlock`.
    TextBlock,
    /// `ast.Paragraph`.
    Paragraph,
    /// `ast.Heading`.
    Heading {
        /// `Level` (1–6).
        level: u8,
    },
    /// `ast.ThematicBreak`.
    ThematicBreak,
    /// `ast.CodeBlock`.
    CodeBlock,
    /// `ast.FencedCodeBlock`; `info` is the segment of the `Info` text node.
    FencedCodeBlock {
        /// The info string's segment, when there is one.
        info: Option<Segment>,
    },
    /// `ast.Blockquote`.
    Blockquote,
    /// `ast.List`.
    List {
        /// `Marker`: `-`, `+`, `*`, `.` or `)`.
        marker: u8,
        /// `IsTight`.
        is_tight: bool,
        /// `Start` (ordered lists).
        start: i64,
    },
    /// `ast.ListItem`.
    ListItem {
        /// `Offset`.
        offset: isize,
    },
    /// `ast.HTMLBlock`.
    HTMLBlock {
        /// `HTMLBlockType`.
        typ: HtmlBlockType,
        /// `ClosureLine` (start `-1` when absent).
        closure_line: Segment,
    },
    /// `ast.LinkReferenceDefinition`.
    LinkReferenceDefinition {
        /// `Label`.
        label: Vec<u8>,
        /// `Destination`.
        destination: Vec<u8>,
        /// `Title` (`None` is Go's nil).
        title: Option<Vec<u8>>,
    },
    /// `ast.Text`.
    Text {
        /// `Segment`.
        segment: Segment,
        /// Soft/hard line break, raw, code flags.
        flags: u8,
    },
    /// `ast.String`.
    String {
        /// `Value`.
        value: Vec<u8>,
        /// Raw/code flags.
        flags: u8,
    },
    /// `ast.CodeSpan`.
    CodeSpan,
    /// `ast.Emphasis`.
    Emphasis {
        /// `Level`: 1 is `<em>`, 2 is `<strong>`.
        level: u8,
    },
    /// `ast.Link`.
    Link {
        /// `Destination`.
        destination: Vec<u8>,
        /// `Title` (`None` is Go's nil).
        title: Option<Vec<u8>>,
        /// `Reference`.
        reference: Option<ReferenceLink>,
    },
    /// `ast.Image`.
    Image {
        /// `Destination`.
        destination: Vec<u8>,
        /// `Title` (`None` is Go's nil).
        title: Option<Vec<u8>>,
        /// `Reference`.
        reference: Option<ReferenceLink>,
    },
    /// `ast.AutoLink`.
    AutoLink {
        /// `AutoLinkType`.
        typ: AutoLinkType,
        /// `Protocol` (`None` is Go's nil).
        protocol: Option<Vec<u8>>,
        /// The segment of the private `value` text node.
        value: Segment,
    },
    /// `ast.RawHTML`.
    RawHTML {
        /// `Segments`.
        segments: Segments,
    },
    /// `extension/ast.Strikethrough`.
    Strikethrough,
    /// `extension/ast.Table`.
    Table {
        /// `Alignments`.
        alignments: Vec<Alignment>,
    },
    /// `extension/ast.TableHeader`.
    TableHeader {
        /// `Alignments` (never set by goldmark: `NewTableHeader` leaves it nil).
        alignments: Vec<Alignment>,
    },
    /// `extension/ast.TableRow`.
    TableRow {
        /// `Alignments`.
        alignments: Vec<Alignment>,
    },
    /// `extension/ast.TableCell`.
    TableCell {
        /// `Alignment`.
        alignment: Alignment,
    },
    /// `extension/ast.TaskCheckBox`.
    TaskCheckBox {
        /// `IsChecked`.
        checked: bool,
    },
    /// `parser.Delimiter`.
    Delimiter(Delimiter),
    /// `parser.linkLabelState`.
    LinkLabelState(LinkLabelState),
}

/// Which delimiter processor owns a delimiter run (`parser.DelimiterProcessor`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelimiterProcessor {
    /// `emphasisDelimiterProcessor` (`*`, `_`).
    Emphasis,
    /// `strikethroughDelimiterProcessor` (`~`).
    Strikethrough,
}

/// Port of `parser.Delimiter` (delimiter.go:31).
#[derive(Clone, Debug)]
pub struct Delimiter {
    /// `Segment`.
    pub segment: Segment,
    /// `CanOpen`.
    pub can_open: bool,
    /// `CanClose`.
    pub can_close: bool,
    /// `Length`.
    pub length: isize,
    /// `OriginalLength`.
    pub original_length: isize,
    /// `Char`.
    pub ch: u8,
    /// `PreviousDelimiter`.
    pub previous_delimiter: Option<NodeId>,
    /// `NextDelimiter`.
    pub next_delimiter: Option<NodeId>,
    /// `Processor`.
    pub processor: DelimiterProcessor,
}

/// Port of `parser.linkLabelState` (link.go:18).
#[derive(Clone, Debug)]
pub struct LinkLabelState {
    /// `Segment`.
    pub segment: Segment,
    /// `IsImage`.
    pub is_image: bool,
    /// `Prev`.
    pub prev: Option<NodeId>,
    /// `Next`.
    pub next: Option<NodeId>,
    /// `First`.
    pub first: Option<NodeId>,
    /// `Last`.
    pub last: Option<NodeId>,
}

/// One node: Go's `BaseNode` links plus `BaseBlock`'s lines and the kind's payload.
#[derive(Clone, Debug)]
pub struct Node {
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    parent: Option<NodeId>,
    next: Option<NodeId>,
    prev: Option<NodeId>,
    child_count: isize,
    attributes: Option<Vec<Attribute>>,
    pos: Option<isize>,
    lines: Segments,
    blank_previous_lines: bool,
    /// The kind's payload.
    pub data: NodeData,
}

/// The arena holding one parsed document.
#[derive(Clone, Debug)]
pub struct Ast {
    nodes: Vec<Node>,
    root: NodeId,
}

impl Default for Ast {
    fn default() -> Self {
        Ast::new()
    }
}

impl Ast {
    /// A new arena holding an empty `Document` (`ast.NewDocument`).
    pub fn new() -> Self {
        let mut a = Ast {
            nodes: Vec::new(),
            root: NodeId(0),
        };
        a.root = a.new_node(NodeData::Document);
        a
    }

    /// The document node.
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// Allocate a detached node.
    pub fn new_node(&mut self, data: NodeData) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            first_child: None,
            last_child: None,
            parent: None,
            next: None,
            prev: None,
            child_count: 0,
            attributes: None,
            pos: None,
            lines: Segments::new(),
            blank_previous_lines: false,
            data,
        });
        id
    }

    /// Port of `ast.NewTextSegment`.
    pub fn new_text_segment(&mut self, segment: Segment) -> NodeId {
        self.new_node(NodeData::Text { segment, flags: 0 })
    }

    /// Port of `ast.NewRawTextSegment`.
    pub fn new_raw_text_segment(&mut self, segment: Segment) -> NodeId {
        self.new_node(NodeData::Text {
            segment,
            flags: TEXT_RAW,
        })
    }

    /// The node itself.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// The node itself, mutably.
    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0 as usize]
    }

    /// The node's payload.
    pub fn data(&self, id: NodeId) -> &NodeData {
        &self.node(id).data
    }

    /// The node's payload, mutably.
    pub fn data_mut(&mut self, id: NodeId) -> &mut NodeData {
        &mut self.node_mut(id).data
    }

    /// Port of `Node.Kind`.
    pub fn kind(&self, id: NodeId) -> NodeKind {
        match &self.node(id).data {
            NodeData::Document => NodeKind::Document,
            NodeData::TextBlock => NodeKind::TextBlock,
            NodeData::Paragraph => NodeKind::Paragraph,
            NodeData::Heading { .. } => NodeKind::Heading,
            NodeData::ThematicBreak => NodeKind::ThematicBreak,
            NodeData::CodeBlock => NodeKind::CodeBlock,
            NodeData::FencedCodeBlock { .. } => NodeKind::FencedCodeBlock,
            NodeData::Blockquote => NodeKind::Blockquote,
            NodeData::List { .. } => NodeKind::List,
            NodeData::ListItem { .. } => NodeKind::ListItem,
            NodeData::HTMLBlock { .. } => NodeKind::HTMLBlock,
            NodeData::LinkReferenceDefinition { .. } => NodeKind::LinkReferenceDefinition,
            NodeData::Text { .. } => NodeKind::Text,
            NodeData::String { .. } => NodeKind::String,
            NodeData::CodeSpan => NodeKind::CodeSpan,
            NodeData::Emphasis { .. } => NodeKind::Emphasis,
            NodeData::Link { .. } => NodeKind::Link,
            NodeData::Image { .. } => NodeKind::Image,
            NodeData::AutoLink { .. } => NodeKind::AutoLink,
            NodeData::RawHTML { .. } => NodeKind::RawHTML,
            NodeData::Strikethrough => NodeKind::Strikethrough,
            NodeData::Table { .. } => NodeKind::Table,
            NodeData::TableHeader { .. } => NodeKind::TableHeader,
            NodeData::TableRow { .. } => NodeKind::TableRow,
            NodeData::TableCell { .. } => NodeKind::TableCell,
            NodeData::TaskCheckBox { .. } => NodeKind::TaskCheckBox,
            NodeData::Delimiter(_) => NodeKind::Delimiter,
            NodeData::LinkLabelState(_) => NodeKind::LinkLabelState,
        }
    }

    /// Port of `Node.Type`.
    pub fn node_type(&self, id: NodeId) -> NodeType {
        match self.kind(id) {
            NodeKind::Document => NodeType::Document,
            NodeKind::TextBlock
            | NodeKind::Paragraph
            | NodeKind::Heading
            | NodeKind::ThematicBreak
            | NodeKind::CodeBlock
            | NodeKind::FencedCodeBlock
            | NodeKind::Blockquote
            | NodeKind::List
            | NodeKind::ListItem
            | NodeKind::HTMLBlock
            | NodeKind::LinkReferenceDefinition
            | NodeKind::Table
            | NodeKind::TableHeader
            | NodeKind::TableRow
            | NodeKind::TableCell => NodeType::Block,
            _ => NodeType::Inline,
        }
    }

    /// Port of `Node.IsRaw`.
    pub fn is_raw(&self, id: NodeId) -> bool {
        match &self.node(id).data {
            NodeData::CodeBlock
            | NodeData::FencedCodeBlock { .. }
            | NodeData::HTMLBlock { .. }
            | NodeData::LinkReferenceDefinition { .. } => true,
            NodeData::Text { flags, .. } | NodeData::String { flags, .. } => flags & TEXT_RAW != 0,
            _ => false,
        }
    }

    /// Port of `Node.Pos`.
    pub fn pos(&self, id: NodeId) -> isize {
        let n = self.node(id);
        match &n.data {
            NodeData::Document => 0,
            NodeData::TextBlock
            | NodeData::Paragraph
            | NodeData::LinkReferenceDefinition { .. } => {
                if n.lines.is_empty() {
                    -1
                } else {
                    n.lines.at(0).start
                }
            }
            NodeData::Text { segment, .. } => segment.start,
            NodeData::String { .. } => -1,
            _ => n.pos.unwrap_or(-1),
        }
    }

    /// Port of `Node.SetPos`.
    pub fn set_pos(&mut self, id: NodeId, v: isize) {
        self.node_mut(id).pos = Some(v);
    }

    /// Port of `Node.Parent`.
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).parent
    }
    /// Port of `Node.FirstChild`.
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).first_child
    }
    /// Port of `Node.LastChild`.
    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).last_child
    }
    /// Port of `Node.NextSibling`.
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).next
    }
    /// Port of `Node.PreviousSibling`.
    pub fn previous_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).prev
    }
    /// Port of `Node.HasChildren`.
    pub fn has_children(&self, id: NodeId) -> bool {
        self.node(id).first_child.is_some()
    }
    /// Port of `Node.ChildCount` — Go's counter, drift included (see the module docs).
    pub fn child_count(&self, id: NodeId) -> isize {
        self.node(id).child_count
    }
    /// The children in order.
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut c = self.first_child(id);
        while let Some(n) = c {
            out.push(n);
            c = self.next_sibling(n);
        }
        out
    }

    /// Port of `BaseBlock.Lines` (inline nodes have none; Go panics, this is empty).
    pub fn lines(&self, id: NodeId) -> &Segments {
        &self.node(id).lines
    }
    /// Mutable `Lines`.
    pub fn lines_mut(&mut self, id: NodeId) -> &mut Segments {
        &mut self.node_mut(id).lines
    }
    /// Port of `BaseBlock.SetLines`.
    pub fn set_lines(&mut self, id: NodeId, lines: Segments) {
        self.node_mut(id).lines = lines;
    }
    /// Port of `HasBlankPreviousLines`.
    pub fn has_blank_previous_lines(&self, id: NodeId) -> bool {
        self.node(id).blank_previous_lines
    }
    /// Port of `SetBlankPreviousLines`.
    pub fn set_blank_previous_lines(&mut self, id: NodeId, v: bool) {
        self.node_mut(id).blank_previous_lines = v;
    }

    /// Port of `Node.Attributes` (`None` is Go's nil slice).
    pub fn attributes(&self, id: NodeId) -> Option<&[Attribute]> {
        self.node(id).attributes.as_deref()
    }
    /// Port of `Node.Attribute`.
    pub fn attribute(&self, id: NodeId, name: &[u8]) -> Option<&AttributeValue> {
        self.node(id)
            .attributes
            .as_ref()?
            .iter()
            .find(|a| a.name == name)
            .map(|a| &a.value)
    }
    /// Port of `Node.SetAttribute`.
    pub fn set_attribute(&mut self, id: NodeId, name: &[u8], value: AttributeValue) {
        let attrs = self.node_mut(id).attributes.get_or_insert_with(Vec::new);
        if let Some(a) = attrs.iter_mut().find(|a| a.name == name) {
            a.value = value;
            return;
        }
        attrs.push(Attribute {
            name: name.to_vec(),
            value,
        });
    }

    // ---- Text / String accessors ----

    /// The segment of a `Text` node.
    pub fn text_segment(&self, id: NodeId) -> Option<Segment> {
        match &self.node(id).data {
            NodeData::Text { segment, .. } => Some(*segment),
            _ => None,
        }
    }
    /// Mutable segment of a `Text` node.
    pub(crate) fn text_segment_mut(&mut self, id: NodeId) -> Option<&mut Segment> {
        match &mut self.node_mut(id).data {
            NodeData::Text { segment, .. } => Some(segment),
            _ => None,
        }
    }
    fn flags(&self, id: NodeId) -> u8 {
        match &self.node(id).data {
            NodeData::Text { flags, .. } | NodeData::String { flags, .. } => *flags,
            _ => 0,
        }
    }
    fn set_flag(&mut self, id: NodeId, bit: u8, v: bool) {
        if let NodeData::Text { flags, .. } | NodeData::String { flags, .. } =
            &mut self.node_mut(id).data
        {
            if v {
                *flags |= bit;
            } else {
                *flags &= !bit;
            }
        }
    }
    /// Port of `Text.SoftLineBreak`.
    pub fn soft_line_break(&self, id: NodeId) -> bool {
        matches!(self.data(id), NodeData::Text { .. }) && self.flags(id) & TEXT_SOFT_LINE_BREAK != 0
    }
    /// Port of `Text.SetSoftLineBreak`.
    pub fn set_soft_line_break(&mut self, id: NodeId, v: bool) {
        self.set_flag(id, TEXT_SOFT_LINE_BREAK, v);
    }
    /// Port of `Text.HardLineBreak`.
    pub fn hard_line_break(&self, id: NodeId) -> bool {
        matches!(self.data(id), NodeData::Text { .. }) && self.flags(id) & TEXT_HARD_LINE_BREAK != 0
    }
    /// Port of `Text.SetHardLineBreak`.
    pub fn set_hard_line_break(&mut self, id: NodeId, v: bool) {
        self.set_flag(id, TEXT_HARD_LINE_BREAK, v);
    }
    /// Port of `Text.SetRaw` / `String.SetRaw`.
    pub fn set_raw(&mut self, id: NodeId, v: bool) {
        self.set_flag(id, TEXT_RAW, v);
    }
    /// Port of `String.IsCode`.
    pub fn is_code(&self, id: NodeId) -> bool {
        matches!(self.data(id), NodeData::String { .. }) && self.flags(id) & TEXT_CODE != 0
    }
    /// Port of `String.SetCode`.
    pub fn set_code(&mut self, id: NodeId, v: bool) {
        self.set_flag(id, TEXT_CODE, v);
    }
    /// The value of a `String` node.
    pub fn string_value(&self, id: NodeId) -> Option<&[u8]> {
        match &self.node(id).data {
            NodeData::String { value, .. } => Some(value),
            _ => None,
        }
    }

    // ---- tree mutation (ast.go:224-331) ----

    fn ensure_isolated(&mut self, v: NodeId) {
        if let Some(p) = self.parent(v) {
            self.remove_child(p, v);
        }
    }

    /// Port of `BaseNode.RemoveChild`: a no-op unless `v` is a child of `parent`.
    pub fn remove_child(&mut self, parent: NodeId, v: NodeId) {
        if self.parent(v) != Some(parent) {
            return;
        }
        self.node_mut(parent).child_count -= 1;
        let prev = self.previous_sibling(v);
        let next = self.next_sibling(v);
        match prev {
            Some(p) => self.node_mut(p).next = next,
            None => self.node_mut(parent).first_child = next,
        }
        match next {
            Some(n) => self.node_mut(n).prev = prev,
            None => self.node_mut(parent).last_child = prev,
        }
        let n = self.node_mut(v);
        n.parent = None;
        n.prev = None;
        n.next = None;
    }

    /// Port of `BaseNode.RemoveChildren`.
    pub fn remove_children(&mut self, parent: NodeId) {
        let mut c = self.first_child(parent);
        while let Some(n) = c {
            let next = self.next_sibling(n);
            let node = self.node_mut(n);
            node.parent = None;
            node.prev = None;
            node.next = None;
            c = next;
        }
        let p = self.node_mut(parent);
        p.first_child = None;
        p.last_child = None;
        p.child_count = 0;
    }

    /// Port of `BaseNode.AppendChild`.
    pub fn append_child(&mut self, parent: NodeId, v: NodeId) {
        self.ensure_isolated(v);
        match self.node(parent).first_child {
            None => {
                self.node_mut(parent).first_child = Some(v);
                let n = self.node_mut(v);
                n.next = None;
                n.prev = None;
            }
            Some(_) => {
                if let Some(last) = self.node(parent).last_child {
                    self.node_mut(last).next = Some(v);
                    self.node_mut(v).prev = Some(last);
                }
            }
        }
        self.node_mut(v).parent = Some(parent);
        let p = self.node_mut(parent);
        p.last_child = Some(v);
        p.child_count += 1;
    }

    /// Port of `BaseNode.ReplaceChild`.
    pub fn replace_child(&mut self, parent: NodeId, v1: NodeId, insertee: NodeId) {
        self.insert_before(parent, Some(v1), insertee);
        self.remove_child(parent, v1);
    }

    /// Port of `BaseNode.InsertAfter`.
    pub fn insert_after(&mut self, parent: NodeId, v1: NodeId, insertee: NodeId) {
        let next = self.next_sibling(v1);
        self.insert_before(parent, next, insertee);
    }

    /// Port of `BaseNode.InsertBefore`, with its counter drift (see the module docs).
    pub fn insert_before(&mut self, parent: NodeId, v1: Option<NodeId>, insertee: NodeId) {
        self.node_mut(parent).child_count += 1;
        let Some(c) = v1 else {
            self.append_child(parent, insertee);
            return;
        };
        self.ensure_isolated(insertee);
        if self.parent(c) == Some(parent) {
            let prev = self.previous_sibling(c);
            match prev {
                Some(p) => {
                    self.node_mut(p).next = Some(insertee);
                    self.node_mut(insertee).prev = Some(p);
                }
                None => {
                    self.node_mut(parent).first_child = Some(insertee);
                    self.node_mut(insertee).prev = None;
                }
            }
            self.node_mut(insertee).next = Some(c);
            self.node_mut(c).prev = Some(insertee);
            self.node_mut(insertee).parent = Some(parent);
        }
    }

    /// Port of `ast.MergeOrAppendTextSegment`.
    pub fn merge_or_append_text_segment(&mut self, parent: NodeId, s: Segment) {
        if let Some(last) = self.last_child(parent)
            && let NodeData::Text { segment, flags } = &mut self.node_mut(last).data
            && segment.stop == s.start
            && *flags & TEXT_SOFT_LINE_BREAK == 0
        {
            *segment = segment.with_stop(s.stop);
            return;
        }
        let t = self.new_text_segment(s);
        self.append_child(parent, t);
    }

    /// Port of `ast.MergeOrReplaceTextSegment`.
    pub fn merge_or_replace_text_segment(&mut self, parent: NodeId, n: NodeId, s: Segment) {
        if let Some(prev) = self.previous_sibling(n)
            && let NodeData::Text { segment, flags } = &mut self.node_mut(prev).data
            && segment.stop == s.start
            && *flags & TEXT_SOFT_LINE_BREAK == 0
        {
            *segment = segment.with_stop(s.stop);
            self.remove_child(parent, n);
            return;
        }
        let t = self.new_text_segment(s);
        self.replace_child(parent, n, t);
    }

    /// Port of `ast.Walk`: depth first, `entering` before the children and `!entering` after.
    /// Iterative; links are read when Go's recursion reads them.
    pub fn walk<E>(
        &mut self,
        root: NodeId,
        mut walker: impl FnMut(&mut Ast, NodeId, bool) -> Result<WalkStatus, E>,
    ) -> Result<(), E> {
        enum Frame {
            Enter(NodeId),
            Next { node: NodeId, next: Option<NodeId> },
            After { node: NodeId, child: NodeId },
        }
        let mut stack = vec![Frame::Enter(root)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Enter(n) => match walker(self, n, true)? {
                    WalkStatus::Stop => return Ok(()),
                    WalkStatus::SkipChildren => {
                        if walker(self, n, false)? == WalkStatus::Stop {
                            return Ok(());
                        }
                    }
                    WalkStatus::Continue => {
                        let first = self.first_child(n);
                        stack.push(Frame::Next {
                            node: n,
                            next: first,
                        });
                    }
                },
                Frame::Next { node, next } => match next {
                    Some(c) => {
                        stack.push(Frame::After { node, child: c });
                        stack.push(Frame::Enter(c));
                    }
                    None => {
                        if walker(self, node, false)? == WalkStatus::Stop {
                            return Ok(());
                        }
                    }
                },
                Frame::After { node, child } => {
                    let next = self.next_sibling(child);
                    stack.push(Frame::Next { node, next });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(a: &mut Ast, s: isize, e: isize) -> NodeId {
        a.new_text_segment(Segment::new(s, e))
    }

    #[test]
    fn insert_before_nil_counts_twice() {
        let mut a = Ast::new();
        let p = a.new_node(NodeData::Paragraph);
        let x = text(&mut a, 0, 1);
        a.insert_before(p, None, x);
        assert_eq!(a.child_count(p), 2);
        assert_eq!(a.children(p), vec![x]);
    }

    #[test]
    fn insert_before_foreign_reference_counts_without_inserting() {
        let mut a = Ast::new();
        let p = a.new_node(NodeData::Paragraph);
        let q = a.new_node(NodeData::Paragraph);
        let y = text(&mut a, 0, 1);
        a.append_child(q, y);
        let x = text(&mut a, 1, 2);
        a.insert_before(p, Some(y), x);
        assert_eq!(a.child_count(p), 1);
        assert!(a.children(p).is_empty());
        assert_eq!(a.parent(x), None);
    }

    #[test]
    fn replace_and_insert_after_keep_order() {
        let mut a = Ast::new();
        let p = a.new_node(NodeData::Paragraph);
        let x = text(&mut a, 0, 1);
        let y = text(&mut a, 1, 2);
        let z = text(&mut a, 2, 3);
        a.append_child(p, x);
        a.append_child(p, y);
        a.insert_after(p, x, z);
        assert_eq!(a.children(p), vec![x, z, y]);
        let w = text(&mut a, 3, 4);
        a.replace_child(p, z, w);
        assert_eq!(a.children(p), vec![x, w, y]);
        assert_eq!(a.child_count(p), 3);
        a.remove_child(p, x);
        assert_eq!(a.first_child(p), Some(w));
        assert_eq!(a.previous_sibling(w), None);
    }

    #[test]
    fn remove_child_of_another_parent_is_a_noop() {
        let mut a = Ast::new();
        let p = a.new_node(NodeData::Paragraph);
        let q = a.new_node(NodeData::Paragraph);
        let x = text(&mut a, 0, 1);
        a.append_child(q, x);
        a.remove_child(p, x);
        assert_eq!(a.parent(x), Some(q));
        assert_eq!(a.child_count(p), 0);
    }

    #[test]
    fn merge_or_append_respects_soft_break_and_adjacency() {
        let mut a = Ast::new();
        let p = a.new_node(NodeData::Paragraph);
        a.merge_or_append_text_segment(p, Segment::new(0, 2));
        a.merge_or_append_text_segment(p, Segment::new(2, 4));
        assert_eq!(a.children(p).len(), 1);
        let t = a.children(p)[0];
        assert_eq!(a.text_segment(t), Some(Segment::new(0, 4)));
        a.set_soft_line_break(t, true);
        a.merge_or_append_text_segment(p, Segment::new(4, 5));
        assert_eq!(a.children(p).len(), 2);
        a.merge_or_append_text_segment(p, Segment::new(9, 10));
        assert_eq!(a.children(p).len(), 3);
    }

    #[test]
    fn walk_order_skip_and_stop() {
        let mut a = Ast::new();
        let r = a.root();
        let p = a.new_node(NodeData::Paragraph);
        let q = a.new_node(NodeData::Paragraph);
        a.append_child(r, p);
        a.append_child(r, q);
        let x = text(&mut a, 0, 1);
        a.append_child(p, x);
        let mut seen = Vec::new();
        a.walk::<()>(r, |ast, n, e| {
            seen.push((ast.kind(n), e));
            Ok(if n == p && e {
                WalkStatus::SkipChildren
            } else {
                WalkStatus::Continue
            })
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (NodeKind::Document, true),
                (NodeKind::Paragraph, true),
                (NodeKind::Paragraph, false),
                (NodeKind::Paragraph, true),
                (NodeKind::Paragraph, false),
                (NodeKind::Document, false),
            ]
        );
        let mut count = 0;
        a.walk::<()>(r, |_, _, _| {
            count += 1;
            Ok(if count == 2 {
                WalkStatus::Stop
            } else {
                WalkStatus::Continue
            })
        })
        .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn walk_propagates_errors() {
        let mut a = Ast::new();
        let r = a.root();
        assert_eq!(a.walk(r, |_, _, _| Err::<WalkStatus, _>(7)), Err(7));
    }

    #[test]
    fn pos_per_kind() {
        let mut a = Ast::new();
        assert_eq!(a.pos(a.root()), 0);
        let p = a.new_node(NodeData::Paragraph);
        assert_eq!(a.pos(p), -1);
        a.set_pos(p, 9);
        assert_eq!(a.pos(p), -1);
        a.lines_mut(p).append(Segment::new(3, 5));
        assert_eq!(a.pos(p), 3);
        let s = a.new_node(NodeData::String {
            value: b"x".to_vec(),
            flags: 0,
        });
        a.set_pos(s, 4);
        assert_eq!(a.pos(s), -1);
        let e = a.new_node(NodeData::Emphasis { level: 1 });
        assert_eq!(a.pos(e), -1);
        a.set_pos(e, 4);
        assert_eq!(a.pos(e), 4);
    }
}
