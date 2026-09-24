//! Port of `text/template/parse/node.go`: the parse tree.
//!
//! Go has one `Node` interface for statements and expressions alike; this port splits them into
//! [`Node`] (what a [`ListNode`] holds) and [`Arg`] (what a [`CommandNode`] holds), which is the
//! partition the parser already guarantees. Each node's `String()` is reproduced by its
//! `Display`, because execution and escaping errors quote it.
//!
//! The nodes `html/template` edits (text, action, template call) carry an `id`. Go edits those
//! nodes in place through pointers; this port records edits by id and rebuilds the trees that
//! hold them (see `html::escape`).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The text a tree was parsed from and the name errors report it under (`Tree.ParseName`).
#[derive(Debug)]
pub(crate) struct Src {
    pub parse_name: String,
    pub text: Arc<str>,
}

/// A node's link back to its tree (`node.tree()`); `None` for nodes `html/template` synthesises.
pub(crate) type SrcRef = Option<Arc<Src>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh node identity, standing in for the pointer identity Go's escaper keys its edits by.
pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// `Tree.ErrorContext` (parse.go:156): `"name:line:col"` for a byte position.
pub(crate) fn error_location(pos: usize, src: &Src) -> String {
    let text = src.text.get(..pos).unwrap_or(&src.text);
    let byte_num = match text.rfind('\n') {
        None => pos,
        Some(i) => pos - (i + 1),
    };
    let line_num = 1 + text.matches('\n').count();
    format!("{}:{}:{}", src.parse_name, line_num, byte_num)
}

/// `ListNode`.
#[derive(Debug, Clone)]
pub(crate) struct ListNode {
    pub pos: usize,
    pub src: SrcRef,
    pub nodes: Vec<Node>,
}

/// The statement-level nodes a list can hold.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    Text(TextNode),
    Action(ActionNode),
    If(BranchNode),
    Range(BranchNode),
    With(BranchNode),
    Template(TemplateNode),
    Break(PosNode),
    Continue(PosNode),
    Comment(CommentNode),
}

#[derive(Debug, Clone)]
pub(crate) struct TextNode {
    pub id: u64,
    pub pos: usize,
    pub src: SrcRef,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CommentNode {
    pub pos: usize,
    pub src: SrcRef,
    pub text: String,
}

/// `BreakNode` / `ContinueNode` (and the position-only `DotNode` / `NilNode`).
#[derive(Debug, Clone)]
pub(crate) struct PosNode {
    pub pos: usize,
    pub src: SrcRef,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ActionNode {
    pub id: u64,
    pub pos: usize,
    pub src: SrcRef,
    pub pipe: PipeNode,
}

/// `BranchNode`: the shared body of `if`, `range` and `with`.
#[derive(Debug, Clone)]
pub(crate) struct BranchNode {
    pub pos: usize,
    pub src: SrcRef,
    pub pipe: PipeNode,
    pub list: ListNode,
    pub else_list: Option<ListNode>,
}

#[derive(Debug, Clone)]
pub(crate) struct TemplateNode {
    pub id: u64,
    pub pos: usize,
    pub src: SrcRef,
    pub line: usize,
    pub name: String,
    pub pipe: Option<PipeNode>,
}

#[derive(Debug, Clone)]
pub(crate) struct PipeNode {
    pub pos: usize,
    pub src: SrcRef,
    /// The line of the pipeline's first token; a branch node's `Line` is its pipeline's.
    pub line: usize,
    pub is_assign: bool,
    pub decl: Vec<VariableNode>,
    pub cmds: Vec<CommandNode>,
}

#[derive(Debug, Clone)]
pub(crate) struct CommandNode {
    pub pos: usize,
    pub src: SrcRef,
    pub args: Vec<Arg>,
}

/// The operand-level nodes a command can hold.
#[derive(Debug, Clone)]
pub(crate) enum Arg {
    Field(FieldNode),
    Chain(ChainNode),
    Identifier(IdentifierNode),
    Pipe(Box<PipeNode>),
    Variable(VariableNode),
    Bool(BoolNode),
    Dot(PosNode),
    Nil(PosNode),
    Number(NumberNode),
    String(StringNode),
}

#[derive(Debug, Clone)]
pub(crate) struct FieldNode {
    pub pos: usize,
    pub src: SrcRef,
    pub ident: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChainNode {
    pub pos: usize,
    pub src: SrcRef,
    pub node: Box<Arg>,
    pub field: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct IdentifierNode {
    pub pos: usize,
    pub src: SrcRef,
    pub ident: String,
}

#[derive(Debug, Clone)]
pub(crate) struct VariableNode {
    pub pos: usize,
    pub src: SrcRef,
    pub ident: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct BoolNode {
    pub pos: usize,
    pub src: SrcRef,
    pub value: bool,
}

/// `NumberNode`. Complex constants are rejected at parse time by this port (see `parse`), so
/// the complex fields are absent.
#[derive(Debug, Clone)]
pub(crate) struct NumberNode {
    pub pos: usize,
    pub src: SrcRef,
    pub is_int: bool,
    pub is_uint: bool,
    pub is_float: bool,
    pub int64: i64,
    pub uint64: u64,
    pub float64: f64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct StringNode {
    pub pos: usize,
    pub src: SrcRef,
    pub quoted: String,
    pub text: String,
}

impl Arg {
    pub(crate) fn pos(&self) -> usize {
        match self {
            Arg::Field(n) => n.pos,
            Arg::Chain(n) => n.pos,
            Arg::Identifier(n) => n.pos,
            Arg::Pipe(n) => n.pos,
            Arg::Variable(n) => n.pos,
            Arg::Bool(n) => n.pos,
            Arg::Dot(n) | Arg::Nil(n) => n.pos,
            Arg::Number(n) => n.pos,
            Arg::String(n) => n.pos,
        }
    }

    pub(crate) fn src(&self) -> Option<&Arc<Src>> {
        match self {
            Arg::Field(n) => n.src.as_ref(),
            Arg::Chain(n) => n.src.as_ref(),
            Arg::Identifier(n) => n.src.as_ref(),
            Arg::Pipe(n) => n.src.as_ref(),
            Arg::Variable(n) => n.src.as_ref(),
            Arg::Bool(n) => n.src.as_ref(),
            Arg::Dot(n) | Arg::Nil(n) => n.src.as_ref(),
            Arg::Number(n) => n.src.as_ref(),
            Arg::String(n) => n.src.as_ref(),
        }
    }
}

impl Node {
    pub(crate) fn pos(&self) -> usize {
        match self {
            Node::Text(n) => n.pos,
            Node::Action(n) => n.pos,
            Node::If(n) | Node::Range(n) | Node::With(n) => n.pos,
            Node::Template(n) => n.pos,
            Node::Break(n) | Node::Continue(n) => n.pos,
            Node::Comment(n) => n.pos,
        }
    }

    pub(crate) fn src(&self) -> Option<&Arc<Src>> {
        match self {
            Node::Text(n) => n.src.as_ref(),
            Node::Action(n) => n.src.as_ref(),
            Node::If(n) | Node::Range(n) | Node::With(n) => n.src.as_ref(),
            Node::Template(n) => n.src.as_ref(),
            Node::Break(n) | Node::Continue(n) => n.src.as_ref(),
            Node::Comment(n) => n.src.as_ref(),
        }
    }
}

impl fmt::Display for ListNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for n in &self.nodes {
            write!(f, "{n}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Text(n) => f.write_str(&n.text),
            Node::Action(n) => write!(f, "{{{{{}}}}}", n.pipe),
            Node::If(n) => write_branch(f, "if", n),
            Node::Range(n) => write_branch(f, "range", n),
            Node::With(n) => write_branch(f, "with", n),
            Node::Template(n) => write!(f, "{n}"),
            Node::Break(_) => f.write_str("{{break}}"),
            Node::Continue(_) => f.write_str("{{continue}}"),
            Node::Comment(n) => write!(f, "{{{{{}}}}}", n.text),
        }
    }
}

impl fmt::Display for TemplateNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{{{template {}", crate::strconv::quote(&self.name))?;
        if let Some(p) = &self.pipe {
            write!(f, " {p}")?;
        }
        f.write_str("}}")
    }
}

fn write_branch(f: &mut fmt::Formatter<'_>, name: &str, b: &BranchNode) -> fmt::Result {
    write!(f, "{{{{{name} {}}}}}{}", b.pipe, b.list)?;
    if let Some(e) = &b.else_list {
        write!(f, "{{{{else}}}}{e}")?;
    }
    f.write_str("{{end}}")
}

impl fmt::Display for PipeNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.decl.is_empty() {
            for (i, v) in self.decl.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{v}")?;
            }
            f.write_str(if self.is_assign { " = " } else { " := " })?;
        }
        for (i, c) in self.cmds.iter().enumerate() {
            if i > 0 {
                f.write_str(" | ")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl fmt::Display for CommandNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, a) in self.args.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            if let Arg::Pipe(p) = a {
                write!(f, "({p})")?;
                continue;
            }
            write!(f, "{a}")?;
        }
        Ok(())
    }
}

impl fmt::Display for VariableNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.ident.join("."))
    }
}

impl fmt::Display for FieldNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for id in &self.ident {
            write!(f, ".{id}")?;
        }
        Ok(())
    }
}

impl fmt::Display for ChainNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Arg::Pipe(p) = &*self.node {
            write!(f, "({p})")?;
        } else {
            write!(f, "{}", self.node)?;
        }
        for field in &self.field {
            write!(f, ".{field}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::Field(n) => write!(f, "{n}"),
            Arg::Chain(n) => write!(f, "{n}"),
            Arg::Identifier(n) => f.write_str(&n.ident),
            Arg::Pipe(n) => write!(f, "{n}"),
            Arg::Variable(n) => write!(f, "{n}"),
            Arg::Bool(n) => f.write_str(if n.value { "true" } else { "false" }),
            Arg::Dot(_) => f.write_str("."),
            Arg::Nil(_) => f.write_str("nil"),
            Arg::Number(n) => f.write_str(&n.text),
            Arg::String(n) => f.write_str(&n.quoted),
        }
    }
}

/// `IsEmptyTree` (parse.go:302): a tree of nothing but space (Go's `unicode.IsSpace`) and
/// comments.
pub(crate) fn is_empty_list(l: &ListNode) -> bool {
    l.nodes.iter().all(|n| match n {
        Node::Text(t) => t.text.trim_matches(char::is_whitespace).is_empty(),
        Node::Comment(_) => true,
        _ => false,
    })
}

/// `ListNode.CopyList`: a deep copy whose editable nodes get fresh identities, as Go's copy gets
/// fresh pointers. Source links are kept, so errors in a copy still point into the original text.
pub(crate) fn copy_list(l: &ListNode) -> ListNode {
    ListNode {
        pos: l.pos,
        src: l.src.clone(),
        nodes: l.nodes.iter().map(copy_node).collect(),
    }
}

fn copy_node(n: &Node) -> Node {
    match n {
        Node::Text(t) => Node::Text(TextNode {
            id: next_id(),
            ..t.clone()
        }),
        Node::Action(a) => Node::Action(ActionNode {
            id: next_id(),
            ..a.clone()
        }),
        Node::If(b) => Node::If(copy_branch(b)),
        Node::Range(b) => Node::Range(copy_branch(b)),
        Node::With(b) => Node::With(copy_branch(b)),
        Node::Template(t) => Node::Template(TemplateNode {
            id: next_id(),
            ..t.clone()
        }),
        other => other.clone(),
    }
}

fn copy_branch(b: &BranchNode) -> BranchNode {
    BranchNode {
        pos: b.pos,
        src: b.src.clone(),
        pipe: b.pipe.clone(),
        list: copy_list(&b.list),
        else_list: b.else_list.as_ref().map(copy_list),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_location_counts_lines_and_bytes() {
        let src = Src {
            parse_name: "f".into(),
            text: Arc::from("ab\ncd{{.X}}"),
        };
        assert_eq!(error_location(1, &src), "f:1:1");
        assert_eq!(error_location(5, &src), "f:2:2");
        assert_eq!(error_location(3, &src), "f:2:0");
    }

    #[test]
    fn empty_tree_is_space_and_comments() {
        let text = |t: &str| {
            Node::Text(TextNode {
                id: 0,
                pos: 0,
                src: None,
                text: t.into(),
            })
        };
        let l = ListNode {
            pos: 0,
            src: None,
            nodes: vec![text(" \n\u{a0}\t")],
        };
        assert!(is_empty_list(&l));
        let l2 = ListNode {
            pos: 0,
            src: None,
            nodes: vec![text(" x ")],
        };
        assert!(!is_empty_list(&l2));
    }
}
