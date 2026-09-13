//! Port of `inspect.go`: `Inspect`, `InspectBlock`, `InspectInline`, and the post-size limit
//! `MaxLen` / `SetMaxPostRunes`.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::blocks::Block;
use crate::inlines::{Inline, merge_inline_text};

/// Port of `defaultMaxPostRunes` (inspect.go:14): the value in force until
/// [`set_max_post_runes`] is called — 64 KiB of post at four bytes per rune, doubled.
pub const DEFAULT_MAX_POST_RUNES: usize = 2 * 16 * 1024;

static MAX_POST_RUNES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_POST_RUNES);

/// Port of `markdown.SetMaxPostRunes` (inspect.go:26): registers the configured maximum post
/// size, in runes, from which [`max_len`] derives the byte limit. A process-wide atomic, as in
/// Go.
///
/// **Call site to wire:** the Go server calls it once, after the store is created —
/// `markdown.SetMaxPostRunes(ps.MaxPostSize())` at `app/platform/service.go:338`, where
/// `MaxPostSize()` (`app/platform/config.go:392`) is `Store.Post().GetMaxPostSize()`: the
/// `Posts.Message` column's declared length, or `model.PostMessageMaxRunesV2` (65,535) when the
/// store cannot say. The app layer owns that call; this crate only holds the value.
pub fn set_max_post_runes(size: usize) {
    MAX_POST_RUNES.store(size, Ordering::SeqCst);
}

/// Port of `markdown.MaxLen` (inspect.go:34): the maximum input length **in bytes** that
/// [`crate::parse`] and [`inspect`] accept — four times the registered rune count, the worst case
/// of four bytes per rune. Default 131,072.
pub fn max_len() -> usize {
    MAX_POST_RUNES.load(Ordering::SeqCst).saturating_mul(4)
}

/// One node of the parsed tree, as handed to an [`inspect`] callback. Go passes `any` and the
/// callback type-switches; the two enums carry the same information.
#[derive(Clone, Copy, Debug)]
pub enum Node<'a> {
    Block(&'a Block<'a>),
    Inline(&'a Inline<'a>),
}

impl Node<'_> {
    /// `Some(text)` when this is an inline `*markdown.Text` node, else `None`.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Node::Inline(Inline::Text(t)) => Some(&t.text),
            _ => None,
        }
    }
}

/// Port of `markdown.Inspect` (inspect.go:41). Calls `f` for every block and inline in
/// depth-first order — for a paragraph, its **merged** inlines right after the paragraph itself
/// — and `f(None)` after each node's children.
///
/// Two things the Go doc comment does not say, both pinned by the corpus:
///
/// - `f(None)` follows a node **whether or not `f` returned true for it**. A refused node stays
///   on Go's traversal stack and is popped, with the nil call, on the next iteration
///   (inspect.go:83). Refusing only skips the children.
/// - An input longer than [`max_len`] produces no callbacks at all, not even for the document.
///
/// Traversal is iterative, so nesting depth costs no stack.
pub fn inspect(markdown: &str, mut f: impl FnMut(Option<&Node<'_>>) -> bool) {
    if markdown.len() > max_len() {
        return;
    }
    let (document, reference_definitions) = crate::parse(markdown);
    let root = Block::Document(document);
    inspect_block(&root, |block| {
        let Some(block) = block else {
            return f(None);
        };
        if !f(Some(&Node::Block(block))) {
            return false;
        }
        if let Block::Paragraph(paragraph) = block {
            for inline in merge_inline_text(paragraph.parse_inlines(&reference_definitions)) {
                inspect_inline(&inline, |inline| match inline {
                    None => f(None),
                    Some(inline) => f(Some(&Node::Inline(inline))),
                });
            }
        }
        true
    });
}

/// Port of `markdown.InspectBlock` (inspect.go:66): blocks only, depth first, `f(None)` after
/// each block's children (and after a refused block).
pub fn inspect_block<'a>(block: &'a Block<'a>, mut f: impl FnMut(Option<&'a Block<'a>>) -> bool) {
    enum Frame<'a> {
        Enter(&'a Block<'a>),
        Exit,
    }
    let mut stack = vec![Frame::Enter(block)];
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Exit => {
                f(None);
            }
            Frame::Enter(block) => {
                stack.push(Frame::Exit);
                if !f(Some(block)) {
                    continue;
                }
                for child in block.children().iter().rev() {
                    stack.push(Frame::Enter(child));
                }
            }
        }
    }
}

/// Port of `markdown.InspectInline` (inspect.go:106): inlines only, descending into the four
/// link and image kinds (not into an `Autolink`), `f(None)` after each node's children (and
/// after a refused node).
pub fn inspect_inline<'a>(
    inline: &'a Inline<'a>,
    mut f: impl FnMut(Option<&'a Inline<'a>>) -> bool,
) {
    enum Frame<'a> {
        Enter(&'a Inline<'a>),
        Exit,
    }
    let mut stack = vec![Frame::Enter(inline)];
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Exit => {
                f(None);
            }
            Frame::Enter(inline) => {
                stack.push(Frame::Exit);
                if !f(Some(inline)) {
                    continue;
                }
                for child in inline.inspected_children().iter().rev() {
                    stack.push(Frame::Enter(child));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(markdown: &str, mut accept: impl FnMut(&Node<'_>) -> bool) -> Vec<String> {
        let mut out = Vec::new();
        inspect(markdown, |node| match node {
            None => {
                out.push("nil".to_owned());
                true
            }
            Some(node) => {
                out.push(match node {
                    Node::Block(b) => format!("B:{}", block_name(b)),
                    Node::Inline(i) => match i {
                        Inline::Text(t) => format!("T:{}", t.text),
                        other => format!("I:{}", inline_name(other)),
                    },
                });
                accept(node)
            }
        });
        out
    }

    fn block_name(b: &Block<'_>) -> &'static str {
        match b {
            Block::Document(_) => "Document",
            Block::Paragraph(_) => "Paragraph",
            Block::List(_) => "List",
            Block::ListItem(_) => "ListItem",
            Block::BlockQuote(_) => "BlockQuote",
            Block::FencedCode(_) => "FencedCode",
            Block::IndentedCode(_) => "IndentedCode",
        }
    }

    fn inline_name(i: &Inline<'_>) -> &'static str {
        match i {
            Inline::Text(_) => "Text",
            Inline::CodeSpan(_) => "CodeSpan",
            Inline::HardLineBreak => "HardLineBreak",
            Inline::SoftLineBreak => "SoftLineBreak",
            Inline::InlineLink(_) => "InlineLink",
            Inline::InlineImage(_) => "InlineImage",
            Inline::ReferenceLink(_) => "ReferenceLink",
            Inline::ReferenceImage(_) => "ReferenceImage",
            Inline::Autolink(_) => "Autolink",
            Inline::Emoji(_) => "Emoji",
        }
    }

    #[test]
    fn order_and_pops() {
        assert_eq!(
            trace("a [b](c)", |_| true),
            vec![
                "B:Document",
                "B:Paragraph",
                "T:a ",
                "nil",
                "I:InlineLink",
                "T:b",
                "nil",
                "nil",
                "nil",
                "nil"
            ]
        );
    }

    #[test]
    fn refusing_a_node_skips_children_but_still_pops() {
        assert_eq!(
            trace("> a", |n| !matches!(n, Node::Block(Block::BlockQuote(_)))),
            vec!["B:Document", "B:BlockQuote", "nil", "nil"]
        );
        assert_eq!(
            trace("[b](c)", |n| !matches!(
                n,
                Node::Inline(Inline::InlineLink(_))
            )),
            vec![
                "B:Document",
                "B:Paragraph",
                "I:InlineLink",
                "nil",
                "nil",
                "nil"
            ]
        );
    }

    #[test]
    fn as_text_is_only_for_text() {
        let mut texts = Vec::new();
        inspect("a `b` c", |node| {
            if let Some(t) = node.and_then(|n| n.as_text()) {
                texts.push(t.to_owned());
            }
            true
        });
        assert_eq!(texts, vec!["a ", " c"]);
    }

    #[test]
    fn max_len_gate() {
        let over = "a".repeat(max_len() + 1);
        let mut calls = 0;
        inspect(&over, |_| {
            calls += 1;
            true
        });
        assert_eq!(calls, 0);
        let (document, _) = crate::parse(&over);
        assert!(document.children.is_empty());
        let exact = "a".repeat(max_len());
        let (document, _) = crate::parse(&exact);
        assert_eq!(document.children.len(), 1);
    }

    #[test]
    fn deep_nesting_inspects_without_recursion() {
        let depth = 20_000;
        let md = format!("{}a{}", "![".repeat(depth), "](u)".repeat(depth));
        let mut images = 0;
        inspect(&md, |node| {
            if matches!(node, Some(Node::Inline(Inline::InlineImage(_)))) {
                images += 1;
            }
            true
        });
        assert_eq!(images, depth);
    }
}
