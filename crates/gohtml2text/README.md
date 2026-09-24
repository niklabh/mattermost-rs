# gohtml2text

A byte-exact Rust port of [`github.com/jaytaylor/html2text`](https://github.com/jaytaylor/html2text)
(and the `CleanBom` it calls from `github.com/ssor/bom`): HTML in, readable plain text out, the
way Go programs — Mattermost's mail sender among them — build the `text/plain` alternative of an
HTML e-mail.

```rust
let html = r#"<h1>Welcome</h1><p>Read the <a href="https://example.com/docs">docs</a>, <b>today</b>.</p><ul><li>one</li><li>two</li></ul>"#;

assert_eq!(
    gohtml2text::from_string(html).unwrap(),
    "*******\nWelcome\n*******\n\nRead the docs ( https://example.com/docs ) , *today*.\n\n* one\n* two",
);

let text_only = gohtml2text::Options { text_only: true, ..Default::default() };
assert_eq!(
    gohtml2text::from_string_with_options(html, text_only).unwrap(),
    "Welcome.\n\nRead the docs , today..\n\none\ntwo",
);
```

Both outputs are html2text's own for the same input, quirks included: the space before the
comma, and the doubled full stop under `TextOnly`.

## Why a port

When a Rust program has to produce the same text a Go program would — the same line breaks,
link rendering and heading dividers — any other HTML-to-text converter produces *a* text, not
*the* text. The document is parsed by `gohtml`(https://crates.io/crates/gohtml), a byte-exact
port of the `golang.org/x/net/html` parser html2text runs on, and the output is tested against
html2text itself over a corpus of documents.

Lengths are Go's: the heading divider counts runes, and "space" is Go's `unicode.IsSpace`.

## Not ported

`PrettyTables`, which renders tables through `olekukonko/tablewriter`; tables always take the
plain paragraph path.

## Licence

MIT, the licence of the Go code it is ported from (`LICENSE-HTML2TEXT`, `LICENSE-BOM`).
