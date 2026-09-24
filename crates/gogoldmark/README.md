# gogoldmark

A byte-exact Rust port of [goldmark](https://github.com/yuin/goldmark) v1.8.2 — Go's CommonMark
parser, its default HTML renderer, and the GFM extensions (Strikethrough, Table, Linkify and
TaskList).

```rust
use gogoldmark::{Extension, convert};

let md = "# Hi\n\n~~old~~ see www.example.com\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n- [x] done\n";
assert_eq!(
    convert(md, &[Extension::Gfm]),
    "<h1>Hi</h1>\n\
     <p><del>old</del> see <a href=\"http://www.example.com\">www.example.com</a></p>\n\
     <table>\n<thead>\n<tr>\n<th>a</th>\n<th style=\"text-align:right\">b</th>\n</tr>\n</thead>\n\
     <tbody>\n<tr>\n<td>1</td>\n<td style=\"text-align:right\">2</td>\n</tr>\n</tbody>\n</table>\n\
     <ul>\n<li><input checked=\"\" disabled=\"\" type=\"checkbox\"> done</li>\n</ul>\n",
);

// goldmark's default: raw HTML is omitted, not passed through.
assert_eq!(
    convert("*emph* and <b>raw</b>\n", &[]),
    "<p><em>emph</em> and <!-- raw HTML omitted -->raw<!-- raw HTML omitted --></p>\n",
);
```

Both outputs are goldmark's own for the same input.

## Why a port

CommonMark leaves room between implementations — link reference edge cases, HTML block
boundaries, autolink extents, table alignment markup — and a different renderer produces *a*
rendering, not goldmark's. This crate ports goldmark's parser and renderer and is tested against
goldmark itself: a Go program converts a corpus under several extension sets and records the
AST, and the tests assert byte equality.

The AST (`Ast`, `NodeKind`) and the renderer registry are public, so a custom node renderer
can be plugged in the way goldmark's `renderer.NodeRenderer` is.

## Not ported

The renderer options (`WithUnsafe`, `WithXHTML`, `WithHardWraps`, East Asian line breaks), the
parser options (`WithAttribute`, `WithAutoHeadingID`), the other extensions (footnotes,
definition lists, typographer, CJK) and custom linkify patterns.

## Licence

MIT, the licence of goldmark (`LICENSE-GOLDMARK`).
