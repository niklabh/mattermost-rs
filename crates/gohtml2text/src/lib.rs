//! Port of `github.com/jaytaylor/html2text@v0.0.0-20260303211410-1a4bdc82ecec` (html2text.go) and
//! of the `CleanBom` it calls from `github.com/ssor/bom` — the conversion Mattermost's mail sender
//! runs over every HTML body to build its text/plain alternative
//! (`platform/shared/mail/mail.go:312`, `html2text.FromString(htmlMessage)`, default options). On
//! an error Mattermost logs a warning and sends an empty text part.
//!
//! The document is parsed by `gohtml`, the port of `golang.org/x/net/html` this library runs on.
//! Not Mattermost source: MIT (LICENSE-HTML2TEXT, LICENSE-BOM). This crate must never depend on an
//! `mm-*` crate.
//!
//! # Byte-exactness notes
//!
//! * Lengths are Go's: `lineLength` and the heading divider count **runes**, so a CJK or accented
//!   line is measured in code points, not bytes or display columns.
//! * "Space" is Go's `unicode.IsSpace` ([`is_space`]), which counts U+00A0 and U+0085; the
//!   collapsing regexp `[ \r\n\t]+` does not, so a no-break space survives collapsing but can
//!   start a line or end a quoted line break.
//! * `CleanBom` runs twice (`FromString`, then `NewReaderWithoutBom`), so up to two leading
//!   byte-order marks are dropped and a third is text.
//! * The heading sub-traversal starts from a zero context whose `endsWithSpace` is false, so the
//!   heading text begins with a space that counts towards the divider (`<h1>Title</h1>` gets
//!   five `*`) and is only removed at the end with every other `"\n "`.
//! * The heading and bold sub-traversals use a **zero** context, so a link inside `<h1>` or `<b>`
//!   prints its URL even under [`Options::omit_links`] or [`Options::text_only`]; under
//!   `text_only` a nested blockquote's closing still appends the `" "` Go appends, so the prefix
//!   grows by one space per closed level.
//!
//! # Not ported
//!
//! `PrettyTables` / `PrettyTablesOptions` (prettytables.go), which render tables through
//! `olekukonko/tablewriter`. Mattermost never enables them; [`Options`] has no such field, so
//! tables always take the plain paragraph path. `FromReader` is subsumed by [`from_string`].

/// The README's examples, compiled and run as doctests so the crates.io page cannot drift.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

use gohtml::{Atom, Document, NodeId, NodeType, ParseError};

/// The error `FromString` returns: only ever the parser's (nesting past 512 elements).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Parse(#[from] ParseError),
}

/// Port of `html2text.Options` (html2text.go:17) without the table fields (see the crate docs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// `OmitLinks`: never print a link's `( href )`.
    pub omit_links: bool,
    /// `TextOnly`: no link targets, no `*` around bold or `* ` before list items, no heading
    /// dividers (a heading ends with `.` instead), no `>` quote prefixes.
    pub text_only: bool,
}

/// Port of `html2text.FromString` (html2text.go:66) with default `Options` — the call Mattermost
/// makes.
pub fn from_string(input: &str) -> Result<String, Error> {
    from_string_with_options(input, Options::default())
}

/// Port of `html2text.FromString(input, options)`.
pub fn from_string_with_options(input: &str, options: Options) -> Result<String, Error> {
    // `bom.CleanBom` in FromString, then again in `bom.NewReaderWithoutBom` (FromReader).
    let input = clean_bom(clean_bom(input));
    let doc = gohtml::parse(input)?;
    Ok(from_html_node(&doc, doc.root(), options))
}

/// Port of `bom.CleanBom` (bom.go:15): drop one leading UTF-8 byte-order mark.
pub fn clean_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Port of `html2text.FromHTMLNode` (html2text.go:25). Go's version returns an error only when a
/// `bytes.Buffer` write fails, which it never does.
pub fn from_html_node(doc: &Document, node: NodeId, options: Options) -> String {
    let buf = Traversal::new(doc, options).run(node);
    // `strings.Replace(buf, "\n ", "\n", -1)`, then `newlineRe` (`\n\n+` → `\n\n`), then
    // `strings.TrimSpace`.
    let text = collapse_newlines(&buf.replace("\n ", "\n"));
    trim_space(&text).to_owned()
}

/// `maxLineLen` (html2text.go:417): quoted lines break before this many runes.
const MAX_LINE_LEN: usize = 74;

/// Go's `unicode.IsSpace`: the Latin-1 spaces (including U+0085 and U+00A0) and, above that,
/// Unicode's `White_Space` property.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0b}'
            | '\u{0c}'
            | '\r'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `strings.TrimSpace`.
fn trim_space(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `spacingRe.ReplaceAllString(s, " ")`: every run of `[ \r\n\t]` becomes one space.
fn collapse_spacing(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if matches!(c, ' ' | '\r' | '\n' | '\t') {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// `newlineRe.ReplaceAllString(s, "\n\n")`: every run of two or more `\n` becomes two.
fn collapse_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for c in s.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push('\n');
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

/// Port of `getAttrVal` (html2text.go:490): the first attribute with this key, whatever its
/// namespace — so an SVG `xlink:href` answers for `href`.
fn attr_val<'a>(doc: &'a Document, node: NodeId, name: &str) -> &'a str {
    doc[node]
        .attr
        .iter()
        .find(|a| a.key == name)
        .map_or("", |a| a.val.as_str())
}

/// Port of `textifyTraverseContext` (html2text.go:79) minus the table context.
#[derive(Debug, Default)]
struct Ctx {
    buf: String,
    prefix: String,
    options: Options,
    ends_with_space: bool,
    just_closed_div: bool,
    blockquote_level: usize,
    line_length: usize,
    is_pre: bool,
}

impl Ctx {
    /// Port of `emit` (html2text.go:382).
    fn emit(&mut self, data: &str) {
        if data.is_empty() {
            return;
        }
        let starts_with_dot = data.starts_with('.');
        for line in self.break_long_lines(data) {
            // `breakLongLines` never yields an empty line, so Go's `runes[0]` cannot panic.
            let (Some(first), Some(last)) = (line.chars().next(), line.chars().next_back()) else {
                continue;
            };
            if !is_space(first) && !self.ends_with_space && !starts_with_dot {
                self.buf.push(' ');
                self.line_length += 1;
            }
            self.ends_with_space = is_space(last);
            for c in line.chars() {
                self.buf.push(c);
                self.line_length += 1;
                if c == '\n' {
                    self.line_length = 0;
                    if !self.prefix.is_empty() {
                        self.buf.push_str(&self.prefix);
                    }
                }
            }
        }
    }

    /// Port of `breakLongLines` (html2text.go:419): inside a blockquote, cut `data` into lines of
    /// at most [`MAX_LINE_LEN`] runes counting what the current line already holds — at the last
    /// space at or before the limit, or failing that at the first space after it.
    fn break_long_lines(&self, data: &str) -> Vec<String> {
        if self.blockquote_level == 0 {
            return vec![data.to_owned()];
        }
        let mut ret = Vec::new();
        let mut runes: &[char] = &data.chars().collect::<Vec<_>>();
        let mut existing = self.line_length;
        if existing >= MAX_LINE_LEN {
            ret.push("\n".to_owned());
            existing = 0;
        }
        while runes.len() + existing > MAX_LINE_LEN {
            // `existing < MAX_LINE_LEN` here, and `runes.len() > MAX_LINE_LEN - existing`, so
            // `limit` indexes `runes`.
            let limit = MAX_LINE_LEN - existing;
            let mut i = match runes[..=limit].iter().rposition(|&c| is_space(c)) {
                Some(i) => i,
                // "No spaces, so go the other way."
                None => runes[limit..]
                    .iter()
                    .position(|&c| is_space(c))
                    .map_or(runes.len(), |p| limit + p),
            };
            let mut line: String = runes[..i].iter().collect();
            line.push('\n');
            ret.push(line);
            while i < runes.len() && is_space(runes[i]) {
                i += 1;
            }
            runes = &runes[i..];
            existing = 0;
        }
        if !runes.is_empty() {
            ret.push(runes.iter().collect());
        }
        ret
    }
}

/// What to do once an element's children have been traversed — the code after
/// `traverseChildren` in each case of `handleElement` (html2text.go:109).
#[derive(Debug, Clone, Copy)]
enum After {
    Heading(Atom),
    Blockquote,
    Div,
    Li,
    Bold,
    Anchor(NodeId),
    Paragraph,
    Pre,
}

#[derive(Debug, Clone, Copy)]
enum Task {
    Visit(NodeId),
    After(After),
}

/// Go's recursive `traverse` / `handleElement` / `traverseChildren`, run on an explicit stack so
/// a tree as deep as the parser allows needs no deep call stack. The heading and bold cases'
/// `subCtx` become a pushed [`Ctx`], popped by their [`After`].
struct Traversal<'a> {
    doc: &'a Document,
    ctxs: Vec<Ctx>,
    tasks: Vec<Task>,
}

impl<'a> Traversal<'a> {
    fn new(doc: &'a Document, options: Options) -> Self {
        Traversal {
            doc,
            ctxs: vec![Ctx {
                options,
                ..Ctx::default()
            }],
            tasks: Vec::new(),
        }
    }

    fn ctx(&mut self) -> &mut Ctx {
        let n = self.ctxs.len() - 1;
        &mut self.ctxs[n]
    }

    fn run(mut self, node: NodeId) -> String {
        self.tasks.push(Task::Visit(node));
        while let Some(task) = self.tasks.pop() {
            match task {
                Task::Visit(n) => self.traverse(n),
                Task::After(a) => self.after(a),
            }
        }
        self.ctxs.pop().map(|c| c.buf).unwrap_or_default()
    }

    /// `traverseChildren` (html2text.go:372): schedule the children, first child on top.
    fn children(&mut self, node: NodeId) {
        let kids: Vec<NodeId> = self.doc.children(node).collect();
        self.tasks.extend(kids.into_iter().rev().map(Task::Visit));
    }

    /// Schedule `after` to run once `node`'s children are done.
    fn children_then(&mut self, node: NodeId, after: After) {
        self.tasks.push(Task::After(after));
        self.children(node);
    }

    /// Port of `traverse` (html2text.go:352).
    fn traverse(&mut self, node: NodeId) {
        let n = &self.doc[node];
        match n.node_type {
            NodeType::Text => {
                let data = if self.ctx().is_pre {
                    n.data.clone()
                } else {
                    trim_space(&collapse_spacing(&n.data)).to_owned()
                };
                self.ctx().emit(&data);
            }
            NodeType::Element => self.handle_element(node),
            _ => self.children(node),
        }
    }

    /// Port of `handleElement` (html2text.go:109), up to each case's `traverseChildren`.
    fn handle_element(&mut self, node: NodeId) {
        self.ctx().just_closed_div = false;
        let doc = self.doc;
        let n = &doc[node];
        match n.data_atom {
            Some(Atom::Br) => self.ctx().emit("\n"),
            Some(a @ (Atom::H1 | Atom::H2 | Atom::H3)) => {
                // `subCtx := textifyTraverseContext{}` — zero options, zero state.
                self.ctxs.push(Ctx::default());
                self.children_then(node, After::Heading(a));
            }
            Some(Atom::Blockquote) => {
                let ctx = self.ctx();
                ctx.blockquote_level += 1;
                if !ctx.options.text_only {
                    ctx.prefix = ">".repeat(ctx.blockquote_level) + " ";
                }
                ctx.emit("\n");
                if ctx.blockquote_level == 1 {
                    ctx.emit("\n");
                }
                self.children_then(node, After::Blockquote);
            }
            Some(Atom::Div) => {
                let ctx = self.ctx();
                if ctx.line_length > 0 {
                    ctx.emit("\n");
                }
                self.children_then(node, After::Div);
            }
            Some(Atom::Li) => {
                let ctx = self.ctx();
                if !ctx.options.text_only {
                    ctx.emit("* ");
                }
                self.children_then(node, After::Li);
            }
            Some(Atom::B | Atom::Strong) => {
                self.ctxs.push(Ctx {
                    ends_with_space: true,
                    ..Ctx::default()
                });
                self.children_then(node, After::Bold);
            }
            Some(Atom::A) => {
                // "If image is the only child, take its alt text as the link text."
                let img = n
                    .first_child
                    .filter(|&c| n.last_child == Some(c) && doc[c].data_atom == Some(Atom::Img));
                match img {
                    Some(img) => {
                        let alt = attr_val(doc, img, "alt");
                        if !alt.is_empty() {
                            self.ctx().emit(alt);
                        }
                        self.tasks.push(Task::After(After::Anchor(node)));
                    }
                    None => self.children_then(node, After::Anchor(node)),
                }
            }
            Some(Atom::P | Atom::Ul | Atom::Table) => {
                // `paragraphHandler` (html2text.go:273); a table takes it because PrettyTables
                // is off.
                self.ctx().emit("\n\n");
                self.children_then(node, After::Paragraph);
            }
            Some(Atom::Pre) => {
                self.ctx().is_pre = true;
                self.children_then(node, After::Pre);
            }
            Some(Atom::Style | Atom::Script | Atom::Head) => {
                // "Ignore the subtree."
            }
            // `Tfoot`, `Th`, `Tr`, `Td` without PrettyTables, and everything else.
            _ => self.children(node),
        }
    }

    /// The code after `traverseChildren` in each case of `handleElement`.
    fn after(&mut self, after: After) {
        match after {
            After::Heading(atom) => {
                let s = self.ctxs.pop().map(|c| c.buf).unwrap_or_default();
                let ctx = self.ctx();
                if ctx.options.text_only {
                    ctx.emit(&(s + ".\n\n"));
                    return;
                }
                let mut divider_len: isize = 0;
                for line in s.split('\n') {
                    let line_len = line.chars().count() as isize;
                    if line_len - 1 > divider_len {
                        divider_len = line_len - 1;
                    }
                }
                let ch = if atom == Atom::H1 { "*" } else { "-" };
                let divider = ch.repeat(divider_len as usize);
                if atom == Atom::H3 {
                    ctx.emit(&format!("\n\n{s}\n{divider}\n\n"));
                } else {
                    ctx.emit(&format!("\n\n{divider}\n{s}\n{divider}\n\n"));
                }
            }
            After::Blockquote => {
                let ctx = self.ctx();
                ctx.blockquote_level -= 1;
                if !ctx.options.text_only {
                    ctx.prefix = ">".repeat(ctx.blockquote_level);
                }
                if ctx.blockquote_level > 0 {
                    ctx.prefix.push(' ');
                }
                ctx.emit("\n\n");
            }
            After::Div => {
                let ctx = self.ctx();
                if !ctx.just_closed_div {
                    ctx.emit("\n");
                }
                ctx.just_closed_div = true;
            }
            After::Li => self.ctx().emit("\n"),
            After::Bold => {
                let s = self.ctxs.pop().map(|c| c.buf).unwrap_or_default();
                let ctx = self.ctx();
                if ctx.options.text_only {
                    ctx.emit(&(s + "."));
                } else {
                    ctx.emit(&format!("*{s}*"));
                }
            }
            After::Anchor(node) => self.after_anchor(node),
            After::Paragraph => self.ctx().emit("\n\n"),
            After::Pre => self.ctx().is_pre = false,
        }
    }

    /// The `atom.A` case after its children (html2text.go:227).
    fn after_anchor(&mut self, node: NodeId) {
        let doc = self.doc;
        let n = &doc[node];
        // "For simple link element content with single text node only, peek at the link text."
        let link_text = match n.first_child {
            Some(c) if doc[c].next_sibling.is_none() && doc[c].node_type == NodeType::Text => {
                doc[c].data.as_str()
            }
            _ => "",
        };
        let mut href_link = String::new();
        let href = attr_val(doc, node, "href");
        if !href.is_empty() {
            // `normalizeHrefLink` (html2text.go:465).
            let href = trim_space(href);
            let href = href.strip_prefix("mailto:").unwrap_or(href);
            let options = self.ctx().options;
            // "Don't print link href if it matches link element content or if the link is
            // empty."
            if !href.is_empty() && link_text != href && !options.omit_links && !options.text_only {
                href_link = format!("( {href} )");
            }
        }
        self.ctx().emit(&href_link);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h2t(s: &str) -> String {
        from_string(s).unwrap()
    }

    fn with(s: &str, options: Options) -> String {
        from_string_with_options(s, options).unwrap()
    }

    const TEXT_ONLY: Options = Options {
        omit_links: false,
        text_only: true,
    };
    const OMIT_LINKS: Options = Options {
        omit_links: true,
        text_only: false,
    };

    #[test]
    fn inline_text_is_joined_with_single_spaces() {
        assert_eq!(h2t("a   b\n\n c\t\td"), "a b c d");
        assert_eq!(h2t("a<span>b</span>"), "a b");
        assert_eq!(h2t("text<span>.after</span>"), "text.after");
        assert_eq!(h2t(""), "");
    }

    /// The zero sub-context starts with `endsWithSpace == false`, so the heading text it builds
    /// begins with a space (dropped later by the `"\n "` replacement) — and that space counts
    /// towards `lineLen`, making the divider as long as the visible text.
    #[test]
    fn headings_get_dividers_as_long_as_the_text() {
        assert_eq!(h2t("<h1>Title</h1>"), "*****\nTitle\n*****");
        assert_eq!(h2t("<h2>ab</h2>"), "--\nab\n--");
        assert_eq!(h2t("<h3>Third</h3>"), "Third\n-----");
        assert_eq!(h2t("<h1></h1>"), "");
        assert_eq!(h2t("<h2>日本語</h2>"), "---\n日本語\n---");
        assert_eq!(with("<h1>Title</h1>", TEXT_ONLY), "Title.");
    }

    #[test]
    fn links_print_their_target_unless_it_is_the_text() {
        assert_eq!(
            h2t("<a href='http://e.example'>Example</a>"),
            "Example ( http://e.example )"
        );
        assert_eq!(
            h2t("<a href='http://e.example'>http://e.example</a>"),
            "http://e.example"
        );
        assert_eq!(
            h2t("<a href='mailto:u@e.example'>u@e.example</a>"),
            "u@e.example"
        );
        assert_eq!(h2t("<a href='MAILTO:u@e'>u@e</a>"), "u@e ( MAILTO:u@e )");
        assert_eq!(h2t("<a href='h'><img alt='Alt'></a>"), "Alt ( h )");
        assert_eq!(with("<a href='h'>t</a>", OMIT_LINKS), "t");
        // The bold sub-context has zero options.
        assert_eq!(with("<b><a href='h'>t</a></b>", OMIT_LINKS), "*t ( h )*");
    }

    #[test]
    fn blockquotes_prefix_and_break_at_74_runes() {
        // The opening `\n\n` leaves an empty quoted line.
        assert_eq!(h2t("<blockquote>q</blockquote>"), "> \n> q");
        let long = format!("<blockquote>{} b</blockquote>", "a".repeat(75));
        assert_eq!(h2t(&long), format!("> \n> {}\n> b", "a".repeat(75)));
        let out = h2t(&format!("<blockquote>{}</blockquote>", "word ".repeat(30)));
        assert!(
            out.lines().all(|l| l.chars().count() <= MAX_LINE_LEN + 2),
            "{out}"
        );
    }

    #[test]
    fn two_byte_order_marks_are_dropped_and_a_third_is_kept() {
        assert_eq!(h2t("\u{feff}\u{feff}x"), "x");
        assert_eq!(h2t("\u{feff}\u{feff}\u{feff}x"), "\u{feff}x");
    }

    #[test]
    fn go_space_includes_nbsp_and_nel() {
        assert!(is_space('\u{a0}') && is_space('\u{85}') && is_space('\u{3000}'));
        assert!(!is_space('\u{feff}') && !is_space('\u{200b}') && !is_space('x'));
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            assert_eq!(is_space(c), c.is_whitespace(), "{c:?}");
        }
    }

    #[test]
    fn the_parser_error_is_the_error() {
        let err = from_string(&"<div>".repeat(600)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "html: open stack of elements exceeds 512 nodes"
        );
    }

    #[test]
    fn collapsing_helpers() {
        assert_eq!(collapse_spacing(" a \r\n\t b\u{a0} "), " a b\u{a0} ");
        assert_eq!(collapse_newlines("a\n\n\n\nb\nc\n\n"), "a\n\nb\nc\n\n");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    /// Every html2text case of the oracle — each Mattermost e-mail template executed through
    /// Mattermost's own template container, the raw sources, the targeted list under every
    /// option combination and the random corpus — converts to Go's text byte for byte, or fails
    /// with Go's error.
    #[test]
    fn from_string_matches_go() {
        let o: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_html2text.json"))
                .expect("the fixture is JSON");
        let cases = o["html2text"].as_array().expect("html2text cases");
        assert!(cases.len() > 2000, "{}", cases.len());
        let mut failures = Vec::new();
        let mut templates = 0;
        for case in cases {
            let src = case["src"].as_str().unwrap_or("?");
            let input = case["in"].as_str().expect("input");
            let options = Options {
                omit_links: case["opts"]["omit_links"].as_bool().unwrap_or(false),
                text_only: case["opts"]["text_only"].as_bool().unwrap_or(false),
            };
            if src.starts_with("template/") {
                templates += 1;
            }
            match from_string_with_options(input, options) {
                Ok(out) => {
                    if case["err"].is_string() {
                        failures.push(format!("{src}: Go failed with {}", case["err"]));
                    } else if Some(out.as_str()) != case["out"].as_str() {
                        failures.push(format!(
                            "{src} {options:?}: input {input:?}\n  ours {out:?}\n  go   {:?}",
                            case["out"]
                        ));
                    }
                }
                Err(e) => {
                    if case["err"].as_str() != Some(e.to_string().as_str()) {
                        failures.push(format!("{src}: we failed with {e}, Go: {}", case["err"]));
                    }
                }
            }
        }
        assert!(templates >= 90, "{templates} template renders");
        assert!(
            failures.is_empty(),
            "{} of {} cases differ:\n{}",
            failures.len(),
            cases.len(),
            failures[..failures.len().min(20)].join("\n")
        );
    }
}
