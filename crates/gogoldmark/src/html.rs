//! Port of `renderer/html/html.go` with its default configuration (no `WithUnsafe`, no
//! `WithXHTML`, no `WithHardWraps`, no East Asian line breaks — the options Mattermost never
//! sets), and of the GFM extensions' HTML renderers (`extension/strikethrough.go`,
//! `table.go`, `tasklist.go`).
//!
//! Unsafe mode is not ported: raw HTML always renders as `<!-- raw HTML omitted -->` and
//! dangerous URLs (`javascript:`, `vbscript:`, `file:`, `data:` other than four image types)
//! render as an empty `href`/`src`.

use std::rc::Rc;

use crate::ast::{Alignment, Ast, AttributeValue, AutoLinkType, NodeData, NodeId, WalkStatus};
use crate::renderer::{NodeRenderer, NodeRendererFuncRegisterer, RenderError};
use crate::util;

type R = Result<WalkStatus, RenderError>;

const CONTINUE: R = Ok(WalkStatus::Continue);

// ---- defaultWriter ----

/// Port of `defaultWriter.RawWrite`: HTML-escape `"&<>` and turn NUL into U+FFFD.
pub fn raw_write(w: &mut Vec<u8>, source: &[u8]) {
    for &c in source {
        match util::escape_html_byte(c) {
            Some(e) => w.extend_from_slice(e),
            None => w.push(c),
        }
    }
}

/// Port of `defaultWriter.SecureWrite`: NUL becomes U+FFFD, nothing else changes.
pub fn secure_write(w: &mut Vec<u8>, source: &[u8]) {
    for &c in source {
        if c == 0 {
            w.extend_from_slice("\u{FFFD}".as_bytes());
        } else {
            w.push(c);
        }
    }
}

fn escape_rune(w: &mut Vec<u8>, v: u64) {
    if v < 256
        && let Some(e) = util::escape_html_byte(v as u8)
    {
        w.extend_from_slice(e);
        return;
    }
    let mut buf = [0u8; 4];
    w.extend_from_slice(util::to_valid_rune(v).encode_utf8(&mut buf).as_bytes());
}

/// Port of `defaultWriter.Write`: backslash escapes, numeric and named character references
/// resolved, then [`raw_write`].
pub fn write(w: &mut Vec<u8>, source: &[u8]) {
    let mut escaped = false;
    let limit = source.len();
    let mut n = 0;
    let mut i = 0;
    while i < limit {
        let c = source[i];
        if escaped && util::is_punct(c) {
            raw_write(w, &source[n..i - 1]);
            n = i;
            escaped = false;
            i += 1;
            continue;
        }
        if c == 0 {
            raw_write(w, &source[n..i]);
            raw_write(w, "\u{FFFD}".as_bytes());
            n = i + 1;
            escaped = false;
            i += 1;
            continue;
        }
        if c == b'&' {
            let pos = i;
            let next = i + 1;
            if next < limit && source[next] == b'#' {
                let nnext = next + 1;
                if nnext < limit {
                    let nc = source[nnext];
                    if nc == b'x' || nc == b'X' {
                        let start = nnext + 1;
                        let (j, ok) = util::read_while(source, start, limit, util::is_hex_decimal);
                        if ok && j < limit && source[j] == b';' && j - start < 7 {
                            let v = util::go_parse_uint32(&source[start..j], 16);
                            raw_write(w, &source[n..pos]);
                            n = j + 1;
                            escape_rune(w, v);
                            i = j + 1;
                            continue;
                        }
                    } else if nc.is_ascii_digit() {
                        let start = nnext;
                        let (j, ok) = util::read_while(source, start, limit, util::is_numeric);
                        if ok && j < limit && j - start < 8 && source[j] == b';' {
                            let v = util::go_parse_uint32(&source[start..j], 10);
                            raw_write(w, &source[n..pos]);
                            n = j + 1;
                            escape_rune(w, v);
                            i = j + 1;
                            continue;
                        }
                    }
                }
            } else {
                let start = next;
                let (j, ok) = util::read_while(source, start, limit, util::is_alpha_numeric);
                if ok
                    && j < limit
                    && source[j] == b';'
                    && let Some(chars) = util::lookup_html5_entity(&source[start..j])
                {
                    raw_write(w, &source[n..pos]);
                    n = j + 1;
                    raw_write(w, chars);
                    i = j + 1;
                    continue;
                }
            }
            // `i = next - 1`, then the loop's `i++`; `c` is still `&`.
            escaped = false;
            i = next;
            continue;
        }
        escaped = c == b'\\';
        i += 1;
    }
    raw_write(w, &source[n..]);
}

fn has_prefix_fold(s: &[u8], prefix: &[u8]) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Port of `html.IsDangerousURL`.
pub fn is_dangerous_url(url: &[u8]) -> bool {
    if has_prefix_fold(url, b"data:image/") && url.len() >= 11 {
        let v = &url[11..];
        if has_prefix_fold(v, b"png;")
            || has_prefix_fold(v, b"gif;")
            || has_prefix_fold(v, b"jpeg;")
            || has_prefix_fold(v, b"webp;")
            || has_prefix_fold(v, b"svg+xml;")
        {
            return false;
        }
        return true;
    }
    has_prefix_fold(url, b"javascript:")
        || has_prefix_fold(url, b"vbscript:")
        || has_prefix_fold(url, b"file:")
        || has_prefix_fold(url, b"data:")
}

const GLOBAL_ATTRIBUTES: &[&[u8]] = &[
    b"accesskey",
    b"autocapitalize",
    b"autofocus",
    b"class",
    b"contenteditable",
    b"dir",
    b"draggable",
    b"enterkeyhint",
    b"hidden",
    b"id",
    b"inert",
    b"inputmode",
    b"is",
    b"itemid",
    b"itemprop",
    b"itemref",
    b"itemscope",
    b"itemtype",
    b"lang",
    b"part",
    b"role",
    b"slot",
    b"spellcheck",
    b"style",
    b"tabindex",
    b"title",
    b"translate",
];

const TABLE_CELL_ATTRIBUTES: &[&[u8]] = &[
    b"abbr", b"align", b"axis", b"bgcolor", b"char", b"charoff", b"colspan", b"headers", b"height",
    b"rowspan", b"scope", b"valign", b"width",
];

/// Port of `html.RenderAttributes` with a filter made of the global attributes plus `extra`.
fn render_attributes(w: &mut Vec<u8>, ast: &Ast, node: NodeId, extra: &[&[u8]]) {
    let Some(attrs) = ast.attributes(node) else {
        return;
    };
    for attr in attrs {
        let name: &[u8] = &attr.name;
        if !GLOBAL_ATTRIBUTES.contains(&name)
            && !extra.contains(&name)
            && !name.starts_with(b"data-")
        {
            continue;
        }
        w.push(b' ');
        w.extend_from_slice(name);
        w.extend_from_slice(b"=\"");
        let AttributeValue::Bytes(v) = &attr.value;
        w.extend_from_slice(&util::escape_html(v));
        w.push(b'"');
    }
}

fn write_lines(w: &mut Vec<u8>, source: &[u8], ast: &Ast, n: NodeId) {
    for line in ast.lines(n).as_slice() {
        raw_write(w, &line.value(source));
    }
}

fn render_heading(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    let level = match ast.data(n) {
        NodeData::Heading { level } => *level,
        _ => 0,
    };
    let digit = b"0123456".get(level as usize).copied().unwrap_or(b'0');
    if entering {
        w.extend_from_slice(b"<h");
        w.push(digit);
        w.push(b'>');
    } else {
        w.extend_from_slice(b"</h");
        w.push(digit);
        w.extend_from_slice(b">\n");
    }
    CONTINUE
}

fn render_blockquote(w: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, entering: bool) -> R {
    w.extend_from_slice(if entering {
        b"<blockquote>\n"
    } else {
        b"</blockquote>\n"
    });
    CONTINUE
}

fn render_code_block(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if entering {
        w.extend_from_slice(b"<pre><code>");
        write_lines(w, src, ast, n);
    } else {
        w.extend_from_slice(b"</code></pre>\n");
    }
    CONTINUE
}

/// Port of `FencedCodeBlock.Language`: the info string up to its first space.
pub fn fenced_code_block_language(ast: &Ast, n: NodeId, source: &[u8]) -> Option<Vec<u8>> {
    let NodeData::FencedCodeBlock { info: Some(info) } = ast.data(n) else {
        return None;
    };
    let v = info.value(source);
    let end = v.iter().position(|&c| c == b' ').unwrap_or(v.len());
    Some(v[..end].to_vec())
}

fn render_fenced_code_block(
    w: &mut Vec<u8>,
    src: &[u8],
    ast: &mut Ast,
    n: NodeId,
    entering: bool,
) -> R {
    if entering {
        w.extend_from_slice(b"<pre><code");
        if let Some(language) = fenced_code_block_language(ast, n, src) {
            w.extend_from_slice(b" class=\"language-");
            write(w, &language);
            w.push(b'"');
        }
        w.push(b'>');
        write_lines(w, src, ast, n);
    } else {
        w.extend_from_slice(b"</code></pre>\n");
    }
    CONTINUE
}

fn render_html_block(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    let has_closure =
        matches!(ast.data(n), NodeData::HTMLBlock { closure_line, .. } if closure_line.start >= 0);
    if entering || has_closure {
        w.extend_from_slice(b"<!-- raw HTML omitted -->\n");
    }
    CONTINUE
}

fn render_list(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    let (ordered, start) = match ast.data(n) {
        NodeData::List { marker, start, .. } => (*marker == b'.' || *marker == b')', *start),
        _ => (false, 0),
    };
    let tag: &[u8] = if ordered { b"ol" } else { b"ul" };
    if entering {
        w.push(b'<');
        w.extend_from_slice(tag);
        if ordered && start != 1 {
            w.extend_from_slice(format!(" start=\"{start}\"").as_bytes());
        }
        w.extend_from_slice(b">\n");
    } else {
        w.extend_from_slice(b"</");
        w.extend_from_slice(tag);
        w.extend_from_slice(b">\n");
    }
    CONTINUE
}

fn render_list_item(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if entering {
        w.extend_from_slice(b"<li>");
        if let Some(fc) = ast.first_child(n)
            && !matches!(ast.data(fc), NodeData::TextBlock)
        {
            w.push(b'\n');
        }
    } else {
        w.extend_from_slice(b"</li>\n");
    }
    CONTINUE
}

fn render_paragraph(w: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, entering: bool) -> R {
    w.extend_from_slice(if entering { b"<p>" } else { b"</p>\n" });
    CONTINUE
}

fn render_text_block(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering && ast.next_sibling(n).is_some() && ast.first_child(n).is_some() {
        w.push(b'\n');
    }
    CONTINUE
}

fn render_thematic_break(w: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, entering: bool) -> R {
    if entering {
        w.extend_from_slice(b"<hr>\n");
    }
    CONTINUE
}

/// Port of `AutoLink.URL`: `protocol://value`, or the value.
pub fn auto_link_url(ast: &Ast, n: NodeId, source: &[u8]) -> Vec<u8> {
    match ast.data(n) {
        NodeData::AutoLink {
            protocol, value, ..
        } => {
            let v = value.value(source);
            match protocol {
                Some(p) => {
                    let mut out = p.clone();
                    out.extend_from_slice(b"://");
                    out.extend_from_slice(&v);
                    out
                }
                None => v.into_owned(),
            }
        }
        _ => Vec::new(),
    }
}

fn render_auto_link(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering {
        return CONTINUE;
    }
    let NodeData::AutoLink { typ, value, .. } = ast.data(n) else {
        return CONTINUE;
    };
    let (typ, label) = (*typ, value.value(src).into_owned());
    w.extend_from_slice(b"<a href=\"");
    let url = util::url_escape(&auto_link_url(ast, n, src), false);
    if typ == AutoLinkType::Email && !has_prefix_fold(&url, b"mailto:") {
        w.extend_from_slice(b"mailto:");
    }
    if !is_dangerous_url(&url) {
        w.extend_from_slice(&util::escape_html(&url));
    }
    w.extend_from_slice(b"\">");
    w.extend_from_slice(&util::escape_html(&label));
    w.extend_from_slice(b"</a>");
    CONTINUE
}

fn render_code_span(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if entering {
        w.extend_from_slice(b"<code>");
        for c in ast.children(n) {
            let Some(segment) = ast.text_segment(c) else {
                continue;
            };
            let value = segment.value(src);
            if let Some(v) = value.strip_suffix(b"\n") {
                raw_write(w, v);
                raw_write(w, b" ");
            } else {
                raw_write(w, &value);
            }
        }
        return Ok(WalkStatus::SkipChildren);
    }
    w.extend_from_slice(b"</code>");
    CONTINUE
}

fn render_emphasis(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    let tag: &[u8] = match ast.data(n) {
        NodeData::Emphasis { level: 2 } => b"strong",
        _ => b"em",
    };
    if entering {
        w.push(b'<');
        w.extend_from_slice(tag);
        w.push(b'>');
    } else {
        w.extend_from_slice(b"</");
        w.extend_from_slice(tag);
        w.push(b'>');
    }
    CONTINUE
}

fn render_link(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering {
        w.extend_from_slice(b"</a>");
        return CONTINUE;
    }
    let NodeData::Link {
        destination, title, ..
    } = ast.data(n)
    else {
        return CONTINUE;
    };
    w.extend_from_slice(b"<a href=\"");
    let dest = util::url_escape(destination, true);
    if !is_dangerous_url(&dest) {
        w.extend_from_slice(&util::escape_html(&dest));
    }
    w.push(b'"');
    if let Some(t) = title {
        w.extend_from_slice(b" title=\"");
        write(w, t);
        w.push(b'"');
    }
    w.push(b'>');
    CONTINUE
}

/// Port of `html.Renderer.renderTexts` (the image `alt`), iteratively.
fn render_texts(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId) {
    let mut stack = vec![ast.first_child(n)];
    while let Some(top) = stack.last_mut() {
        let Some(c) = *top else {
            stack.pop();
            continue;
        };
        *top = ast.next_sibling(c);
        match ast.data(c) {
            NodeData::String { .. } => {
                let _ = render_string(w, src, ast, c, true);
            }
            NodeData::Text { .. } => {
                let _ = render_text(w, src, ast, c, true);
            }
            _ => stack.push(ast.first_child(c)),
        }
    }
}

fn render_image(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering {
        return CONTINUE;
    }
    let NodeData::Image {
        destination, title, ..
    } = ast.data(n)
    else {
        return CONTINUE;
    };
    let (destination, title) = (destination.clone(), title.clone());
    w.extend_from_slice(b"<img src=\"");
    let dest = util::url_escape(&destination, true);
    if !is_dangerous_url(&dest) {
        w.extend_from_slice(&util::escape_html(&dest));
    }
    w.extend_from_slice(b"\" alt=\"");
    render_texts(w, src, ast, n);
    w.push(b'"');
    if let Some(t) = title {
        w.extend_from_slice(b" title=\"");
        write(w, &t);
        w.push(b'"');
    }
    w.push(b'>');
    Ok(WalkStatus::SkipChildren)
}

fn render_raw_html(w: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, entering: bool) -> R {
    if entering {
        w.extend_from_slice(b"<!-- raw HTML omitted -->");
    }
    Ok(WalkStatus::SkipChildren)
}

fn render_text(w: &mut Vec<u8>, src: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering {
        return CONTINUE;
    }
    let Some(segment) = ast.text_segment(n) else {
        return CONTINUE;
    };
    let value = segment.value(src);
    if ast.is_raw(n) {
        raw_write(w, &value);
    } else {
        write(w, &value);
        if ast.hard_line_break(n) {
            w.extend_from_slice(b"<br>\n");
        } else if ast.soft_line_break(n) {
            w.push(b'\n');
        }
    }
    CONTINUE
}

fn render_string(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering {
        return CONTINUE;
    }
    let Some(v) = ast.string_value(n) else {
        return CONTINUE;
    };
    if ast.is_code(n) {
        w.extend_from_slice(v);
    } else if ast.is_raw(n) {
        raw_write(w, v);
    } else {
        write(w, v);
    }
    CONTINUE
}

fn noop(_: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, _: bool) -> R {
    CONTINUE
}

fn skip(_: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, _: bool) -> R {
    Ok(WalkStatus::SkipChildren)
}

/// Port of `html.Renderer` (html.go:250) with the default `Config`.
#[derive(Clone, Copy, Debug, Default)]
pub struct HtmlRenderer;

impl NodeRenderer for HtmlRenderer {
    fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer) {
        use crate::ast::NodeKind as K;
        reg.register(K::Document, Rc::new(noop));
        reg.register(K::Heading, Rc::new(render_heading));
        reg.register(K::Blockquote, Rc::new(render_blockquote));
        reg.register(K::CodeBlock, Rc::new(render_code_block));
        reg.register(K::FencedCodeBlock, Rc::new(render_fenced_code_block));
        reg.register(K::HTMLBlock, Rc::new(render_html_block));
        reg.register(K::List, Rc::new(render_list));
        reg.register(K::ListItem, Rc::new(render_list_item));
        reg.register(K::Paragraph, Rc::new(render_paragraph));
        reg.register(K::TextBlock, Rc::new(render_text_block));
        reg.register(K::ThematicBreak, Rc::new(render_thematic_break));
        reg.register(K::LinkReferenceDefinition, Rc::new(skip));
        reg.register(K::AutoLink, Rc::new(render_auto_link));
        reg.register(K::CodeSpan, Rc::new(render_code_span));
        reg.register(K::Emphasis, Rc::new(render_emphasis));
        reg.register(K::Image, Rc::new(render_image));
        reg.register(K::Link, Rc::new(render_link));
        reg.register(K::RawHTML, Rc::new(render_raw_html));
        reg.register(K::Text, Rc::new(render_text));
        reg.register(K::String, Rc::new(render_string));
    }
}

// ---- extension/strikethrough.go ----

fn render_strikethrough(w: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, entering: bool) -> R {
    w.extend_from_slice(if entering { b"<del>" } else { b"</del>" });
    CONTINUE
}

/// Port of `extension.StrikethroughHTMLRenderer`.
#[derive(Clone, Copy, Debug, Default)]
pub struct StrikethroughHtmlRenderer;

impl NodeRenderer for StrikethroughHtmlRenderer {
    fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer) {
        reg.register(
            crate::ast::NodeKind::Strikethrough,
            Rc::new(render_strikethrough),
        );
    }
}

// ---- extension/tasklist.go ----

fn render_task_check_box(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if !entering {
        return CONTINUE;
    }
    if matches!(ast.data(n), NodeData::TaskCheckBox { checked: true }) {
        w.extend_from_slice(b"<input checked=\"\" disabled=\"\" type=\"checkbox\"");
    } else {
        w.extend_from_slice(b"<input disabled=\"\" type=\"checkbox\"");
    }
    w.extend_from_slice(b"> ");
    CONTINUE
}

/// Port of `extension.TaskCheckBoxHTMLRenderer`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskCheckBoxHtmlRenderer;

impl NodeRenderer for TaskCheckBoxHtmlRenderer {
    fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer) {
        reg.register(
            crate::ast::NodeKind::TaskCheckBox,
            Rc::new(render_task_check_box),
        );
    }
}

// ---- extension/table.go ----

fn render_table(w: &mut Vec<u8>, _: &[u8], _: &mut Ast, _: NodeId, entering: bool) -> R {
    w.extend_from_slice(if entering {
        b"<table>\n"
    } else {
        b"</table>\n"
    });
    CONTINUE
}

fn render_table_header(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if entering {
        w.extend_from_slice(b"<thead>\n<tr>\n");
    } else {
        w.extend_from_slice(b"</tr>\n</thead>\n");
        if ast.next_sibling(n).is_some() {
            w.extend_from_slice(b"<tbody>\n");
        }
    }
    CONTINUE
}

fn render_table_row(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    if entering {
        w.extend_from_slice(b"<tr>\n");
    } else {
        w.extend_from_slice(b"</tr>\n");
        if ast.parent(n).and_then(|p| ast.last_child(p)) == Some(n) {
            w.extend_from_slice(b"</tbody>\n");
        }
    }
    CONTINUE
}

/// `TableCellAlignDefault` without XHTML is `TableCellAlignStyle`: the alignment is appended
/// to the node's `style` attribute (mutating the node, as Go does) and rendered with it.
fn render_table_cell(w: &mut Vec<u8>, _: &[u8], ast: &mut Ast, n: NodeId, entering: bool) -> R {
    let alignment = match ast.data(n) {
        NodeData::TableCell { alignment } => *alignment,
        _ => Alignment::None,
    };
    let header = ast
        .parent(n)
        .is_some_and(|p| matches!(ast.data(p), NodeData::TableHeader { .. }));
    let tag: &[u8] = if header { b"th" } else { b"td" };
    if entering {
        w.push(b'<');
        w.extend_from_slice(tag);
        if alignment != Alignment::None {
            let mut style = match ast.attribute(n, b"style") {
                Some(AttributeValue::Bytes(v)) => {
                    let mut v = v.clone();
                    v.push(b';');
                    v
                }
                None => Vec::new(),
            };
            style.extend_from_slice(b"text-align:");
            style.extend_from_slice(alignment.as_str().as_bytes());
            ast.set_attribute(n, b"style", AttributeValue::Bytes(style));
        }
        render_attributes(w, ast, n, TABLE_CELL_ATTRIBUTES);
        w.push(b'>');
    } else {
        w.extend_from_slice(b"</");
        w.extend_from_slice(tag);
        w.extend_from_slice(b">\n");
    }
    CONTINUE
}

/// Port of `extension.TableHTMLRenderer` with the default `TableConfig`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TableHtmlRenderer;

impl NodeRenderer for TableHtmlRenderer {
    fn register_funcs(self: Rc<Self>, reg: &mut dyn NodeRendererFuncRegisterer) {
        use crate::ast::NodeKind as K;
        reg.register(K::Table, Rc::new(render_table));
        reg.register(K::TableHeader, Rc::new(render_table_header));
        reg.register(K::TableRow, Rc::new(render_table_row));
        reg.register(K::TableCell, Rc::new(render_table_cell));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(f: fn(&mut Vec<u8>, &[u8]), s: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        f(&mut out, s);
        out
    }

    #[test]
    fn writer_escapes_and_references() {
        assert_eq!(w(write, b"a\\*b"), b"a*b");
        assert_eq!(w(write, b"a\\b"), b"a\\b");
        assert_eq!(
            w(write, b"&amp; &#35; &#x41; &copy;"),
            "&amp; # A \u{a9}".as_bytes()
        );
        assert_eq!(w(write, b"&#60;&#x3c;"), b"&lt;&lt;");
        assert_eq!(
            w(write, b"&#0; &#x110000; &#xD800;"),
            "\u{fffd} \u{fffd} \u{fffd}".as_bytes()
        );
        assert_eq!(
            w(write, b"&#12345678; &#x1234567;"),
            b"&amp;#12345678; &amp;#x1234567;"
        );
        assert_eq!(
            w(write, b"&bogus; &\x00"),
            "&amp;bogus; &amp;\u{fffd}".as_bytes()
        );
        assert_eq!(w(write, b"\\&amp;"), b"&amp;amp;");
        assert_eq!(w(raw_write, b"<\"'>"), b"&lt;&quot;'&gt;");
        assert_eq!(w(secure_write, b"<\x00>"), "<\u{fffd}>".as_bytes());
    }

    #[test]
    fn dangerous_urls() {
        assert!(is_dangerous_url(b"JavaScript:alert(1)"));
        assert!(is_dangerous_url(b"vbscript:x"));
        assert!(is_dangerous_url(b"file:///etc"));
        assert!(is_dangerous_url(b"data:text/html,x"));
        assert!(is_dangerous_url(b"data:image/bmp;x"));
        assert!(!is_dangerous_url(b"DATA:image/PNG;base64,x"));
        assert!(!is_dangerous_url(b"data:image/svg+xml;x"));
        assert!(!is_dangerous_url(b"https://x"));
    }
}
