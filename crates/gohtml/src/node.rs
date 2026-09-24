//! Port of `golang.org/x/net@v0.56.0/html/node.go`: the parse tree.
//!
//! Go links `*Node` values by pointer; here every node lives in one arena ([`Document`]) and links
//! by [`NodeId`]. The link operations are transcribed literally — including what they do to a
//! tree when their preconditions do not hold (`InsertBefore` with an `oldChild` that is not a
//! child of `n`), because the tree builder reaches such states on malformed input and the tree it
//! then produces is the one Go produces.

use std::ops::Index;

use crate::atom::Atom;
pub use crate::token::Attribute;

/// Port of `html.NodeType` (node.go:12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    Error,
    Text,
    Document,
    Element,
    Comment,
    Doctype,
    /// Never produced by the parser; Go reserves it for `Render`.
    Raw,
    /// `scopeMarkerNode`: the marker the parser pushes onto its list of active formatting
    /// elements. It is never linked into the tree.
    ScopeMarker,
}

impl NodeType {
    /// `NodeType.String()` (nodetype_string.go), as `%v` prints it.
    pub fn as_str(self) -> &'static str {
        match self {
            NodeType::Error => "ErrorNode",
            NodeType::Text => "TextNode",
            NodeType::Document => "DocumentNode",
            NodeType::Element => "ElementNode",
            NodeType::Comment => "CommentNode",
            NodeType::Doctype => "DoctypeNode",
            NodeType::Raw => "RawNode",
            NodeType::ScopeMarker => "scopeMarkerNode",
        }
    }
}

/// A node's index in its [`Document`]. Equality is Go's pointer equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

/// Port of `html.Node` (node.go:44). `data_atom` is `None` where Go's `DataAtom` is zero; an empty
/// `namespace` means HTML, and the parser only ever writes `"svg"` or `"math"` into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub node_type: NodeType,
    pub data_atom: Option<Atom>,
    pub data: String,
    pub namespace: String,
    pub attr: Vec<Attribute>,
}

impl Node {
    /// A detached node of type `node_type` with `data` and nothing else.
    pub fn new(node_type: NodeType, data: impl Into<String>) -> Self {
        Node {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            node_type,
            data_atom: None,
            data: data.into(),
            namespace: String::new(),
            attr: Vec::new(),
        }
    }
}

/// A Go panic raised by a link operation, with Go's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkPanic(pub(crate) &'static str);

/// The arena every node of one parse lives in. Nodes the parser detached (a `<body>` replaced by a
/// `<frameset>`, a text node merged into its neighbour) stay in the arena, unreachable from
/// [`Document::root`], exactly as Go leaves them to the garbage collector.
#[derive(Debug, Clone)]
pub struct Document {
    nodes: Vec<Node>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Index<NodeId> for Document {
    type Output = Node;
    fn index(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }
}

impl Document {
    /// An arena holding one `DocumentNode`, the root.
    pub fn new() -> Self {
        Document {
            nodes: vec![Node::new(NodeType::Document, "")],
        }
    }

    /// The `DocumentNode` `html.Parse` returns.
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    /// `n.FirstChild`, `n.FirstChild.NextSibling`, … — the children of `id` in order.
    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(self[id].first_child, move |&c| self[c].next_sibling)
    }

    /// Adds `node` to the arena, detached.
    pub fn alloc(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        NodeId(self.nodes.len() - 1)
    }

    pub(crate) fn get_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0]
    }

    /// Port of `InsertBefore` (node.go:63). Like Go, it trusts that `old_child` (when given) is a
    /// child of `n`; when it is not, the links end up exactly as Go's end up.
    pub(crate) fn insert_before(
        &mut self,
        n: NodeId,
        new_child: NodeId,
        old_child: Option<NodeId>,
    ) -> Result<(), LinkPanic> {
        let c = &self[new_child];
        if c.parent.is_some() || c.prev_sibling.is_some() || c.next_sibling.is_some() {
            return Err(LinkPanic(
                "html: InsertBefore called for an attached child Node",
            ));
        }
        let (prev, next) = match old_child {
            Some(old) => (self[old].prev_sibling, Some(old)),
            None => (self[n].last_child, None),
        };
        match prev {
            Some(p) => self.get_mut(p).next_sibling = Some(new_child),
            None => self.get_mut(n).first_child = Some(new_child),
        }
        match next {
            Some(x) => self.get_mut(x).prev_sibling = Some(new_child),
            None => self.get_mut(n).last_child = Some(new_child),
        }
        let c = self.get_mut(new_child);
        c.parent = Some(n);
        c.prev_sibling = prev;
        c.next_sibling = next;
        Ok(())
    }

    /// Port of `AppendChild` (node.go:92).
    pub(crate) fn append_child(&mut self, n: NodeId, c: NodeId) -> Result<(), LinkPanic> {
        let cn = &self[c];
        if cn.parent.is_some() || cn.prev_sibling.is_some() || cn.next_sibling.is_some() {
            return Err(LinkPanic(
                "html: AppendChild called for an attached child Node",
            ));
        }
        let last = self[n].last_child;
        match last {
            Some(l) => self.get_mut(l).next_sibling = Some(c),
            None => self.get_mut(n).first_child = Some(c),
        }
        self.get_mut(n).last_child = Some(c);
        let cn = self.get_mut(c);
        cn.parent = Some(n);
        cn.prev_sibling = last;
        Ok(())
    }

    /// Port of `RemoveChild` (node.go:112).
    pub(crate) fn remove_child(&mut self, n: NodeId, c: NodeId) -> Result<(), LinkPanic> {
        if self[c].parent != Some(n) {
            return Err(LinkPanic("html: RemoveChild called for a non-child Node"));
        }
        let (prev, next) = (self[c].prev_sibling, self[c].next_sibling);
        if self[n].first_child == Some(c) {
            self.get_mut(n).first_child = next;
        }
        if let Some(x) = next {
            self.get_mut(x).prev_sibling = prev;
        }
        if self[n].last_child == Some(c) {
            self.get_mut(n).last_child = prev;
        }
        if let Some(p) = prev {
            self.get_mut(p).next_sibling = next;
        }
        let cn = self.get_mut(c);
        cn.parent = None;
        cn.prev_sibling = None;
        cn.next_sibling = None;
        Ok(())
    }

    /// Port of `reparentChildren` (node.go:135).
    pub(crate) fn reparent_children(&mut self, dst: NodeId, src: NodeId) -> Result<(), LinkPanic> {
        while let Some(child) = self[src].first_child {
            self.remove_child(src, child)?;
            self.append_child(dst, child)?;
        }
        Ok(())
    }

    /// Port of `clone` (node.go:148): type, atom, data and attributes — **not** the namespace,
    /// which Go's clone leaves empty.
    pub(crate) fn clone_node(&mut self, id: NodeId) -> NodeId {
        let n = &self[id];
        let mut m = Node::new(n.node_type, n.data.as_str());
        m.data_atom = n.data_atom;
        m.attr = n.attr.clone();
        self.alloc(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(doc: &mut Document, s: &str) -> NodeId {
        doc.alloc(Node::new(NodeType::Text, s))
    }

    fn kids(doc: &Document, id: NodeId) -> Vec<String> {
        doc.children(id).map(|c| doc[c].data.clone()).collect()
    }

    #[test]
    fn append_insert_remove_keep_the_links_consistent() {
        let mut d = Document::new();
        let root = d.root();
        let a = text(&mut d, "a");
        let b = text(&mut d, "b");
        let c = text(&mut d, "c");
        d.append_child(root, a).unwrap();
        d.append_child(root, c).unwrap();
        d.insert_before(root, b, Some(c)).unwrap();
        assert_eq!(kids(&d, root), ["a", "b", "c"]);
        assert_eq!(d[c].prev_sibling, Some(b));
        d.remove_child(root, a).unwrap();
        assert_eq!(kids(&d, root), ["b", "c"]);
        assert_eq!(d[b].prev_sibling, None);
        d.remove_child(root, c).unwrap();
        assert_eq!(d[root].last_child, Some(b));
        let e = text(&mut d, "e");
        d.insert_before(root, e, None).unwrap();
        assert_eq!(kids(&d, root), ["b", "e"]);
    }

    #[test]
    fn attached_nodes_panic_as_go_does() {
        let mut d = Document::new();
        let root = d.root();
        let a = text(&mut d, "a");
        d.append_child(root, a).unwrap();
        assert_eq!(
            d.append_child(root, a),
            Err(LinkPanic(
                "html: AppendChild called for an attached child Node"
            ))
        );
        assert_eq!(
            d.insert_before(root, a, None),
            Err(LinkPanic(
                "html: InsertBefore called for an attached child Node"
            ))
        );
        let b = text(&mut d, "b");
        assert_eq!(
            d.remove_child(root, b),
            Err(LinkPanic("html: RemoveChild called for a non-child Node"))
        );
    }

    /// `InsertBefore` with an `oldChild` that has no parent: Go makes the new node `n`'s first
    /// child **and** links it before the orphan, leaving `n`'s other children unreachable from
    /// `FirstChild`. The foster-parenting code reaches this when a table has been detached.
    #[test]
    fn insert_before_an_orphan_relinks_as_go_does() {
        let mut d = Document::new();
        let root = d.root();
        let a = text(&mut d, "a");
        d.append_child(root, a).unwrap();
        let orphan = text(&mut d, "orphan");
        let n = text(&mut d, "n");
        d.insert_before(root, n, Some(orphan)).unwrap();
        assert_eq!(d[root].first_child, Some(n));
        assert_eq!(d[root].last_child, Some(a));
        assert_eq!(d[n].next_sibling, Some(orphan));
        assert_eq!(d[orphan].prev_sibling, Some(n));
    }

    #[test]
    fn clone_drops_the_namespace() {
        let mut d = Document::new();
        let mut n = Node::new(NodeType::Element, "b");
        n.namespace = "svg".into();
        n.data_atom = Some(Atom::B);
        n.attr.push(Attribute {
            namespace: String::new(),
            key: "k".into(),
            val: "v".into(),
        });
        let id = d.alloc(n);
        let c = d.clone_node(id);
        assert_eq!(d[c].namespace, "");
        assert_eq!(d[c].data_atom, Some(Atom::B));
        assert_eq!(d[c].attr, d[id].attr);
        assert_ne!(c, id);
    }
}
