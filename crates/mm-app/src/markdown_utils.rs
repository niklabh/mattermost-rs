//! Port of `channels/utils/markdown.go` — Mattermost's wrapper over goldmark (ported in the
//! `gogoldmark` crate).
//!
//! - [`strip_markdown`] / [`strip_markdown_and_decode`] produce the text of a push notification:
//!   goldmark with the Strikethrough extension rendered by `notificationRenderer`, which writes
//!   text and code verbatim and nothing else. What that drops is exactly what the renderer does
//!   not write: link destinations, `<...>` autolinks (their text lives in a private node), raw
//!   HTML, thematic breaks. Entities stay encoded (`&gt;` survives [`strip_markdown`]) until
//!   [`strip_markdown_and_decode`]'s `html.UnescapeString`.
//! - [`markdown_to_html`] produces a notification e-mail's HTML: two regex pre-passes, then
//!   goldmark with GFM and the default renderer (raw HTML omitted).
//!
//! The renderer registration order matters for [`strip_markdown`]: the notification renderer and
//! the Strikethrough extension's HTML renderer are both at priority 500 and both claim
//! `Strikethrough`; goldmark's stable sort lets the one added first — the notification renderer's
//! no-op — win, so `~~x~~` strips to `x` rather than `<del>x</del>`.

use std::rc::Rc;

use gogoldmark::ast::{Ast, NodeId, NodeKind, WalkStatus};
use gogoldmark::{
    Extension, Markdown, NodeRenderer, NodeRendererFunc, NodeRendererFuncRegisterer, RenderError,
    Renderer,
};

/// Errors from the markdown utilities. goldmark's `Convert` only fails when its writer does,
/// which an in-memory buffer never does; the `Result` mirrors the Go signatures.
#[derive(Debug, thiserror::Error)]
pub enum MarkdownError {
    /// goldmark's render failed.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// The large-stack thread could not be started ([`crate::deep_stack`]).
    #[error("could not start a rendering thread: {0}")]
    Thread(std::io::Error),
}

/// Port of `utils.StripMarkdown` (markdown.go:19): markdown to plain text, trimmed.
pub fn strip_markdown(markdown: &str) -> Result<String, MarkdownError> {
    crate::deep_stack::run(|| strip_markdown_here(markdown)).map_err(MarkdownError::Thread)?
}

/// [`strip_markdown`] on the caller's stack; see [`crate::deep_stack`].
fn strip_markdown_here(markdown: &str) -> Result<String, MarkdownError> {
    let renderer = Renderer::new().with_node_renderer(Rc::new(NotificationRenderer), 500);
    let md = Markdown::with_renderer(&[Extension::Strikethrough], renderer);
    let mut buf = Vec::new();
    md.convert(markdown.as_bytes(), &mut buf)?;
    // `strings.TrimSpace` trims Unicode White_Space, which is what `str::trim` trims.
    Ok(String::from_utf8_lossy(&buf).trim().to_owned())
}

/// Port of `utils.StripMarkdownAndDecode` (markdown.go:45): [`strip_markdown`], then
/// `html.UnescapeString`. For plain-text contexts only — the output is not HTML-safe.
pub fn strip_markdown_and_decode(markdown: &str) -> Result<String, MarkdownError> {
    crate::deep_stack::run(|| strip_markdown_and_decode_here(markdown))
        .map_err(MarkdownError::Thread)?
}

/// [`strip_markdown_and_decode`] on the caller's stack; see [`crate::deep_stack`].
fn strip_markdown_and_decode_here(markdown: &str) -> Result<String, MarkdownError> {
    let stripped = strip_markdown_here(markdown)?;
    Ok(mm_model::go_html::unescape_string(&stripped))
}

/// Go's `regexp.Regexp.Expand` over a template, for a pattern with two unnamed groups: `$n`,
/// `${n}` and `$$`; a name is the longest run of `unicode.IsLetter`/`IsDigit`/`_`, a name that
/// is not a group number (or has a leading zero, or names a group that did not match) expands
/// to nothing, and a malformed `$` is kept.
fn go_expand(template: &str, groups: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut t = template;
    while let Some(p) = t.find('$') {
        out.push_str(&t[..p]);
        t = &t[p + 1..];
        if let Some(rest) = t.strip_prefix('$') {
            out.push('$');
            t = rest;
            continue;
        }
        let (brace, body) = match t.strip_prefix('{') {
            Some(b) => (true, b),
            None => (false, t),
        };
        let name_len: usize = body
            .char_indices()
            .find(|&(_, c)| {
                !(mm_model::utils::is_go_letter(c) || mm_model::utils::is_go_digit(c) || c == '_')
            })
            .map_or(body.len(), |(i, _)| i);
        if name_len == 0 || (brace && !body[name_len..].starts_with('}')) {
            out.push('$');
            continue;
        }
        let name = &body[..name_len];
        t = &body[name_len + usize::from(brace)..];
        let mut num: i64 = 0;
        for b in name.bytes() {
            if !b.is_ascii_digit() || num >= 100_000_000 {
                num = -1;
                break;
            }
            num = num * 10 + i64::from(b - b'0');
        }
        if name.len() > 1 && name.starts_with('0') {
            num = -1;
        }
        if num >= 0
            && let Some(g) = groups.get(num as usize)
        {
            out.push_str(g);
        }
    }
    out.push_str(t);
    out
}

/// `relLinkReg` = `\[(.*)]\((/.*)\)`, hand-matched on one line (`.` stops at `\n`) under RE2's
/// leftmost-first preference: both `.*` are greedy, so group 2 ends at the line's **last** `)`
/// and group 1 at the last `](/` that leaves one after it; the match starts at the first `[`
/// before that. The match consumes the line's last `)`, so a line holds at most one match —
/// `[a](/x) [b](/y)` is one match whose group 1 is `a](/x) [b`.
fn rel_link_match(line: &str) -> Option<(usize, usize, usize, usize)> {
    let b = line.as_bytes();
    let last_paren = b.iter().rposition(|&c| c == b')')?;
    let j = (0..b.len())
        .rev()
        .find(|&j| j + 3 <= last_paren && b[j] == b']' && b[j + 1] == b'(' && b[j + 2] == b'/')?;
    let s = b[..j].iter().position(|&c| c == b'[')?;
    // (match start, group-1 end, group-2 start, match end)
    Some((s, j, j + 2, last_paren + 1))
}

/// The first pre-pass of `MarkdownToHTML`: `relLinkReg.ReplaceAllStringFunc` rewriting each
/// relative link through `"[$1](" + siteURL + "$2)"` — a template, so `$` in the site URL is
/// expanded too, exactly as Go does.
fn absolutize_relative_links(markdown: &str, site_url: &str) -> String {
    let template = format!("[$1]({site_url}$2)");
    let mut out = String::with_capacity(markdown.len());
    let mut first = true;
    for line in markdown.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        match rel_link_match(line) {
            Some((s, g1_end, g2_start, e)) => {
                let groups = [&line[s..e], &line[s + 1..g1_end], &line[g2_start..e - 1]];
                out.push_str(&line[..s]);
                out.push_str(&go_expand(&template, &groups));
                out.push_str(&line[e..]);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// The second pre-pass: `blockquoteReg` = `^|\n(&gt;)` through `html.UnescapeString`. The
/// alternation prefers the empty `^` at offset 0, after which Go's `ReplaceAll` steps one
/// character, so only a `\n&gt;` at offset 1 or later becomes `\n>` — a message that *starts*
/// with `&gt;` keeps it.
fn unescape_blockquotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut offset = 0;
    while let Some(p) = rest.find("\n&gt;") {
        if offset + p == 0 {
            out.push_str(&rest[..1]);
            rest = &rest[1..];
            offset += 1;
            continue;
        }
        out.push_str(&rest[..p]);
        out.push_str("\n>");
        rest = &rest[p + 5..];
        offset += p + 5;
    }
    out.push_str(rest);
    out
}

/// Port of `utils.MarkdownToHTML` (markdown.go:57).
pub fn markdown_to_html(markdown: &str, site_url: &str) -> Result<String, MarkdownError> {
    crate::deep_stack::run(|| markdown_to_html_here(markdown, site_url))
        .map_err(MarkdownError::Thread)?
}

/// [`markdown_to_html`] on the caller's stack; see [`crate::deep_stack`].
fn markdown_to_html_here(markdown: &str, site_url: &str) -> Result<String, MarkdownError> {
    let abs = absolutize_relative_links(markdown, site_url);
    let clean = unescape_blockquotes(&abs);
    let md = Markdown::new(&[Extension::Gfm]);
    let mut buf = Vec::new();
    md.convert(clean.as_bytes(), &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

type R = Result<WalkStatus, RenderError>;

fn render_default(_: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, _: bool) -> R {
    Ok(WalkStatus::Continue)
}

fn render_item(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering && ast.next_sibling(n).is_some() {
        w.push(b' ');
    }
    Ok(WalkStatus::Continue)
}

fn write_lines(w: &mut Vec<u8>, source: &[u8], ast: &Ast, n: NodeId) {
    for line in ast.lines(n).as_slice() {
        w.extend_from_slice(&line.value(source));
    }
}

fn render_code_block(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if entering {
        write_lines(w, src, ast, n);
    }
    Ok(WalkStatus::Continue)
}

fn render_text(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering {
        return Ok(WalkStatus::Continue);
    }
    if let Some(segment) = ast.text_segment(n) {
        w.extend_from_slice(&segment.value(src));
    }
    if !ast.is_raw(n) && (ast.hard_line_break(n) || ast.soft_line_break(n)) {
        w.push(b'\n');
    }
    Ok(WalkStatus::Continue)
}

fn render_text_block(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering && ast.next_sibling(n).is_some() && ast.first_child(n).is_some() {
        w.push(b' ');
    }
    Ok(WalkStatus::Continue)
}

fn render_string(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if entering && let Some(v) = ast.string_value(n) {
        w.extend_from_slice(v);
    }
    Ok(WalkStatus::Continue)
}

/// Port of `notificationRenderer` (markdown.go:87).
#[derive(Clone, Copy, Debug, Default)]
pub struct NotificationRenderer;

impl NodeRenderer for NotificationRenderer {
    fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer) {
        let default: NodeRendererFunc = Rc::new(render_default);
        let item: NodeRendererFunc = Rc::new(render_item);
        let code: NodeRendererFunc = Rc::new(render_code_block);
        // block
        reg.register(NodeKind::Document, Rc::clone(&default));
        reg.register(NodeKind::Heading, Rc::clone(&item));
        reg.register(NodeKind::Blockquote, Rc::clone(&default));
        reg.register(NodeKind::CodeBlock, Rc::clone(&code));
        reg.register(NodeKind::FencedCodeBlock, code);
        reg.register(NodeKind::HTMLBlock, Rc::clone(&default));
        reg.register(NodeKind::List, Rc::clone(&default));
        reg.register(NodeKind::ListItem, Rc::clone(&item));
        reg.register(NodeKind::Paragraph, item);
        reg.register(NodeKind::TextBlock, Rc::new(render_text_block));
        reg.register(NodeKind::ThematicBreak, Rc::clone(&default));
        // inlines
        reg.register(NodeKind::AutoLink, Rc::clone(&default));
        reg.register(NodeKind::CodeSpan, Rc::clone(&default));
        reg.register(NodeKind::Emphasis, Rc::clone(&default));
        reg.register(NodeKind::Image, Rc::clone(&default));
        reg.register(NodeKind::Link, Rc::clone(&default));
        reg.register(NodeKind::RawHTML, Rc::clone(&default));
        reg.register(NodeKind::Text, Rc::new(render_text));
        reg.register(NodeKind::String, Rc::new(render_string));
        // strikethrough
        reg.register(NodeKind::Strikethrough, default);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_spaces_between_items_and_paragraphs() {
        assert_eq!(strip_markdown("- one\n- two").unwrap(), "one two");
        assert_eq!(strip_markdown("# h\n\npara").unwrap(), "h para");
        assert_eq!(strip_markdown("a\nb").unwrap(), "a\nb");
        assert_eq!(strip_markdown("a  \nb").unwrap(), "a\nb");
        assert_eq!(strip_markdown("~~x~~ **y**").unwrap(), "x y");
        assert_eq!(strip_markdown("[t](/u) <http://a.b>").unwrap(), "t");
        assert_eq!(strip_markdown("    code\n").unwrap(), "code");
        assert_eq!(strip_markdown("&gt; x").unwrap(), "&gt; x");
        assert_eq!(strip_markdown_and_decode("&gt; x &#60;").unwrap(), "> x <");
    }

    #[test]
    fn rel_link_is_one_greedy_match_per_line() {
        assert_eq!(
            absolutize_relative_links("[a](/x) [b](/y)", "https://s"),
            "[a](/x) [b](https://s/y)"
        );
        assert_eq!(
            absolutize_relative_links("[a](/x)\n[b](/y)", "https://s"),
            "[a](https://s/x)\n[b](https://s/y)"
        );
        assert_eq!(absolutize_relative_links("[a](/x", "s"), "[a](/x");
        assert_eq!(absolutize_relative_links("[a](/)", "s"), "[a](s/)");
        assert_eq!(absolutize_relative_links("x](/y) [", "s"), "x](/y) [");
        assert_eq!(
            absolutize_relative_links("[x](http://a/b)", "s"),
            "[x](http://a/b)"
        );
    }

    #[test]
    fn expand_follows_go() {
        let g = ["whole", "one", "two"];
        assert_eq!(go_expand("$1-$2", &g), "one-two");
        assert_eq!(go_expand("$$1", &g), "$1");
        assert_eq!(go_expand("${1}x", &g), "onex");
        assert_eq!(go_expand("${1x", &g), "${1x");
        assert_eq!(go_expand("$01 $3 $name $", &g), "   $");
        assert_eq!(go_expand("$1a", &g), "");
        assert_eq!(go_expand("$0", &g), "whole");
    }

    #[test]
    fn blockquote_pass_skips_offset_zero() {
        assert_eq!(unescape_blockquotes("\n&gt; a\n&gt; b"), "\n&gt; a\n> b");
        assert_eq!(unescape_blockquotes("x\n&gt;&gt;"), "x\n>&gt;");
        assert_eq!(unescape_blockquotes("&gt; x"), "&gt; x");
        assert_eq!(unescape_blockquotes(""), "");
    }

    mod go_parity {
        use super::super::*;
        use serde_json::Value;

        fn fixture() -> Value {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/behaviour_goldmark.json"
            );
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        }

        #[test]
        fn markdown_utils_match_mattermost() {
            let f = fixture();
            let site = f["site_url"].as_str().unwrap();
            let mut failures = Vec::new();
            let cases = f["cases"].as_array().unwrap();
            for c in cases {
                let input = c["input"].as_str().unwrap();
                // The fixture omits a value that repeats another (behaviour_goldmark.go).
                let or = |key: &str, base: &str| {
                    c.get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| c[base].as_str().unwrap())
                        .to_owned()
                };
                let checks = [
                    (
                        "strip",
                        strip_markdown(input).unwrap(),
                        or("strip", "strip"),
                    ),
                    (
                        "strip_decode",
                        strip_markdown_and_decode(input).unwrap(),
                        or("strip_decode", "strip"),
                    ),
                    (
                        "prepass",
                        unescape_blockquotes(&absolutize_relative_links(input, site)),
                        or("prepass", "input"),
                    ),
                    (
                        "md_to_html",
                        markdown_to_html(input, site).unwrap(),
                        or("md_to_html", "gfm"),
                    ),
                ];
                for (key, got, want) in checks {
                    if got != want {
                        failures.push(format!(
                            "{} [{key}]\ninput: {input:?}\nwant:  {want:?}\ngot:   {got:?}",
                            c["id"]
                        ));
                    }
                }
                for key in ["strip_err", "strip_decode_err", "md_to_html_err"] {
                    assert!(
                        c.get(key).is_none(),
                        "{} {key}: Go never errors here",
                        c["id"]
                    );
                }
            }
            for c in f["site_cases"].as_array().unwrap() {
                let (input, su) = (
                    c["input"].as_str().unwrap(),
                    c["site_url"].as_str().unwrap(),
                );
                let pre = unescape_blockquotes(&absolutize_relative_links(input, su));
                if pre != c["prepass"].as_str().unwrap() {
                    failures.push(format!(
                        "site {input:?} {su:?}: prepass {pre:?} != {}",
                        c["prepass"]
                    ));
                }
                let got = markdown_to_html(input, su).unwrap();
                if got != c["html"].as_str().unwrap() {
                    failures.push(format!(
                        "site {input:?} {su:?}: html {got:?} != {}",
                        c["html"]
                    ));
                }
            }
            assert!(
                failures.is_empty(),
                "{} divergences; first:\n{}",
                failures.len(),
                failures
                    .iter()
                    .take(20)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n---\n")
            );
            assert!(cases.len() > 3800);
        }
    }
}
