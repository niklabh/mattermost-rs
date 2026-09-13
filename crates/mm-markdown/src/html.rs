//! Port of `html.go`: the reference-style HTML renderer. Go's own comment: "it exists primarily
//! as an aid in testing" — and it is exactly that here, the byte-equal oracle for the parity
//! suite. No server route emits it.
//!
//! Rendering recurses once per inline nesting level, as Go's does; Go grows its stack and this
//! does not, so a post nested tens of thousands of images deep can overflow here. `inspect` —
//! the only function the server calls — is iterative and unaffected. Do not point `render_html`
//! at untrusted input.

use std::fmt::Write as _;

use crate::blocks::{Block, Document};
use crate::inlines::Inline;
use crate::markdown::is_hex_byte;
use crate::reference_definition::ReferenceDefinition;

/// Port of `htmlEscaper` (html.go:10): `&`, `<`, `>` and `"` in a single pass.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Port of `markdown.RenderHTML` (html.go:23).
pub fn render_html(markdown: &str) -> String {
    let (document, reference_definitions) = crate::parse(markdown);
    render_block_html(&Block::Document(document), &reference_definitions)
}

/// Port of `markdown.RenderBlockHTML` (html.go:27).
pub fn render_block_html(
    block: &Block<'_>,
    reference_definitions: &[ReferenceDefinition<'_>],
) -> String {
    render_block(block, reference_definitions, false)
}

/// Port of `renderBlockHTML` (html.go:31). `is_tight_list` drops the `<p>` around a paragraph
/// that is the direct content of a tight list item.
fn render_block(
    block: &Block<'_>,
    reference_definitions: &[ReferenceDefinition<'_>],
    is_tight_list: bool,
) -> String {
    let mut out = String::new();
    match block {
        Block::Document(Document { children }) => {
            for child in children {
                out.push_str(&render_block_html(child, reference_definitions));
            }
        }
        Block::Paragraph(p) => {
            if p.text.is_empty() {
                return String::new();
            }
            if !is_tight_list {
                out.push_str("<p>");
            }
            for inline in p.parse_inlines(reference_definitions) {
                out.push_str(&render_inline_html(&inline));
            }
            if !is_tight_list {
                out.push_str("</p>");
            }
        }
        Block::List(list) => {
            if list.is_ordered {
                if list.ordered_start != 1 {
                    let _ = write!(out, "<ol start=\"{}\">", list.ordered_start);
                } else {
                    out.push_str("<ol>");
                }
            } else {
                out.push_str("<ul>");
            }
            for child in &list.children {
                out.push_str(&render_block(child, reference_definitions, !list.is_loose));
            }
            out.push_str(if list.is_ordered { "</ol>" } else { "</ul>" });
        }
        Block::ListItem(item) => {
            out.push_str("<li>");
            for child in &item.children {
                out.push_str(&render_block(child, reference_definitions, is_tight_list));
            }
            out.push_str("</li>");
        }
        Block::BlockQuote(quote) => {
            out.push_str("<blockquote>");
            for child in &quote.children {
                out.push_str(&render_block_html(child, reference_definitions));
            }
            out.push_str("</blockquote>");
        }
        Block::FencedCode(code) => {
            let info = code.info();
            if !info.is_empty() {
                // Go: `strings.Fields(info)[0]`, which panics on an info string that is all
                // Unicode whitespace after unescaping (`&nbsp;`); an empty class is the
                // non-panicking reading of the same input.
                let language = info.split_whitespace().next().unwrap_or("");
                let _ = write!(
                    out,
                    "<pre><code class=\"language-{}\">",
                    html_escape(language)
                );
            } else {
                out.push_str("<pre><code>");
            }
            out.push_str(&html_escape(&code.code()));
            out.push_str("</code></pre>");
        }
        Block::IndentedCode(code) => {
            out.push_str("<pre><code>");
            out.push_str(&html_escape(&code.code()));
            out.push_str("</code></pre>");
        }
    }
    out
}

/// Port of `escapeURL` (html.go:112): percent-encodes every byte outside the reserved and
/// unreserved sets, keeping an existing `%XX`. The escape is `%X` with **no zero padding**
/// (`fmt.Sprintf("%%%0X", b)`), so a byte below 0x10 comes out as `%1`, not `%01`.
fn escape_url(url: &str) -> String {
    let bytes = url.as_bytes();
    let mut result = String::with_capacity(url.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'-' | b'_'
            | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' | b'#' => {
                result.push(b as char);
                i += 1;
            }
            _ => {
                if b == b'%'
                    && i + 2 < bytes.len()
                    && is_hex_byte(bytes[i + 1])
                    && is_hex_byte(bytes[i + 2])
                {
                    result.push_str(&url[i..i + 3]);
                    i += 3;
                } else if b.is_ascii_alphanumeric() {
                    result.push(b as char);
                    i += 1;
                } else {
                    let _ = write!(result, "%{b:X}");
                    i += 1;
                }
            }
        }
    }
    result
}

/// Port of `markdown.RenderInlineHTML` (html.go:134).
pub fn render_inline_html(inline: &Inline<'_>) -> String {
    let mut out = String::new();
    match inline {
        Inline::Text(t) => return html_escape(&t.text),
        Inline::HardLineBreak => return "<br />".to_owned(),
        Inline::SoftLineBreak => return "\n".to_owned(),
        Inline::CodeSpan(c) => return format!("<code>{}</code>", html_escape(&c.code)),
        Inline::InlineImage(image) => {
            let _ = write!(
                out,
                "<img src=\"{}\" alt=\"{}\"",
                html_escape(&escape_url(&image.destination())),
                html_escape(&render_image_alt_text(&image.children))
            );
            let title = image.title();
            if !title.is_empty() {
                let _ = write!(out, " title=\"{}\"", html_escape(&title));
            }
            out.push_str(" />");
        }
        Inline::ReferenceImage(image) => {
            let _ = write!(
                out,
                "<img src=\"{}\" alt=\"{}\"",
                html_escape(&escape_url(&image.destination())),
                html_escape(&render_image_alt_text(&image.children))
            );
            let title = image.title();
            if !title.is_empty() {
                let _ = write!(out, " title=\"{}\"", html_escape(&title));
            }
            out.push_str(" />");
        }
        Inline::InlineLink(link) => {
            let _ = write!(
                out,
                "<a href=\"{}\"",
                html_escape(&escape_url(&link.destination()))
            );
            let title = link.title();
            if !title.is_empty() {
                let _ = write!(out, " title=\"{}\"", html_escape(&title));
            }
            out.push('>');
            for child in &link.children {
                out.push_str(&render_inline_html(child));
            }
            out.push_str("</a>");
        }
        Inline::ReferenceLink(link) => {
            let _ = write!(
                out,
                "<a href=\"{}\"",
                html_escape(&escape_url(&link.destination()))
            );
            let title = link.title();
            if !title.is_empty() {
                let _ = write!(out, " title=\"{}\"", html_escape(&title));
            }
            out.push('>');
            for child in &link.children {
                out.push_str(&render_inline_html(child));
            }
            out.push_str("</a>");
        }
        Inline::Autolink(link) => {
            let _ = write!(
                out,
                "<a href=\"{}\">",
                html_escape(&escape_url(&link.destination()))
            );
            for child in &link.children {
                out.push_str(&render_inline_html(child));
            }
            out.push_str("</a>");
        }
        Inline::Emoji(emoji) => {
            let escaped = html_escape(&emoji.name);
            let _ = write!(
                out,
                "<span data-emoji-name=\"{escaped}\" data-literal=\":{escaped}:\" />"
            );
        }
    }
    out
}

/// Port of `renderImageAltText` (html.go:200).
fn render_image_alt_text(children: &[Inline<'_>]) -> String {
    let mut out = String::new();
    for child in children {
        out.push_str(&render_image_child_alt_text(child));
    }
    out
}

/// Port of `renderImageChildAltText` (html.go:208): text, and the text inside nested inline
/// images and links — reference links and everything else contribute nothing.
fn render_image_child_alt_text(inline: &Inline<'_>) -> String {
    match inline {
        Inline::Text(t) => t.text.to_string(),
        Inline::InlineImage(l) | Inline::InlineLink(l) => render_image_alt_text(&l.children),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_escaping_is_unpadded_uppercase_hex() {
        assert_eq!(escape_url("/a b"), "/a%20b");
        assert_eq!(escape_url("/\x01"), "/%1");
        assert_eq!(escape_url("/ü"), "/%C3%BC");
        assert_eq!(escape_url("/a%20b%2"), "/a%20b%252");
        assert_eq!(
            escape_url(";/?:@&=+$,-_.!~*'()#Az09"),
            ";/?:@&=+$,-_.!~*'()#Az09"
        );
        assert_eq!(escape_url("\""), "%22");
        assert_eq!(escape_url("%zz"), "%25zz");
        assert_eq!(escape_url("%2"), "%252");
    }

    #[test]
    fn html_escaping_is_single_pass() {
        assert_eq!(
            html_escape("&amp; <b> \"q\" 'x'"),
            "&amp;amp; &lt;b&gt; &quot;q&quot; 'x'"
        );
    }

    #[test]
    fn render_shapes() {
        assert_eq!(render_html("a"), "<p>a</p>");
        assert_eq!(render_html("- a\n- b"), "<ul><li>a</li><li>b</li></ul>");
        assert_eq!(
            render_html("- a\n\n- b"),
            "<ul><li><p>a</p></li><li><p>b</p></li></ul>"
        );
        assert_eq!(render_html("2. a"), "<ol start=\"2\"><li>a</li></ol>");
        assert_eq!(render_html("> a"), "<blockquote><p>a</p></blockquote>");
        assert_eq!(
            render_html("```x y\n<\n```"),
            "<pre><code class=\"language-x\">&lt;\n</code></pre>"
        );
        assert_eq!(render_html("    <"), "<pre><code>&lt;</code></pre>");
        assert_eq!(render_html(":a<b:"), "<p>:a&lt;b:</p>");
        assert_eq!(
            render_html(":a_b:"),
            "<p><span data-emoji-name=\"a_b\" data-literal=\":a_b:\" /></p>"
        );
        assert_eq!(
            render_html("![a [b](c) `d`](e \"t\")"),
            "<p><img src=\"e\" alt=\"a b \" title=\"t\" /></p>"
        );
        assert_eq!(render_html(""), "");
    }
}
