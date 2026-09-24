# gohtml

A byte-exact Rust port of [`golang.org/x/net/html`](https://pkg.go.dev/golang.org/x/net/html)
v0.56.0: its tokenizer, its HTML5 tree builder (`html.Parse`) and the atom and entity tables they
share.

```rust
use gohtml::token::{TokenType, Tokenizer};
use gohtml::{NodeId, NodeType, parse};

// The tree builder, with HTML5's implied elements and implicit closes.
let doc = parse("<p>One<p>Two").unwrap();
fn elements(doc: &gohtml::Document, id: NodeId, out: &mut Vec<String>) {
    if doc[id].node_type == NodeType::Element {
        out.push(doc[id].data.clone());
    }
    for child in doc.children(id) {
        elements(doc, child, out);
    }
}
let mut names = Vec::new();
elements(&doc, doc.root(), &mut names);
assert_eq!(names, ["html", "head", "body", "p", "p"]);

// The tokenizer, with entities decoded in attribute values.
let mut z = Tokenizer::new(br#"<a href="/x?a=1&amp;b=2">hi</a>"#);
let mut seen = Vec::new();
while z.next_token() != TokenType::Error {
    let t = z.token();
    seen.push((t.token_type, t.data, t.attr.into_iter().map(|a| a.val).collect::<Vec<_>>()));
}
assert_eq!(seen, [
    (TokenType::StartTag, "a".to_owned(), vec!["/x?a=1&b=2".to_owned()]),
    (TokenType::Text, "hi".to_owned(), vec![]),
    (TokenType::EndTag, "a".to_owned(), vec![]),
]);
```

Both results are what x/net/html produces for the same input.

## Why a port

Rust has excellent HTML5 parsers. This one exists for when you need the tree or token stream
*Go* builds — including where x/net/html departs from the specification — because another
program's output depends on it. It is tested against x/net/html itself: a Go program records the
token stream and parse tree for a corpus of documents, and the tests assert equality.

## Differences from Go

- Nodes live in an arena (`Document`, indexed by `NodeId`) instead of being linked by
  pointers; the sibling and parent links are the same.
- Where Go panics and `html.Parse` recovers the panic into its error, the port returns that error
  with Go's message.
- Not ported: `ParseFragment`, `Render`, the node iterators and the `charset` sub-package.

## Licence

BSD-3-Clause, the licence of the Go code it is ported from (`LICENSE`).
