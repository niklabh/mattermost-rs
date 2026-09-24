//! Port of goldmark's `renderer` package (`renderer/renderer.go`): a walk over the AST that
//! dispatches each node to the function registered for its kind.
//!
//! Registration follows `renderer.Render`'s `initSync` exactly: the node renderers are sorted
//! ascending by priority — a *stable* sort, which Go's `sort.Slice` is below twelve elements —
//! and registered from the last to the first, so for a kind claimed twice the lowest priority
//! value wins, and between equal priorities the one added **first** wins. Mattermost's
//! `StripMarkdown` depends on the second rule: its notification renderer (added first, 500) and
//! the Strikethrough extension's HTML renderer (added by `Extend`, also 500) both claim
//! `Strikethrough`, and the notification renderer's no-op is what runs.

use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;

use crate::ast::{Ast, NodeId, NodeKind, WalkStatus};

/// Errors a render can produce. goldmark's renderer only fails when the writer does or when a
/// node renderer function returns an error.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The output writer failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A node renderer function failed.
    #[error(transparent)]
    Node(Box<dyn std::error::Error + Send + Sync>),
}

/// Port of `renderer.NodeRendererFunc`. The writer is the in-memory buffer goldmark's
/// `bufio.Writer` stands for; `source` is the markdown; the AST is mutable because goldmark's
/// table cell renderer sets an attribute while rendering.
pub type NodeRendererFunc =
    Rc<dyn Fn(&mut Vec<u8>, &[u8], &mut Ast, NodeId, bool) -> Result<WalkStatus, RenderError>>;

/// Port of `renderer.NodeRendererFuncRegisterer`.
pub trait NodeRendererFuncRegisterer {
    /// Register `f` for `kind`, replacing any earlier registration.
    fn register(&mut self, kind: NodeKind, f: NodeRendererFunc);
}

/// Port of `renderer.NodeRenderer`.
pub trait NodeRenderer {
    /// Port of `RegisterFuncs`.
    fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer);
}

#[derive(Default)]
struct Registry {
    funcs: HashMap<NodeKind, NodeRendererFunc>,
}

impl NodeRendererFuncRegisterer for Registry {
    fn register(&mut self, kind: NodeKind, f: NodeRendererFunc) {
        self.funcs.insert(kind, f);
    }
}

/// Port of `renderer.renderer` configured with `WithNodeRenderers`.
#[derive(Clone, Default)]
pub struct Renderer {
    node_renderers: Vec<(Rc<dyn NodeRenderer>, i32)>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("node_renderers", &self.node_renderers.len())
            .finish()
    }
}

impl Renderer {
    /// `renderer.NewRenderer()` with no node renderers.
    pub fn new() -> Self {
        Renderer::default()
    }

    /// `renderer.WithNodeRenderers(util.Prioritized(r, priority))`.
    pub fn with_node_renderer(mut self, r: Rc<dyn NodeRenderer>, priority: i32) -> Self {
        self.add_node_renderer(r, priority);
        self
    }

    /// `Renderer.AddOptions(renderer.WithNodeRenderers(...))`.
    pub fn add_node_renderer(&mut self, r: Rc<dyn NodeRenderer>, priority: i32) {
        self.node_renderers.push((r, priority));
    }

    fn registry(&self) -> Registry {
        let mut sorted = self.node_renderers.clone();
        sorted.sort_by_key(|&(_, p)| p);
        let mut reg = Registry::default();
        for (r, _) in sorted.into_iter().rev() {
            r.register_funcs(&mut reg);
        }
        reg
    }

    /// Port of `renderer.Render`: walk `ast` from `node`, writing into `w`. Kinds with no
    /// registered function are walked through silently.
    pub fn render(
        &self,
        w: &mut dyn Write,
        source: &[u8],
        ast: &mut Ast,
        node: NodeId,
    ) -> Result<(), RenderError> {
        let reg = self.registry();
        let mut buf = Vec::with_capacity(source.len() * 2 + 16);
        ast.walk(node, |ast, n, entering| match reg.funcs.get(&ast.kind(n)) {
            Some(f) => f(&mut buf, source, ast, n, entering),
            None => Ok(WalkStatus::Continue),
        })?;
        w.write_all(&buf)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NodeData;

    struct Tag(&'static [u8]);
    impl NodeRenderer for Tag {
        fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer) {
            let me = self.0;
            reg.register(
                NodeKind::Paragraph,
                Rc::new(move |w, _, _, _, entering| {
                    if entering {
                        w.extend_from_slice(me);
                    }
                    Ok(WalkStatus::Continue)
                }),
            );
        }
    }

    fn render(r: &Renderer) -> Vec<u8> {
        let mut ast = Ast::new();
        let p = ast.new_node(NodeData::Paragraph);
        let root = ast.root();
        ast.append_child(root, p);
        let mut out = Vec::new();
        r.render(&mut out, b"", &mut ast, root).unwrap();
        out
    }

    #[test]
    fn lowest_priority_wins_and_ties_go_to_the_first_added() {
        let r = Renderer::new()
            .with_node_renderer(Rc::new(Tag(b"a")), 500)
            .with_node_renderer(Rc::new(Tag(b"b")), 500);
        assert_eq!(render(&r), b"a");
        let r = Renderer::new()
            .with_node_renderer(Rc::new(Tag(b"a")), 1000)
            .with_node_renderer(Rc::new(Tag(b"b")), 500);
        assert_eq!(render(&r), b"b");
        let r = Renderer::new()
            .with_node_renderer(Rc::new(Tag(b"a")), 500)
            .with_node_renderer(Rc::new(Tag(b"b")), 1000);
        assert_eq!(render(&r), b"a");
    }

    #[test]
    fn node_errors_propagate() {
        struct Fail;
        impl NodeRenderer for Fail {
            fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer) {
                reg.register(
                    NodeKind::Document,
                    Rc::new(|_, _, _, _, _| {
                        Err(RenderError::Node(Box::new(std::io::Error::other("boom"))))
                    }),
                );
            }
        }
        let r = Renderer::new().with_node_renderer(Rc::new(Fail), 1);
        let mut ast = Ast::new();
        let root = ast.root();
        let mut out = Vec::new();
        assert!(matches!(
            r.render(&mut out, b"", &mut ast, root),
            Err(RenderError::Node(_))
        ));
    }
}
