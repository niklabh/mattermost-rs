//! Port of `parser/html_block.go` and `parser/raw_html.go`.
//!
//! goldmark matches HTML with Go regexps. They are hand-matched here, each function naming the
//! pattern it stands for. Two properties of Go's engine are load-bearing and reproduced:
//!
//! - **`(?i)` is Unicode simple case folding**, so `s` also matches `ſ` (U+017F): `<ſcript>`
//!   opens a type-1 HTML block in Go. No other letter of the four type-1 tag names has a
//!   non-ASCII fold partner.
//! - **The inline tag patterns run over a `RuneReader`** (`text.Reader.Match`), which reads
//!   across the block's lines and stops at the first invalid UTF-8 byte *or literal U+FFFD*
//!   (goldmark's `readRuneReader` cannot tell them apart). [`RuneStream`] does the same.
//!
//! Every pattern's leftmost-first result is deterministic for these grammars (no later part can
//! fail in a way an earlier, less greedy choice would rescue), which is argued at each matcher.

use crate::ast::{Ast, NodeData, NodeId, NodeKind};
use crate::text::{BlockReader, Reader, Segment, Segments};
use crate::util;

use super::{Context, STATE_CLOSE, STATE_CONTINUE, STATE_NO_CHILDREN};

/// `allowedBlockTags` (html_block.go:14).
const ALLOWED_BLOCK_TAGS: &[&[u8]] = &[
    b"address",
    b"article",
    b"aside",
    b"base",
    b"basefont",
    b"blockquote",
    b"body",
    b"caption",
    b"center",
    b"col",
    b"colgroup",
    b"dd",
    b"details",
    b"dialog",
    b"dir",
    b"div",
    b"dl",
    b"dt",
    b"fieldset",
    b"figcaption",
    b"figure",
    b"footer",
    b"form",
    b"frame",
    b"frameset",
    b"h1",
    b"h2",
    b"h3",
    b"h4",
    b"h5",
    b"h6",
    b"head",
    b"header",
    b"hr",
    b"html",
    b"iframe",
    b"legend",
    b"li",
    b"link",
    b"main",
    b"menu",
    b"menuitem",
    b"meta",
    b"nav",
    b"noframes",
    b"ol",
    b"optgroup",
    b"option",
    b"p",
    b"param",
    b"search",
    b"section",
    b"summary",
    b"table",
    b"tbody",
    b"td",
    b"tfoot",
    b"th",
    b"thead",
    b"title",
    b"tr",
    b"track",
    b"ul",
];

const TYPE1_NAMES: [&[u8]; 4] = [b"script", b"pre", b"style", b"textarea"];

/// `^[ ]{0,3}<` — the index after the `<`.
fn after_lt(line: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < line.len() && line[i] == b' ' && i < 3 {
        i += 1;
    }
    (line.get(i) == Some(&b'<')).then_some(i + 1)
}

/// A `(?i)` literal of lowercase ASCII at `at`; `s` also matches `ſ` (UTF-8 `C5 BF`).
fn match_ci(line: &[u8], at: usize, name: &[u8]) -> Option<usize> {
    let mut k = at;
    for &ch in name {
        let c = *line.get(k)?;
        if c.to_ascii_lowercase() == ch {
            k += 1;
        } else if ch == b's' && c == 0xC5 && line.get(k + 1) == Some(&0xBF) {
            k += 2;
        } else {
            return None;
        }
    }
    Some(k)
}

/// Go's `\s`: `[\t\n\f\r ]`.
fn is_re_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// `(?:\r\n|\n)?$` at `at`.
fn at_line_end(line: &[u8], at: usize) -> bool {
    let rest = &line[at.min(line.len())..];
    rest.is_empty() || rest == b"\n" || rest == b"\r\n"
}

/// `htmlBlockType1OpenRegexp`:
/// `(?i)^[ ]{0,3}<(script|pre|style|textarea)(?:\s.*|>.*|/>.*|)(?:\r\n|\n)?$`.
/// The line holds at most one `\n`, at its end, so each `.*` alternative always completes.
fn type1_open(line: &[u8]) -> bool {
    let Some(i) = after_lt(line) else {
        return false;
    };
    for name in TYPE1_NAMES {
        if let Some(e) = match_ci(line, i, name) {
            let rest = &line[e..];
            if rest.first().is_some_and(|&c| is_re_space(c) || c == b'>')
                || rest.starts_with(b"/>")
                || at_line_end(line, e)
            {
                return true;
            }
        }
    }
    false
}

/// `htmlBlockType1CloseRegexp`: `(?i)^.*</(?:script|pre|style|textarea)>.*` — an end tag
/// before the first newline.
fn type1_close(text: &[u8]) -> bool {
    let first = match text.iter().position(|&c| c == b'\n') {
        Some(p) => &text[..p],
        None => text,
    };
    let mut i = 0;
    while i + 1 < first.len() {
        if first[i] == b'<' && first[i + 1] == b'/' {
            for name in TYPE1_NAMES {
                if let Some(e) = match_ci(first, i + 2, name)
                    && first.get(e) == Some(&b'>')
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// `[a-zA-Z]+[a-zA-Z0-9\-]*` at `at` — always maximal (a shorter name leaves a name character
/// that no continuation accepts).
fn tag_name(line: &[u8], at: usize) -> Option<usize> {
    if !line.get(at)?.is_ascii_alphabetic() {
        return None;
    }
    let mut e = at + 1;
    while e < line.len() && (line[e].is_ascii_alphanumeric() || line[e] == b'-') {
        e += 1;
    }
    Some(e)
}

/// `htmlBlockType6Regexp`:
/// `^[ ]{0,3}<(?:/[ ]*)?([a-zA-Z]+[a-zA-Z0-9\-]*)(?:[ ].*|>.*|/>.*|)(?:\r\n|\n)?$` — the tag
/// name's span.
fn type6(line: &[u8]) -> Option<(usize, usize)> {
    let mut i = after_lt(line)?;
    if line.get(i) == Some(&b'/') {
        i += 1;
        while line.get(i) == Some(&b' ') {
            i += 1;
        }
    }
    let e = tag_name(line, i)?;
    let rest = &line[e..];
    if rest.first().is_some_and(|&c| c == b' ' || c == b'>')
        || rest.starts_with(b"/>")
        || at_line_end(line, e)
    {
        return Some((i, e));
    }
    None
}

fn is_attr_space(c: u8) -> bool {
    matches!(c, b'\r' | b'\n' | b' ' | b'\t')
}

fn is_attr_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b':'
}

fn is_attr_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b':' | b'.' | b'_' | b'-')
}

fn is_unquoted_value_char(c: u32) -> bool {
    c > 0x20 && !matches!(c, 0x22 | 0x27 | 0x3d | 0x3c | 0x3e | 0x60)
}

/// One `attributePattern` (raw_html.go:52) over a byte line, from `at`:
/// `[\r\n \t]+[a-zA-Z_:][a-zA-Z0-9:._-]*(?:[\r\n \t]*=[\r\n \t]*(?:unquoted|'[^']*'|"[^"]*"))?`.
/// Taking the attribute (and its value) greedily never loses a match the regex would find:
/// what follows can only be another attribute, spaces or the tag end, none of which starts with
/// a name character or `=`.
fn attribute_bytes(line: &[u8], at: usize) -> Option<usize> {
    let mut k = at;
    while k < line.len() && is_attr_space(line[k]) {
        k += 1;
    }
    if k == at || !line.get(k).is_some_and(|&c| is_attr_name_start(c)) {
        return None;
    }
    k += 1;
    while k < line.len() && is_attr_name_char(line[k]) {
        k += 1;
    }
    let name_end = k;
    while k < line.len() && is_attr_space(line[k]) {
        k += 1;
    }
    if line.get(k) != Some(&b'=') {
        return Some(name_end);
    }
    k += 1;
    while k < line.len() && is_attr_space(line[k]) {
        k += 1;
    }
    match line.get(k) {
        Some(&q) if q == b'\'' || q == b'"' => match line[k + 1..].iter().position(|&c| c == q) {
            Some(p) => Some(k + 1 + p + 1),
            None => Some(name_end),
        },
        Some(&c) if is_unquoted_value_char(u32::from(c)) => {
            while k < line.len() && is_unquoted_value_char(u32::from(line[k])) {
                k += 1;
            }
            Some(k)
        }
        _ => Some(name_end),
    }
}

/// `htmlBlockType7Regexp`:
/// `^[ ]{0,3}<(/[ ]*)?([a-zA-Z]+[a-zA-Z0-9\-]*)(attribute*)[ ]*(?:>|/>)[ ]*(?:\r\n|\n)?$` —
/// `(group 1, tag name span, attributes non-empty)`.
/// What `htmlBlockType7Regexp` reports: group 1, the tag name's span, attributes non-empty.
type Type7Match<'l> = (Option<&'l [u8]>, (usize, usize), bool);

fn type7(line: &[u8]) -> Option<Type7Match<'_>> {
    let mut i = after_lt(line)?;
    let mut g1 = None;
    if line.get(i) == Some(&b'/') {
        let s = i;
        i += 1;
        while line.get(i) == Some(&b' ') {
            i += 1;
        }
        g1 = Some(&line[s..i]);
    }
    let e = tag_name(line, i)?;
    let name = (i, e);
    let attrs_start = e;
    let mut k = e;
    while let Some(n) = attribute_bytes(line, k) {
        k = n;
    }
    let has_attr = k != attrs_start;
    while line.get(k) == Some(&b' ') {
        k += 1;
    }
    if line.get(k) == Some(&b'>') {
        k += 1;
    } else if line[k.min(line.len())..].starts_with(b"/>") {
        k += 2;
    } else {
        return None;
    }
    while line.get(k) == Some(&b' ') {
        k += 1;
    }
    at_line_end(line, k).then_some((g1, name, has_attr))
}

/// Port of `htmlBlockParser.Open`.
pub(crate) fn html_block_open(
    ast: &mut Ast,
    reader: &mut Reader<'_>,
    pc: &mut Context,
) -> Option<(NodeId, u8)> {
    let (line, segment) = reader.peek_line();
    let line = line?;
    let last = pc.last_opened_block().map(|b| b.node);
    let mut typ: Option<u8> = None;
    let rest_after_lt = after_lt(&line).map(|i| &line[i..]);
    if type1_open(&line) {
        typ = Some(1);
    } else if rest_after_lt.is_some_and(|r| r.starts_with(b"!--")) {
        typ = Some(2);
    } else if rest_after_lt.is_some_and(|r| r.starts_with(b"?")) {
        typ = Some(3);
    } else if rest_after_lt
        .is_some_and(|r| r.len() >= 2 && r[0] == b'!' && r[1].is_ascii_uppercase())
    {
        typ = Some(4);
    } else if rest_after_lt.is_some_and(|r| r.starts_with(b"![CDATA[")) {
        typ = Some(5);
    } else if let Some((g1, (ns, ne), has_attr)) = type7(&line) {
        let is_close_tag = g1 == Some(b"/");
        let tag_name = line[ns..ne].to_ascii_lowercase();
        if ALLOWED_BLOCK_TAGS.contains(&tag_name.as_slice()) {
            typ = Some(6);
        } else if tag_name != b"script"
            && tag_name != b"style"
            && tag_name != b"pre"
            && !last.is_some_and(|l| ast.kind(l) == NodeKind::Paragraph)
            && !(is_close_tag && has_attr)
        {
            typ = Some(7);
        }
    }
    if typ.is_none()
        && let Some((ns, ne)) = type6(&line)
        && ALLOWED_BLOCK_TAGS.contains(&line[ns..ne].to_ascii_lowercase().as_slice())
    {
        typ = Some(6);
    }
    let typ = typ?;
    reader.advance_to_eol();
    let node = ast.new_node(NodeData::HTMLBlock {
        typ,
        closure_line: Segment::new(-1, -1),
    });
    ast.lines_mut(node).append(segment);
    Some((node, STATE_NO_CHILDREN))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Port of `htmlBlockParser.Continue`.
pub(crate) fn html_block_continue(ast: &mut Ast, node: NodeId, reader: &mut Reader<'_>) -> u8 {
    let typ = match ast.data(node) {
        NodeData::HTMLBlock { typ, .. } => *typ,
        _ => 0,
    };
    let (line, segment) = reader.peek_line();
    let line = line.unwrap_or_default();
    let source = reader.source();
    let first_line = (ast.lines(node).len() == 1).then(|| ast.lines(node).at(0));
    let set_closure = |ast: &mut Ast| {
        if let NodeData::HTMLBlock { closure_line, .. } = ast.data_mut(node) {
            *closure_line = segment;
        }
    };
    match typ {
        1 => {
            if let Some(f) = first_line
                && type1_close(&f.value(source))
            {
                return STATE_CLOSE;
            }
            if type1_close(&line) {
                set_closure(ast);
                reader.advance_to_eol();
                return STATE_CLOSE;
            }
        }
        2..=5 => {
            let closure: &[u8] = match typ {
                2 => b"-->",
                3 => b"?>",
                4 => b">",
                _ => b"]]>",
            };
            if let Some(f) = first_line
                && contains(&f.value(source), closure)
            {
                return STATE_CLOSE;
            }
            if contains(&line, closure) {
                set_closure(ast);
                reader.advance_to_eol();
                return STATE_CLOSE;
            }
        }
        6 | 7 if util::is_blank(&line) => return STATE_CLOSE,
        _ => {}
    }
    ast.lines_mut(node).append(segment);
    reader.advance_to_eol();
    STATE_CONTINUE | STATE_NO_CHILDREN
}

// ---- raw_html.go ----

/// The rune stream `regexp.FindReaderSubmatchIndex` sees through `readRuneReader`: runes of the
/// block from the current position, ending at the block's end or at the first invalid byte or
/// literal U+FFFD. Buffered so the matchers can look ahead and back off.
struct RuneStream<'r, 'a> {
    block: &'r mut BlockReader<'a>,
    buf: Vec<(char, isize)>,
    done: bool,
}

impl<'r, 'a> RuneStream<'r, 'a> {
    fn new(block: &'r mut BlockReader<'a>) -> Self {
        RuneStream {
            block,
            buf: Vec::new(),
            done: false,
        }
    }

    fn get(&mut self, k: usize) -> Option<char> {
        while self.buf.len() <= k && !self.done {
            match self.block.read_rune() {
                Some(r) => self.buf.push(r),
                None => self.done = true,
            }
        }
        self.buf.get(k).map(|&(c, _)| c)
    }

    fn is(&mut self, k: usize, f: impl Fn(char) -> bool) -> bool {
        self.get(k).is_some_and(f)
    }

    fn bytes_to(&self, k: usize) -> isize {
        self.buf[..k].iter().map(|&(_, s)| s).sum()
    }
}

fn ascii(c: char, f: fn(u8) -> bool) -> bool {
    c.is_ascii() && f(c as u8)
}

/// `spaceOrOneNewline*` = `(?:[ \t]|(?:\r\n|\n){0,1})*`: any run of spaces, tabs, `\n` and
/// `\r\n` pairs (a lone `\r` stops it).
fn space_or_newlines(s: &mut RuneStream<'_, '_>, mut k: usize) -> usize {
    loop {
        match s.get(k) {
            Some(' ') | Some('\t') | Some('\n') => k += 1,
            Some('\r') if s.get(k + 1) == Some('\n') => k += 2,
            _ => return k,
        }
    }
}

/// One `attributePattern` over the rune stream (see [`attribute_bytes`]).
fn attribute_runes(s: &mut RuneStream<'_, '_>, at: usize) -> Option<usize> {
    let mut k = at;
    while s.is(k, |c| ascii(c, is_attr_space)) {
        k += 1;
    }
    if k == at || !s.is(k, |c| ascii(c, is_attr_name_start)) {
        return None;
    }
    k += 1;
    while s.is(k, |c| ascii(c, is_attr_name_char)) {
        k += 1;
    }
    let name_end = k;
    while s.is(k, |c| ascii(c, is_attr_space)) {
        k += 1;
    }
    if s.get(k) != Some('=') {
        return Some(name_end);
    }
    k += 1;
    while s.is(k, |c| ascii(c, is_attr_space)) {
        k += 1;
    }
    match s.get(k) {
        Some(q) if q == '\'' || q == '"' => {
            let mut j = k + 1;
            loop {
                match s.get(j) {
                    Some(c) if c == q => return Some(j + 1),
                    Some(_) => j += 1,
                    None => return Some(name_end),
                }
            }
        }
        Some(c) if is_unquoted_value_char(c as u32) => {
            while s.is(k, |c| is_unquoted_value_char(c as u32)) {
                k += 1;
            }
            Some(k)
        }
        _ => Some(name_end),
    }
}

fn rune_tag_name(s: &mut RuneStream<'_, '_>, at: usize) -> Option<usize> {
    if !s.is(at, |c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut k = at + 1;
    while s.is(k, |c| c.is_ascii_alphanumeric() || c == '-') {
        k += 1;
    }
    Some(k)
}

/// `openTagRegexp`: `^<tagname attribute* spaceOrOneNewline* /?>` — the match length in runes.
fn open_tag(s: &mut RuneStream<'_, '_>) -> Option<usize> {
    if s.get(0) != Some('<') {
        return None;
    }
    let mut k = rune_tag_name(s, 1)?;
    while let Some(n) = attribute_runes(s, k) {
        k = n;
    }
    k = space_or_newlines(s, k);
    if s.get(k) == Some('/') {
        k += 1;
    }
    (s.get(k) == Some('>')).then_some(k + 1)
}

/// `closeTagRegexp`: `^</tagname spaceOrOneNewline* >`.
fn close_tag(s: &mut RuneStream<'_, '_>) -> Option<usize> {
    if s.get(0) != Some('<') || s.get(1) != Some('/') {
        return None;
    }
    let k = rune_tag_name(s, 2)?;
    let k = space_or_newlines(s, k);
    (s.get(k) == Some('>')).then_some(k + 1)
}

/// Port of `text.Reader.Match` for the two tag patterns.
fn block_match(block: &mut BlockReader<'_>, close: bool) -> bool {
    let (oldline, oldseg) = block.position();
    let bytes = {
        let mut s = RuneStream::new(block);
        let m = if close {
            close_tag(&mut s)
        } else {
            open_tag(&mut s)
        };
        m.map(|k| s.bytes_to(k))
    };
    block.set_position(oldline, oldseg);
    match bytes {
        Some(n) => {
            block.advance(n);
            true
        }
        None => false,
    }
}

fn raw_html_node(ast: &mut Ast, segments: Segments) -> NodeId {
    ast.new_node(NodeData::RawHTML { segments })
}

/// Port of `rawHTMLParser.parseMultiLineRegexp`.
fn parse_multi_line_regexp(
    ast: &mut Ast,
    block: &mut BlockReader<'_>,
    close: bool,
) -> Option<NodeId> {
    let (sline, ssegment) = block.position();
    if !block_match(block, close) {
        return None;
    }
    let mut segs = Segments::new();
    let (eline, esegment) = block.position();
    block.set_position(sline, ssegment);
    loop {
        let (line, segment) = block.peek_line();
        if line.is_none() {
            break;
        }
        let (l, _) = block.position();
        let start = if l == sline {
            ssegment.start
        } else {
            segment.start
        };
        let end = if l == eline {
            esegment.start
        } else {
            segment.stop
        };
        segs.append(Segment::new(start, end));
        if l == eline {
            block.advance(end - start);
            break;
        }
        block.advance_line();
    }
    Some(raw_html_node(ast, segs))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Port of `rawHTMLParser.parseComment`.
fn parse_comment(ast: &mut Ast, block: &mut BlockReader<'_>) -> Option<NodeId> {
    let (saved_line, saved_segment) = block.position();
    let mut segs = Segments::new();
    let (line, segment) = block.peek_line();
    let line = line?;
    for empty in [&b"<!-->"[..], &b"<!--->"[..]] {
        if line.starts_with(empty) {
            segs.append(segment.with_stop(segment.start + empty.len() as isize));
            block.advance(empty.len() as isize);
            return Some(raw_html_node(ast, segs));
        }
    }
    let mut offset: isize = 4;
    let mut line: Vec<u8> = line[4..].to_vec();
    let mut segment = segment;
    loop {
        if let Some(index) = find(&line, b"-->") {
            let n = offset + index as isize + 3;
            segs.append(segment.with_stop(segment.start + n));
            block.advance(n);
            return Some(raw_html_node(ast, segs));
        }
        offset = 0;
        segs.append(segment);
        block.advance_line();
        let (l, s) = block.peek_line();
        match l {
            Some(l) => {
                line = l.into_owned();
                segment = s;
            }
            None => break,
        }
    }
    block.set_position(saved_line, saved_segment);
    None
}

/// Port of `rawHTMLParser.parseUntil`.
fn parse_until(ast: &mut Ast, block: &mut BlockReader<'_>, closer: &[u8]) -> Option<NodeId> {
    let (saved_line, saved_segment) = block.position();
    let mut segs = Segments::new();
    loop {
        let (line, segment) = block.peek_line();
        let Some(line) = line else {
            break;
        };
        if let Some(index) = find(&line, closer) {
            let n = (index + closer.len()) as isize;
            segs.append(segment.with_stop(segment.start + n));
            block.advance(n);
            return Some(raw_html_node(ast, segs));
        }
        segs.append(segment);
        block.advance_line();
    }
    block.set_position(saved_line, saved_segment);
    None
}

/// Port of `rawHTMLParser.Parse`.
pub(crate) fn raw_html_parse(ast: &mut Ast, block: &mut BlockReader<'_>) -> Option<NodeId> {
    let (line, _) = block.peek_line();
    let line = line?;
    if line.len() > 1 && util::is_alpha_numeric(line[1]) {
        return parse_multi_line_regexp(ast, block, false);
    }
    if line.len() > 2 && line[1] == b'/' && util::is_alpha_numeric(line[2]) {
        return parse_multi_line_regexp(ast, block, true);
    }
    if line.starts_with(b"<!--") {
        return parse_comment(ast, block);
    }
    if line.starts_with(b"<?") {
        return parse_until(ast, block, b"?>");
    }
    if line.len() > 2 && line[1] == b'!' && line[2].is_ascii_uppercase() {
        return parse_until(ast, block, b">");
    }
    if line.starts_with(b"<![CDATA[") {
        return parse_until(ast, block, b"]]>");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type1_open_rules() {
        assert!(type1_open(b"<script>\n"));
        assert!(type1_open(b"   <PRE class='x'>\n"));
        assert!(type1_open(b"<style"));
        assert!(type1_open(b"<textarea\r\n"));
        assert!(type1_open("<\u{17f}cript>".as_bytes()));
        assert!(!type1_open(b"<scriptx>\n"));
        assert!(!type1_open(b"    <script>\n"));
        assert!(!type1_open(b"<script/x\n"));
    }

    #[test]
    fn type1_close_rules() {
        assert!(type1_close(b"x</SCRIPT>y\n"));
        assert!(!type1_close(b"x\n</script>"));
        assert!(!type1_close(b"</script >"));
    }

    #[test]
    fn type6_and_type7_rules() {
        assert_eq!(type6(b"<div>\n"), Some((1, 4)));
        assert_eq!(type6(b"</ div class=x\n"), Some((3, 6)));
        assert_eq!(type6(b"<div\tx\n"), None);
        let (g1, _, attr) = type7(b"<a href=\"x\">\n").unwrap();
        assert!(g1.is_none());
        assert!(attr);
        let (g1, _, attr) = type7(b"</a >\n").unwrap();
        assert_eq!(g1, Some(&b"/"[..]));
        assert!(!attr);
        assert!(type7(b"<a b=c/>\n").is_some());
        assert!(type7(b"<a / >\n").is_none());
        assert!(type7(b"<a>x\n").is_none());
        assert!(type7(b"<a\t>\n").is_none());
    }
}
