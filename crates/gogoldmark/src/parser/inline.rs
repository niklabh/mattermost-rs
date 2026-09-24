//! The inline phase: `parser.parseBlock` (parser.go:1122), delimiter processing
//! (delimiter.go), and the inline parsers — code spans, links and images, `<...>` autolinks,
//! emphasis — plus the GFM ones (strikethrough, task-list checkboxes, linkify). Raw inline HTML
//! is in [`super::html`].

use crate::ast::{
    Ast, AutoLinkType, Delimiter, DelimiterProcessor, LinkLabelState, NodeData, NodeId, NodeKind,
    ReferenceLink, ReferenceLinkType,
};
use crate::text::{BlockReader, FindClosureOptions, Segment, Segments};
use crate::util;

use super::{Context, InlineParserKind, LinkBottom, Parser};

const LINE_BREAK_HARD: u8 = 1;
const LINE_BREAK_SOFT: u8 = 2;
const LINE_BREAK_VISIBLE: u8 = 4;

/// The `bottom` argument of `ProcessDelimiters`: Go's untyped nil (`Nil`) behaves differently
/// from a typed nil `*Delimiter` (`Node(None)`); see [`LinkBottom`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum Bottom {
    Nil,
    Node(Option<NodeId>),
}

impl Parser {
    /// Port of `parser.parseBlock`: splits a leaf block's lines into inline nodes.
    pub(crate) fn parse_block(
        &self,
        ast: &mut Ast,
        block: &mut BlockReader<'_>,
        parent: NodeId,
        pc: &mut Context,
    ) {
        if ast.is_raw(parent) {
            return;
        }
        let mut escaped = false;
        let source = block.source();
        let lines = ast.lines(parent).as_slice().to_vec();
        block.reset(&lines);
        'retry: loop {
            let (line, _) = block.peek_line();
            let Some(line) = line else {
                break;
            };
            let mut line_length = line.len();
            let mut flags = 0u8;
            // Go indexes `line[lineLength-1]` and would panic on an empty line; none is reachable.
            let Some(&last_byte) = line.last() else {
                break;
            };
            let has_new_line = last_byte == b'\n';
            let ll = line_length;
            if ((ll >= 3 && line[ll - 2] == b'\\' && line[ll - 3] != b'\\')
                || (ll == 2 && line[ll - 2] == b'\\'))
                && has_new_line
            {
                line_length -= 2;
                flags |= LINE_BREAK_HARD | LINE_BREAK_VISIBLE;
            } else if ((ll >= 4
                && line[ll - 3] == b'\\'
                && line[ll - 2] == b'\r'
                && line[ll - 4] != b'\\')
                || (ll == 3 && line[ll - 3] == b'\\' && line[ll - 2] == b'\r'))
                && has_new_line
            {
                line_length -= 3;
                flags |= LINE_BREAK_HARD | LINE_BREAK_VISIBLE;
            } else if ll >= 3 && line[ll - 3] == b' ' && line[ll - 2] == b' ' && has_new_line {
                line_length -= 3;
                flags |= LINE_BREAK_HARD;
            } else if ll >= 4
                && line[ll - 4] == b' '
                && line[ll - 3] == b' '
                && line[ll - 2] == b'\r'
                && has_new_line
            {
                line_length -= 4;
                flags |= LINE_BREAK_HARD;
            } else if has_new_line {
                flags |= LINE_BREAK_SOFT;
            }

            let (l, mut start_position) = block.position();
            let mut n: isize = 0;
            for i in 0..line_length {
                let c = line[i];
                if c == b'\n' {
                    break;
                }
                let is_space = util::is_space(c) && c != b'\r' && c != b'\n';
                let is_punct = util::is_punct(c);
                if (is_punct && !escaped) || is_space || i == 0 {
                    let parser_char = if is_space || (i == 0 && !is_punct) {
                        b' '
                    } else {
                        c
                    };
                    let ips = &self.inline_parsers[parser_char as usize];
                    if !ips.is_empty() {
                        block.advance(n);
                        n = 0;
                        let (saved_line, saved_position) = block.position();
                        if i != 0 {
                            let (_, current_position) = block.position();
                            ast.merge_or_append_text_segment(
                                parent,
                                start_position.between(current_position),
                            );
                            start_position = block.position().1;
                        }
                        let mut inline_node = None;
                        for &ip in ips {
                            inline_node = self.parse_inline(ip, ast, parent, block, pc);
                            if let Some(node) = inline_node {
                                if ast.pos(node) < 0 {
                                    ast.set_pos(node, start_position.start);
                                }
                                break;
                            }
                            block.set_position(saved_line, saved_position);
                        }
                        if let Some(node) = inline_node {
                            ast.append_child(parent, node);
                            continue 'retry;
                        }
                    }
                }
                if escaped {
                    escaped = false;
                    n += 1;
                    continue;
                }
                if c == b'\\' {
                    escaped = true;
                    n += 1;
                    continue;
                }
                escaped = false;
                n += 1;
            }
            if n != 0 {
                block.advance(n);
            }
            let (current_l, current_position) = block.position();
            if l != current_l {
                continue;
            }
            let diff = start_position.between(current_position);
            let seg = if flags & (LINE_BREAK_HARD | LINE_BREAK_VISIBLE)
                == LINE_BREAK_HARD | LINE_BREAK_VISIBLE
            {
                diff
            } else {
                diff.trim_right_space(source)
            };
            let text = ast.new_text_segment(seg);
            ast.set_soft_line_break(text, flags & LINE_BREAK_SOFT != 0);
            ast.set_hard_line_break(text, flags & LINE_BREAK_HARD != 0);
            ast.append_child(parent, text);
            block.advance_line();
        }

        process_delimiters(ast, Bottom::Nil, pc);
        for &ip in &self.close_blockers {
            if ip == InlineParserKind::Link {
                link_close_block(ast, pc);
            }
        }
    }

    fn parse_inline(
        &self,
        ip: InlineParserKind,
        ast: &mut Ast,
        parent: NodeId,
        block: &mut BlockReader<'_>,
        pc: &mut Context,
    ) -> Option<NodeId> {
        match ip {
            InlineParserKind::TaskCheckBox => task_check_box_parse(ast, parent, block),
            InlineParserKind::CodeSpan => code_span_parse(ast, block),
            InlineParserKind::Link => link_parse(ast, parent, block, pc),
            InlineParserKind::AutoLink => auto_link_parse(ast, block),
            InlineParserKind::RawHtml => super::html::raw_html_parse(ast, block),
            InlineParserKind::Emphasis => {
                delimiter_parse(ast, block, pc, DelimiterProcessor::Emphasis)
            }
            InlineParserKind::Strikethrough => {
                delimiter_parse(ast, block, pc, DelimiterProcessor::Strikethrough)
            }
            InlineParserKind::Linkify => linkify_parse(ast, parent, block, pc),
        }
    }
}

// ---- delimiter.go ----

fn delim(ast: &Ast, id: NodeId) -> Option<&Delimiter> {
    match ast.data(id) {
        NodeData::Delimiter(d) => Some(d),
        _ => None,
    }
}

fn delim_mut(ast: &mut Ast, id: NodeId) -> Option<&mut Delimiter> {
    match ast.data_mut(id) {
        NodeData::Delimiter(d) => Some(d),
        _ => None,
    }
}

impl DelimiterProcessor {
    fn is_delimiter(self, b: u8) -> bool {
        match self {
            DelimiterProcessor::Emphasis => b == b'*' || b == b'_',
            DelimiterProcessor::Strikethrough => b == b'~',
        }
    }
}

/// Port of `parser.ScanDelimiter`: the flanking rules.
pub(crate) fn scan_delimiter(
    line: &[u8],
    before: char,
    minimum: isize,
    processor: DelimiterProcessor,
) -> Option<Delimiter> {
    let c = *line.first()?;
    if !processor.is_delimiter(c) {
        return None;
    }
    let mut j = 0;
    while j < line.len() && line[j] == c {
        j += 1;
    }
    if (j as isize) < minimum {
        return None;
    }
    let after = if j != line.len() {
        util::to_rune(line, j)
    } else {
        ' '
    };
    let before_is_punctuation = util::is_punct_rune(before);
    let before_is_whitespace = util::is_space_rune(before);
    let after_is_punctuation = util::is_punct_rune(after);
    let after_is_whitespace = util::is_space_rune(after);

    let is_left = !after_is_whitespace
        && (!after_is_punctuation || before_is_whitespace || before_is_punctuation);
    let is_right = !before_is_whitespace
        && (!before_is_punctuation || after_is_whitespace || after_is_punctuation);

    let (can_open, can_close) = if line[0] == b'_' {
        (
            is_left && (!is_right || before_is_punctuation),
            is_right && (!is_left || after_is_punctuation),
        )
    } else {
        (is_left, is_right)
    };
    Some(Delimiter {
        segment: Segment::default(),
        can_open,
        can_close,
        length: j as isize,
        original_length: j as isize,
        ch: c,
        previous_delimiter: None,
        next_delimiter: None,
        processor,
    })
}

/// Port of `Delimiter.CalcComsumption` (the rule of three).
fn calc_consumption(d: &Delimiter, closer: &Delimiter) -> isize {
    if (d.can_close || closer.can_open)
        && (d.original_length + closer.original_length) % 3 == 0
        && closer.original_length % 3 != 0
    {
        return 0;
    }
    if d.length >= 2 && closer.length >= 2 {
        return 2;
    }
    1
}

fn consume_characters(ast: &mut Ast, id: NodeId, n: isize) {
    if let Some(d) = delim_mut(ast, id) {
        d.length -= n;
        d.segment = d.segment.with_stop(d.segment.start + d.length);
    }
}

fn push_delimiter(ast: &mut Ast, pc: &mut Context, d: NodeId) {
    match pc.last_delimiter {
        None => {
            pc.delimiters = Some(d);
            pc.last_delimiter = Some(d);
        }
        Some(l) => {
            pc.last_delimiter = Some(d);
            if let Some(ld) = delim_mut(ast, l) {
                ld.next_delimiter = Some(d);
            }
            if let Some(dd) = delim_mut(ast, d) {
                dd.previous_delimiter = Some(l);
            }
        }
    }
}

/// Port of `parseContext.RemoveDelimiter`: unlink, then turn what is left of the run into text.
pub(crate) fn remove_delimiter(ast: &mut Ast, pc: &mut Context, d: NodeId) {
    let Some(dd) = delim(ast, d) else {
        return;
    };
    let (prev, next, length, segment) = (
        dd.previous_delimiter,
        dd.next_delimiter,
        dd.length,
        dd.segment,
    );
    match prev {
        None => pc.delimiters = next,
        Some(p) => {
            if let Some(pd) = delim_mut(ast, p) {
                pd.next_delimiter = next;
            }
            if let Some(nx) = next
                && let Some(nd) = delim_mut(ast, nx)
            {
                nd.previous_delimiter = prev;
            }
        }
    }
    if next.is_none() {
        pc.last_delimiter = prev;
    }
    if let Some(first) = pc.delimiters
        && let Some(fd) = delim_mut(ast, first)
    {
        fd.previous_delimiter = None;
    }
    if let Some(last) = pc.last_delimiter
        && let Some(ld) = delim_mut(ast, last)
    {
        ld.next_delimiter = None;
    }
    if let Some(dd) = delim_mut(ast, d) {
        dd.next_delimiter = None;
        dd.previous_delimiter = None;
    }
    let Some(parent) = ast.parent(d) else {
        return;
    };
    if length != 0 {
        ast.merge_or_replace_text_segment(parent, d, segment);
    } else {
        ast.remove_child(parent, d);
    }
}

/// Port of `parseContext.ClearDelimiters`.
fn clear_delimiters(ast: &mut Ast, pc: &mut Context, bottom: Option<NodeId>) {
    let Some(last) = pc.last_delimiter else {
        return;
    };
    let mut c = Some(last);
    while let Some(cn) = c {
        if Some(cn) == bottom {
            break;
        }
        let prev = ast.previous_sibling(cn);
        if ast.kind(cn) == NodeKind::Delimiter {
            remove_delimiter(ast, pc, cn);
        }
        c = prev;
    }
}

/// Port of `parser.ProcessDelimiters`.
pub(crate) fn process_delimiters(ast: &mut Ast, bottom: Bottom, pc: &mut Context) {
    let Some(last_delimiter) = pc.last_delimiter else {
        return;
    };
    let bottom_ptr = match bottom {
        Bottom::Nil => None,
        Bottom::Node(b) => b,
    };
    let mut closer: Option<NodeId> = None;
    match bottom {
        Bottom::Node(b) => {
            if b != Some(last_delimiter) {
                let mut c = ast.previous_sibling(last_delimiter);
                while let Some(cn) = c {
                    if Some(cn) == b {
                        break;
                    }
                    if ast.kind(cn) == NodeKind::Delimiter {
                        closer = Some(cn);
                    }
                    c = ast.previous_sibling(cn);
                }
            }
        }
        Bottom::Nil => closer = pc.delimiters,
    }
    if closer.is_none() {
        clear_delimiters(ast, pc, bottom_ptr);
        return;
    }
    while let Some(cl) = closer {
        let Some(cd) = delim(ast, cl).cloned() else {
            break;
        };
        if !cd.can_close {
            closer = cd.next_delimiter;
            continue;
        }
        let mut consume = 0;
        let mut found = false;
        let mut maybe_opener = false;
        let mut opener = cd.previous_delimiter;
        while let Some(op) = opener {
            if Some(op) == bottom_ptr {
                break;
            }
            let Some(od) = delim(ast, op) else {
                break;
            };
            if od.can_open && od.processor == cd.processor && od.ch == cd.ch {
                maybe_opener = true;
                consume = calc_consumption(od, &cd);
                if consume > 0 {
                    found = true;
                    break;
                }
            }
            opener = od.previous_delimiter;
        }
        let (true, Some(op)) = (found, opener) else {
            let next = cd.next_delimiter;
            if !maybe_opener && !cd.can_open {
                remove_delimiter(ast, pc, cl);
            }
            closer = next;
            continue;
        };
        consume_characters(ast, op, consume);
        consume_characters(ast, cl, consume);

        let (op_processor, op_start) = match delim(ast, op) {
            Some(d) => (d.processor, d.segment.start),
            None => break,
        };
        let node = match op_processor {
            DelimiterProcessor::Emphasis => ast.new_node(NodeData::Emphasis {
                level: consume as u8,
            }),
            DelimiterProcessor::Strikethrough => ast.new_node(NodeData::Strikethrough),
        };
        ast.set_pos(node, op_start);
        let parent = ast.parent(op);
        let mut child = ast.next_sibling(op);
        while let Some(ch) = child {
            if ch == cl {
                break;
            }
            let next = ast.next_sibling(ch);
            ast.append_child(node, ch);
            child = next;
        }
        if let Some(p) = parent {
            ast.insert_after(p, op, node);
        }
        let mut c = delim(ast, op).and_then(|d| d.next_delimiter);
        while let Some(cc) = c {
            if cc == cl {
                break;
            }
            let next = delim(ast, cc).and_then(|d| d.next_delimiter);
            remove_delimiter(ast, pc, cc);
            c = next;
        }
        if delim(ast, op).is_some_and(|d| d.length == 0) {
            remove_delimiter(ast, pc, op);
        }
        if let Some(d) = delim(ast, cl)
            && d.length == 0
        {
            let next = d.next_delimiter;
            remove_delimiter(ast, pc, cl);
            closer = next;
        }
    }
    clear_delimiters(ast, pc, bottom_ptr);
}

/// `emphasisParser.Parse` and `strikethroughParser.Parse`.
fn delimiter_parse(
    ast: &mut Ast,
    block: &mut BlockReader<'_>,
    pc: &mut Context,
    processor: DelimiterProcessor,
) -> Option<NodeId> {
    let before = block.precending_character();
    let (line, segment) = block.peek_line();
    let line = line?;
    let mut d = scan_delimiter(&line, before, 1, processor)?;
    if processor == DelimiterProcessor::Strikethrough && (d.original_length > 2 || before == '~') {
        return None;
    }
    d.segment = segment.with_stop(segment.start + d.original_length);
    let len = d.original_length;
    let node = ast.new_node(NodeData::Delimiter(d));
    block.advance(len);
    push_delimiter(ast, pc, node);
    Some(node)
}

// ---- code_span.go ----

fn is_space_or_newline(c: u8) -> bool {
    c == b' ' || c == b'\n'
}

fn code_span_parse(ast: &mut Ast, block: &mut BlockReader<'_>) -> Option<NodeId> {
    let (line, start_segment) = block.peek_line();
    let line = line?;
    let mut opener = 0usize;
    while opener < line.len() && line[opener] == b'`' {
        opener += 1;
    }
    block.advance(opener as isize);
    let (l, pos) = block.position();
    let node = ast.new_node(NodeData::CodeSpan);
    'outer: loop {
        let (line, segment) = block.peek_line();
        let Some(line) = line else {
            block.set_position(l, pos);
            return Some(
                ast.new_text_segment(
                    start_segment.with_stop(start_segment.start + opener as isize),
                ),
            );
        };
        let mut i = 0usize;
        while i < line.len() {
            let c = line[i];
            if c == b'`' {
                let oldi = i;
                while i < line.len() && line[i] == b'`' {
                    i += 1;
                }
                let closure = i - oldi;
                if closure == opener && (i >= line.len() || line[i] != b'`') {
                    let seg = segment.with_stop(segment.start + (i - closure) as isize);
                    if !seg.is_empty() {
                        let t = ast.new_raw_text_segment(seg);
                        ast.append_child(node, t);
                    }
                    block.advance(i as isize);
                    break 'outer;
                }
                // Go's for-loop increments past the run's first non-backtick byte.
                i += 1;
                continue;
            }
            i += 1;
        }
        let t = ast.new_raw_text_segment(segment);
        ast.append_child(node, t);
        block.advance_line();
    }
    let source = block.source();
    let blank = ast.children(node).iter().all(|&c| {
        ast.text_segment(c)
            .is_none_or(|s| util::is_blank(&s.value(source)))
    });
    if !blank
        && let (Some(first), Some(last)) = (ast.first_child(node), ast.last_child(node))
        && let (Some(fs), Some(ls)) = (ast.text_segment(first), ast.text_segment(last))
    {
        let at = |i: isize| usize::try_from(i).ok().and_then(|i| source.get(i).copied());
        let mut should_trim = true;
        if !(!fs.is_empty() && at(fs.start).is_some_and(is_space_or_newline)) {
            should_trim = false;
        }
        if !(!ls.is_empty() && at(ls.stop - 1).is_some_and(is_space_or_newline)) {
            should_trim = false;
        }
        if should_trim {
            if let Some(s) = ast.text_segment_mut(first) {
                *s = s.with_start(s.start + 1);
            }
            if let Some(ls) = ast.text_segment(last)
                && let Some(s) = ast.text_segment_mut(last)
            {
                *s = ls.with_stop(ls.stop - 1);
            }
        }
    }
    Some(node)
}

// ---- auto_link.go ----

fn auto_link_parse(ast: &mut Ast, block: &mut BlockReader<'_>) -> Option<NodeId> {
    let (line, segment) = block.peek_line();
    let line = line?;
    let rest = line.get(1..).unwrap_or(&[]);
    let mut stop = util::find_email_index(rest);
    let mut typ = AutoLinkType::Email;
    if stop < 0 {
        stop = util::find_url_index(rest);
        typ = AutoLinkType::Url;
    }
    if stop < 0 {
        return None;
    }
    stop += 1;
    if stop as usize >= line.len() || line[stop as usize] != b'>' {
        return None;
    }
    let value = Segment::new(segment.start + 1, segment.start + stop);
    block.advance(stop + 1);
    Some(ast.new_node(NodeData::AutoLink {
        typ,
        protocol: None,
        value,
    }))
}

// ---- link.go ----

fn label_state(ast: &Ast, id: NodeId) -> Option<&LinkLabelState> {
    match ast.data(id) {
        NodeData::LinkLabelState(s) => Some(s),
        _ => None,
    }
}

fn label_state_mut(ast: &mut Ast, id: NodeId) -> Option<&mut LinkLabelState> {
    match ast.data_mut(id) {
        NodeData::LinkLabelState(s) => Some(s),
        _ => None,
    }
}

/// Port of `linkLabelStateLength`.
fn link_label_state_length(ast: &Ast, v: NodeId) -> isize {
    let Some(s) = label_state(ast, v) else {
        return 0;
    };
    let (Some(last), Some(first)) = (s.last, s.first) else {
        return 0;
    };
    match (label_state(ast, last), label_state(ast, first)) {
        (Some(l), Some(f)) => l.segment.stop - f.segment.start,
        _ => 0,
    }
}

/// Port of `pushLinkLabelState`.
fn push_link_label_state(ast: &mut Ast, pc: &mut Context, v: NodeId) {
    match pc.link_label_state {
        None => {
            if let Some(s) = label_state_mut(ast, v) {
                s.first = Some(v);
                s.last = Some(v);
            }
            pc.link_label_state = Some(v);
        }
        Some(list) => {
            let l = label_state(ast, list).and_then(|s| s.last);
            if let Some(s) = label_state_mut(ast, list) {
                s.last = Some(v);
            }
            if let Some(l) = l {
                if let Some(ls) = label_state_mut(ast, l) {
                    ls.next = Some(v);
                }
                if let Some(vs) = label_state_mut(ast, v) {
                    vs.prev = Some(l);
                }
            }
        }
    }
}

/// Port of `removeLinkLabelState`, including its bug: removing the head sets the new head's
/// `First` to the removed state.
fn remove_link_label_state(ast: &mut Ast, pc: &mut Context, d: NodeId) {
    let Some(mut list) = pc.link_label_state else {
        return;
    };
    let Some(ds) = label_state(ast, d).cloned() else {
        return;
    };
    let mut list_opt = Some(list);
    match ds.prev {
        None => {
            list_opt = ds.next;
            match ds.next {
                Some(nl) => {
                    list = nl;
                    if let Some(s) = label_state_mut(ast, nl) {
                        s.first = Some(d);
                        s.last = ds.last;
                        s.prev = None;
                    }
                    pc.link_label_state = Some(nl);
                }
                None => pc.link_label_state = None,
            }
        }
        Some(p) => {
            if let Some(ps) = label_state_mut(ast, p) {
                ps.next = ds.next;
            }
            if let Some(nx) = ds.next
                && let Some(ns) = label_state_mut(ast, nx)
            {
                ns.prev = Some(p);
            }
        }
    }
    if list_opt.is_some()
        && ds.next.is_none()
        && let Some(s) = label_state_mut(ast, list)
    {
        s.last = ds.prev;
    }
    if let Some(s) = label_state_mut(ast, d) {
        s.next = None;
        s.prev = None;
        s.first = None;
        s.last = None;
    }
}

fn push_link_bottom(pc: &mut Context) {
    let b = pc.last_delimiter;
    pc.link_bottom = match std::mem::take(&mut pc.link_bottom) {
        LinkBottom::Nil => LinkBottom::One(b),
        LinkBottom::One(x) => LinkBottom::Many(vec![x, b]),
        LinkBottom::Many(mut v) => {
            v.push(b);
            LinkBottom::Many(v)
        }
    };
}

fn pop_link_bottom(pc: &mut Context) -> Bottom {
    match std::mem::take(&mut pc.link_bottom) {
        LinkBottom::Nil => Bottom::Nil,
        LinkBottom::One(x) => Bottom::Node(x),
        LinkBottom::Many(mut v) => {
            let last = v.pop().flatten();
            pc.link_bottom = match v.len() {
                0 => LinkBottom::Nil,
                1 => LinkBottom::One(v[0]),
                _ => LinkBottom::Many(v),
            };
            Bottom::Node(last)
        }
    }
}

fn process_link_label_open(
    ast: &mut Ast,
    block: &mut BlockReader<'_>,
    pos: isize,
    is_image: bool,
    pc: &mut Context,
) -> NodeId {
    let start = if is_image { pos - 1 } else { pos };
    let state = ast.new_node(NodeData::LinkLabelState(LinkLabelState {
        segment: Segment::new(start, pos + 1),
        is_image,
        prev: None,
        next: None,
        first: None,
        last: None,
    }));
    push_link_label_state(ast, pc, state);
    block.advance(1);
    state
}

/// Port of `linkParser.containsLink`, iteratively.
fn contains_link(ast: &Ast, n: NodeId) -> bool {
    let mut stack = vec![n];
    while let Some(start) = stack.pop() {
        let mut c = Some(start);
        while let Some(cn) = c {
            if ast.kind(cn) == NodeKind::Link {
                return true;
            }
            if let Some(fc) = ast.first_child(cn) {
                stack.push(fc);
            }
            c = ast.next_sibling(cn);
        }
    }
    false
}

/// Port of `linkParser.processLinkLabel`.
fn process_link_label(ast: &mut Ast, parent: NodeId, link: NodeId, last: NodeId, pc: &mut Context) {
    let bottom = pop_link_bottom(pc);
    process_delimiters(ast, bottom, pc);
    let mut c = ast.next_sibling(last);
    while let Some(cn) = c {
        let next = ast.next_sibling(cn);
        ast.remove_child(parent, cn);
        ast.append_child(link, cn);
        c = next;
    }
}

fn give_up_label(ast: &mut Ast, pc: &mut Context, last: NodeId) {
    if let (Some(p), Some(s)) = (ast.parent(last), label_state(ast, last).map(|s| s.segment)) {
        ast.merge_or_replace_text_segment(p, last, s);
    }
    pop_link_bottom(pc);
}

fn link_parse(
    ast: &mut Ast,
    parent: NodeId,
    block: &mut BlockReader<'_>,
    pc: &mut Context,
) -> Option<NodeId> {
    let (line, segment) = block.peek_line();
    let line = line?;
    if line[0] == b'!' {
        if line.len() > 1 && line[1] == b'[' {
            block.advance(1);
            push_link_bottom(pc);
            return Some(process_link_label_open(
                ast,
                block,
                segment.start + 1,
                true,
                pc,
            ));
        }
        return None;
    }
    if line[0] == b'[' {
        push_link_bottom(pc);
        return Some(process_link_label_open(
            ast,
            block,
            segment.start,
            false,
            pc,
        ));
    }

    // ']'
    let tlist = pc.link_label_state?;
    let Some(last) = label_state(ast, tlist).and_then(|s| s.last) else {
        pop_link_bottom(pc);
        return None;
    };
    block.advance(1);
    remove_link_label_state(ast, pc, last);
    if link_label_state_length(ast, tlist) > 998 {
        give_up_label(ast, pc, last);
        return None;
    }
    let (last_is_image, last_segment) = label_state(ast, last).map(|s| (s.is_image, s.segment))?;
    if !last_is_image && contains_link(ast, last) {
        give_up_label(ast, pc, last);
        return None;
    }

    let c = block.peek();
    let (l, pos) = block.position();
    let mut link = None;
    match c {
        b'(' => link = parse_link(ast, parent, last, block, pc),
        b'[' => {
            let (lk, has_value) = parse_reference_link(ast, parent, last, block, pc);
            if lk.is_none() && has_value {
                give_up_label(ast, pc, last);
                return None;
            }
            link = lk;
        }
        _ => {}
    }

    let link = match link {
        Some(lk) => lk,
        None => {
            block.set_position(l, pos);
            let ssegment = Segment::new(last_segment.stop, segment.start);
            let maybe_reference = block.value(ssegment);
            if maybe_reference.len() > 999 {
                give_up_label(ast, pc, last);
                return None;
            }
            let Some(r) = pc
                .reference(&util::to_link_reference(&maybe_reference))
                .cloned()
            else {
                give_up_label(ast, pc, last);
                return None;
            };
            let lk = ast.new_node(NodeData::Link {
                destination: Vec::new(),
                title: None,
                reference: None,
            });
            process_link_label(ast, parent, lk, last, pc);
            if let NodeData::Link {
                destination,
                title,
                reference,
            } = ast.data_mut(lk)
            {
                *title = r.title;
                *destination = r.destination;
                *reference = Some(ReferenceLink {
                    typ: ReferenceLinkType::Shortcut,
                    value: maybe_reference,
                });
            }
            lk
        }
    };
    if let Some(p) = ast.parent(last) {
        ast.remove_child(p, last);
    }
    let n = if last_is_image {
        // ast.NewImage: move the link's children and fields to a new Image node.
        let (destination, title, reference) = match ast.data(link) {
            NodeData::Link {
                destination,
                title,
                reference,
            } => (destination.clone(), title.clone(), reference.clone()),
            _ => (Vec::new(), None, None),
        };
        let img = ast.new_node(NodeData::Image {
            destination,
            title,
            reference,
        });
        let mut c = ast.first_child(link);
        while let Some(cn) = c {
            let next = ast.next_sibling(cn);
            ast.remove_child(link, cn);
            ast.append_child(img, cn);
            c = next;
        }
        img
    } else {
        link
    };
    ast.set_pos(n, last_segment.start);
    Some(n)
}

const LINK_FIND_CLOSURE_OPTIONS: FindClosureOptions = FindClosureOptions {
    code_span: false,
    nesting: false,
    newline: true,
    advance: true,
};

fn parse_reference_link(
    ast: &mut Ast,
    parent: NodeId,
    last: NodeId,
    block: &mut BlockReader<'_>,
    pc: &mut Context,
) -> (Option<NodeId>, bool) {
    let (_, orgpos) = block.position();
    block.advance(1);
    let Some(segments) = block.find_closure(b'[', b']', LINK_FIND_CLOSURE_OPTIONS) else {
        return (None, false);
    };
    let mut ref_type = ReferenceLinkType::Full;
    let mut maybe_reference = if segments.len() == 1 {
        block.value(segments.at(0))
    } else {
        let mut v = Vec::new();
        for s in segments.as_slice() {
            v.extend_from_slice(&block.value(*s));
        }
        v
    };
    if util::is_blank(&maybe_reference) {
        let last_stop = label_state(ast, last).map_or(0, |s| s.segment.stop);
        let s = Segment::new(last_stop, orgpos.start - 1);
        maybe_reference = block.value(s);
        ref_type = ReferenceLinkType::Collapsed;
    }
    if maybe_reference.len() > 999 {
        return (None, true);
    }
    let Some(r) = pc
        .reference(&util::to_link_reference(&maybe_reference))
        .cloned()
    else {
        return (None, true);
    };
    let link = ast.new_node(NodeData::Link {
        destination: r.destination,
        title: r.title,
        reference: Some(ReferenceLink {
            typ: ref_type,
            value: maybe_reference,
        }),
    });
    process_link_label(ast, parent, link, last, pc);
    (Some(link), true)
}

fn parse_link(
    ast: &mut Ast,
    parent: NodeId,
    last: NodeId,
    block: &mut BlockReader<'_>,
    pc: &mut Context,
) -> Option<NodeId> {
    block.advance(1);
    block.skip_spaces();
    let mut title = None;
    let mut destination = Vec::new();
    if block.peek() == b')' {
        block.advance(1);
    } else {
        destination = parse_link_destination(block)?;
        block.skip_spaces();
        if block.peek() == b')' {
            block.advance(1);
        } else {
            title = Some(parse_link_title(block)?);
            block.skip_spaces();
            if block.peek() == b')' {
                block.advance(1);
            } else {
                return None;
            }
        }
    }
    let link = ast.new_node(NodeData::Link {
        destination: Vec::new(),
        title: None,
        reference: None,
    });
    process_link_label(ast, parent, link, last, pc);
    if let NodeData::Link {
        destination: d,
        title: t,
        ..
    } = ast.data_mut(link)
    {
        *d = destination;
        *t = title.flatten();
    }
    Some(link)
}

/// Port of `parseLinkDestination`: `None` is Go's `ok == false`.
pub(crate) fn parse_link_destination(block: &mut BlockReader<'_>) -> Option<Vec<u8>> {
    block.skip_spaces();
    let (line, _) = block.peek_line();
    let line: Vec<u8> = line.map(|l| l.into_owned()).unwrap_or_default();
    if block.peek() == b'<' {
        let mut i = 1;
        while i < line.len() {
            let c = line[i];
            if c == b'\\' && i < line.len() - 1 && util::is_punct(line[i + 1]) {
                i += 2;
                continue;
            } else if c == b'>' {
                block.advance(i as isize + 1);
                return Some(line[1..i].to_vec());
            }
            i += 1;
        }
        return None;
    }
    let mut opened = 0;
    let mut i = 0;
    while i < line.len() {
        let c = line[i];
        if c == b'\\' && i < line.len() - 1 && util::is_punct(line[i + 1]) {
            i += 2;
            continue;
        } else if c == b'(' {
            opened += 1;
        } else if c == b')' {
            opened -= 1;
            if opened < 0 {
                break;
            }
        } else if util::is_space(c) {
            break;
        }
        i += 1;
    }
    block.advance(i as isize);
    if i == 0 {
        None
    } else {
        Some(line[..i].to_vec())
    }
}

/// Port of `parseLinkTitle`: `None` is `ok == false`; `Some(None)` is a nil title (multi-line
/// title whose lines were all empty — unreachable, kept for Go's `append(nil, ...)`).
fn parse_link_title(block: &mut BlockReader<'_>) -> Option<Option<Vec<u8>>> {
    block.skip_spaces();
    let opener = block.peek();
    if opener != b'"' && opener != b'\'' && opener != b'(' {
        return None;
    }
    let closer = if opener == b'(' { b')' } else { opener };
    block.advance(1);
    let segments: Segments = block.find_closure(opener, closer, LINK_FIND_CLOSURE_OPTIONS)?;
    if segments.len() == 1 {
        return Some(Some(block.value(segments.at(0))));
    }
    let mut title: Option<Vec<u8>> = None;
    for s in segments.as_slice() {
        let v = block.value(*s);
        if !v.is_empty() || title.is_some() {
            title.get_or_insert_with(Vec::new).extend_from_slice(&v);
        }
    }
    Some(title)
}

/// Port of `linkParser.CloseBlock`: unmatched `[` / `![` become text.
fn link_close_block(ast: &mut Ast, pc: &mut Context) {
    pc.link_bottom = LinkBottom::Nil;
    let mut s = pc.link_label_state;
    while let Some(sn) = s {
        let next = label_state(ast, sn).and_then(|x| x.next);
        remove_link_label_state(ast, pc, sn);
        if let (Some(p), Some(seg)) = (ast.parent(sn), label_state(ast, sn).map(|x| x.segment)) {
            let t = ast.new_text_segment(seg);
            ast.replace_child(p, sn, t);
        }
        s = next;
    }
}

// ---- extension/tasklist.go ----

fn task_check_box_parse(
    ast: &mut Ast,
    parent: NodeId,
    block: &mut BlockReader<'_>,
) -> Option<NodeId> {
    let pp = ast.parent(parent)?;
    if ast.first_child(pp) != Some(parent) {
        return None;
    }
    if ast.has_children(parent) {
        return None;
    }
    if ast.kind(pp) != NodeKind::ListItem {
        return None;
    }
    let (line, _) = block.peek_line();
    let line = line?;
    // `^\[([\sxX])\]\s*` — Go's `\s` is ASCII `[\t\n\f\r ]`.
    let is_re_space = |c: u8| matches!(c, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ');
    if line.len() < 3 || line[0] != b'[' || line[2] != b']' {
        return None;
    }
    let value = line[1];
    if !(is_re_space(value) || value == b'x' || value == b'X') {
        return None;
    }
    let mut end = 3;
    while end < line.len() && is_re_space(line[end]) {
        end += 1;
    }
    block.advance(end as isize);
    Some(ast.new_node(NodeData::TaskCheckBox {
        checked: value == b'x' || value == b'X',
    }))
}

// ---- extension/linkify.go ----

fn is_domain_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-@:%._+~#=".contains(&c)
}

fn is_url_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-@:%_+.~#$!?&/=();,'\">^{}[]`".contains(&c)
}

fn is_www_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-@:%_+.~#!?&/=();,'\">^{}[]`".contains(&c)
}

/// The shared tail of `urlRegexp` and `wwwURLRegxp` after the scheme / `www.`:
/// `[D]{1,256}\.[a-z]+(?::\d+)?(?:[/#?][P]*)?`, under RE2's leftmost-first preference — the
/// greedy `{1,256}` backs off to the last `.` followed by a lowercase letter.
fn linkify_host_path(line: &[u8], p: usize, port: bool, path: fn(u8) -> bool) -> Option<usize> {
    let mut run = 0;
    while p + run < line.len() && run < 257 && is_domain_char(line[p + run]) {
        run += 1;
    }
    let max_k = run.min(256);
    let mut k = max_k;
    while k >= 1 {
        let dot = p + k;
        if dot + 1 < line.len() && line[dot] == b'.' && line[dot + 1].is_ascii_lowercase() {
            let mut end = dot + 1;
            while end < line.len() && line[end].is_ascii_lowercase() {
                end += 1;
            }
            if port && end + 1 < line.len() && line[end] == b':' && line[end + 1].is_ascii_digit() {
                end += 1;
                while end < line.len() && line[end].is_ascii_digit() {
                    end += 1;
                }
            }
            if end < line.len() && matches!(line[end], b'/' | b'#' | b'?') {
                end += 1;
                while end < line.len() && path(line[end]) {
                    end += 1;
                }
            }
            return Some(end);
        }
        k -= 1;
    }
    None
}

/// `urlRegexp.FindSubmatchIndex(line)[1]`, hand-matched.
pub(crate) fn linkify_url_match(line: &[u8]) -> Option<usize> {
    let p = if line.starts_with(b"http://") {
        7
    } else if line.starts_with(b"https://") {
        8
    } else if line.starts_with(b"ftp://") {
        6
    } else {
        return None;
    };
    linkify_host_path(line, p, true, is_url_path_char)
}

/// `wwwURLRegxp.FindSubmatchIndex(line)[1]`, hand-matched.
pub(crate) fn linkify_www_match(line: &[u8]) -> Option<usize> {
    if !line.starts_with(b"www.") {
        return None;
    }
    linkify_host_path(line, 4, false, is_www_path_char)
}

fn linkify_parse(
    ast: &mut Ast,
    parent: NodeId,
    block: &mut BlockReader<'_>,
    pc: &mut Context,
) -> Option<NodeId> {
    if pc.link_label_state.is_some() {
        return None;
    }
    let (line, segment) = block.peek_line();
    let full = line?;
    let mut line: &[u8] = &full;
    let mut consumes: isize = 0;
    let mut start = segment.start;
    let c = line[0];
    if matches!(c, b' ' | b'*' | b'_' | b'~' | b'(') {
        consumes += 1;
        start += 1;
        line = &line[1..];
    }

    let mut m: Option<usize> = None;
    let mut protocol: Option<Vec<u8>> = None;
    let mut typ = AutoLinkType::Url;
    if line.starts_with(b"http:") || line.starts_with(b"https:") || line.starts_with(b"ftp:") {
        m = linkify_url_match(line);
    }
    if m.is_none() && line.starts_with(b"www.") {
        m = linkify_www_match(line);
        protocol = Some(b"http".to_vec());
    }
    if let Some(end) = m.as_mut() {
        let last_char = line[*end - 1];
        if last_char == b'.' {
            *end -= 1;
        } else if last_char == b')' {
            let mut closing: isize = 0;
            for &ch in line[..*end].iter().rev() {
                match ch {
                    b')' => closing += 1,
                    b'(' => closing -= 1,
                    _ => {}
                }
            }
            if closing > 0 {
                *end -= closing as usize;
            }
        } else if last_char == b';' {
            let mut i = *end as isize - 2;
            while i >= 0 {
                if util::is_alpha_numeric(line[i as usize]) {
                    i -= 1;
                    continue;
                }
                break;
            }
            if i != *end as isize - 2 && i >= 0 && line[i as usize] == b'&' {
                *end = i as usize;
            }
        }
    }
    let end = match m {
        Some(e) => e,
        None => {
            if !line.is_empty() && util::is_punct(line[0]) {
                return None;
            }
            typ = AutoLinkType::Email;
            let stop = util::find_email_index(line);
            if stop < 0 {
                return None;
            }
            let stop = stop as usize;
            let at = line.iter().position(|&b| b == b'@').unwrap_or(0);
            let mut end = stop;
            if !line[at..stop - 1].contains(&b'.') {
                return None;
            }
            if line[end - 1] == b'.' {
                end -= 1;
            }
            if end < line.len() {
                let next_char = line[end];
                if next_char == b'-' || next_char == b'_' {
                    return None;
                }
            }
            end
        }
    };
    if consumes != 0 {
        let s = segment.with_stop(segment.start + 1);
        ast.merge_or_append_text_segment(parent, s);
    }
    let mut i = end as isize - 1;
    while i > 0 {
        match line[i as usize] {
            b'?' | b'!' | b'.' | b',' | b':' | b'*' | b'_' | b'~' => i -= 1,
            _ => break,
        }
    }
    i += 1;
    consumes += i;
    block.advance(consumes);
    let value = Segment::new(start, start + i);
    Some(ast.new_node(NodeData::AutoLink {
        typ,
        protocol,
        value,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(line: &[u8], before: char) -> (bool, bool) {
        let d = scan_delimiter(line, before, 1, DelimiterProcessor::Emphasis).unwrap();
        (d.can_open, d.can_close)
    }

    #[test]
    fn flanking_rules() {
        assert_eq!(flags(b"*a", ' '), (true, false));
        assert_eq!(flags(b"* a", 'a'), (false, true));
        assert_eq!(flags(b"*a", 'a'), (true, true));
        assert_eq!(flags(b"_a", 'a'), (false, false));
        assert_eq!(flags(b"_\"", 'a'), (false, true));
        assert_eq!(flags(b"*\"", ' '), (true, false));
        assert_eq!(flags(b"*", '\n'), (false, false));
    }

    #[test]
    fn rule_of_three() {
        let d = |o, c: bool, l| Delimiter {
            segment: Segment::default(),
            can_open: o,
            can_close: c,
            length: l,
            original_length: l,
            ch: b'*',
            previous_delimiter: None,
            next_delimiter: None,
            processor: DelimiterProcessor::Emphasis,
        };
        assert_eq!(calc_consumption(&d(true, true, 1), &d(true, true, 2)), 0);
        assert_eq!(calc_consumption(&d(true, false, 2), &d(false, true, 2)), 2);
        assert_eq!(calc_consumption(&d(true, false, 3), &d(false, true, 3)), 2);
        assert_eq!(calc_consumption(&d(true, false, 1), &d(false, true, 2)), 1);
    }

    #[test]
    fn linkify_regex_ports() {
        assert_eq!(linkify_www_match(b"www.a.bc/x y"), Some(10));
        assert_eq!(linkify_www_match(b"www.a.BC"), None);
        assert_eq!(linkify_www_match(b"www.a.b.C"), Some(7));
        assert_eq!(linkify_url_match(b"http://a.b:80/x y"), Some(15));
        assert_eq!(linkify_url_match(b"https://a.b$c"), Some(11));
        assert_eq!(linkify_url_match(b"https://a.b/$c"), Some(14));
        assert_eq!(linkify_www_match(b"www.a.b/$c"), Some(8));
        assert_eq!(linkify_url_match(b"ftp://x"), None);
        assert_eq!(linkify_url_match(b"http:/x.y"), None);
    }
}
