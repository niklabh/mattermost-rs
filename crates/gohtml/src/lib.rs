//! Byte-exact port of `golang.org/x/net@v0.56.0/html` — the version Mattermost's `server/go.mod`
//! pins — as far as Mattermost uses it:
//!
//! * [`token`]: the tokenizer, which the link-preview code (`mm_model::go_html`) reads `<meta>`
//!   tags off directly;
//! * [`parse()`]: `html.Parse`, the HTML5 tree builder `jaytaylor/html2text` (crate
//!   `gohtml2text`) runs over every e-mail body;
//! * [`atom`], [`entity`]: the generated tables both use.
//!
//! Not Mattermost source: BSD-3-Clause, like the Go it is ported from (./LICENSE). This crate must
//! never depend on an `mm-*` crate.
//!
//! Not ported: `ParseFragment`, `Render`, the `Node` iterators (iter.go) and the `charset`
//! sub-package (mm-model keeps its own `go_charset`).

/// The README's examples, compiled and run as doctests so the crates.io page cannot drift.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod atom;
pub mod entity;
mod gostrings;
pub mod node;
pub mod parse;
pub mod token;

pub use atom::Atom;
pub use node::{Attribute, Document, Node, NodeId, NodeType};
pub use parse::{ParseError, ParseOptions, parse, parse_with_options};

#[cfg(test)]
mod tests {
    use super::atom::{Atom, lookup};

    /// `atom.Lookup` is case-sensitive and knows attribute names as well as tag names. The one
    /// mixed-case atom is `foreignObject`, which the SVG tag-name adjustment looks up; the table
    /// also holds `foreignobject`, which is what the tokenizer's lower-cased name finds first.
    #[test]
    fn atom_lookup_is_go_lookup() {
        assert_eq!(lookup(b"a"), Some(Atom::A));
        assert_eq!(lookup(b"foreignObject"), Some(Atom::ForeignObject));
        assert_eq!(lookup(b"foreignobject"), Some(Atom::Foreignobject));
        assert_eq!(lookup(b"FOREIGNOBJECT"), None);
        assert_eq!(lookup(b"clipPath"), None);
        assert_eq!(lookup(b"alt"), Some(Atom::Alt));
        assert_eq!(lookup(b"annotation-xml"), Some(Atom::AnnotationXml));
        assert_eq!(lookup(b""), None);
        assert_eq!(Atom::AnnotationXml.as_str(), "annotation-xml");
    }
}
