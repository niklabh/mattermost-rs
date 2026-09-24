//! Port of the parsing half of `extension/table.go`: the paragraph transformer that turns a
//! paragraph with a delimiter row into a table, and the AST transformer that removes the
//! backslash of `\|` inside code spans in cells.

use crate::ast::{Alignment, Ast, NodeData, NodeId, NodeKind, WalkStatus};
use crate::text::Segment;
use crate::util;

use super::{Context, EscapedPipeCell};

/// Port of `isTableDelim`.
fn is_table_delim(bs: &[u8]) -> bool {
    let (w, _) = util::indent_width(bs, 0);
    if w > 3 {
        return false;
    }
    let mut all_sep = true;
    for &b in bs {
        if b != b'-' {
            all_sep = false;
        }
        if !(util::is_space(b) || b == b'-' || b == b'|' || b == b':') {
            return false;
        }
    }
    !all_sep
}

/// Go's `\s` (`[\t\n\f\r ]`). Only `\t`, `\n`, `\r` and space can reach it: `\v` and `\f`
/// fail `util.IsSpace` in [`is_table_delim`] first, so whether `\s` would take them is moot.
fn is_re_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// `tableDelimLeft/Right/Center/None`: `^\s*(:?)-+(:?)\s*$`, as the colon pair.
fn delim_col(col: &[u8]) -> Option<(bool, bool)> {
    let mut i = 0;
    while i < col.len() && is_re_space(col[i]) {
        i += 1;
    }
    let left = col.get(i) == Some(&b':');
    if left {
        i += 1;
    }
    let dash_start = i;
    while i < col.len() && col[i] == b'-' {
        i += 1;
    }
    if i == dash_start {
        return None;
    }
    let right = col.get(i) == Some(&b':');
    if right {
        i += 1;
    }
    while i < col.len() && is_re_space(col[i]) {
        i += 1;
    }
    (i == col.len()).then_some((left, right))
}

/// Port of `tableParagraphTransformer.parseDelimiter`; `None` is Go's nil alignments (which
/// includes a delimiter row with no columns).
fn parse_delimiter(segment: Segment, source: &[u8]) -> Option<Vec<Alignment>> {
    let line = segment.value(source);
    if !is_table_delim(&line) {
        return None;
    }
    let mut cols: Vec<&[u8]> = line.split(|&c| c == b'|').collect();
    if cols.first().is_some_and(|c| util::is_blank(c)) {
        cols.remove(0);
    }
    if cols.last().is_some_and(|c| util::is_blank(c)) {
        cols.pop();
    }
    let mut alignments = Vec::new();
    for col in cols {
        // Go tries left, right, center, none in that order; the four are disjoint.
        alignments.push(match delim_col(col)? {
            (true, false) => Alignment::Left,
            (false, true) => Alignment::Right,
            (true, true) => Alignment::Center,
            (false, false) => Alignment::None,
        });
    }
    (!alignments.is_empty()).then_some(alignments)
}

/// Port of `tableParagraphTransformer.parseRow`.
fn parse_row(
    ast: &mut Ast,
    segment: Segment,
    alignments: &[Alignment],
    is_header: bool,
    source: &[u8],
    pc: &mut Context,
) -> NodeId {
    let npos = segment;
    let segment = segment.trim_left_space(source).trim_right_space(source);
    let line = segment.value(source);
    let mut pos = 0usize;
    let mut limit = line.len();
    let row = ast.new_node(NodeData::TableRow {
        alignments: alignments.to_vec(),
    });
    ast.set_pos(row, npos.start);
    if !line.is_empty() && line[pos] == b'|' {
        pos += 1;
    }
    if !line.is_empty() && line[limit - 1] == b'|' {
        limit -= 1;
    }
    let mut i = 0usize;
    while pos < limit {
        let mut alignment = Alignment::None;
        if i >= alignments.len() {
            if !is_header {
                return row;
            }
        } else {
            alignment = alignments[i];
        }
        let mut escaped_cell: Option<usize> = None;
        let node = ast.new_node(NodeData::TableCell { alignment });
        ast.set_pos(node, npos.start + pos as isize - npos.padding);
        let mut has_backtick = false;
        let mut closure = pos;
        while closure < limit {
            if line[closure] == b'`' {
                has_backtick = true;
            }
            if line[closure] == b'|' {
                if closure == 0 || line[closure - 1] != b'\\' {
                    break;
                } else if has_backtick {
                    let list = pc.escaped_pipe_cells.get_or_insert_with(Vec::new);
                    let idx = *escaped_cell.get_or_insert_with(|| {
                        list.push(EscapedPipeCell {
                            cell: node,
                            pos: Vec::new(),
                            transformed: false,
                        });
                        list.len() - 1
                    });
                    list[idx].pos.push(segment.start + closure as isize - 1);
                }
            }
            closure += 1;
        }
        let seg = Segment::new(
            segment.start + pos as isize,
            segment.start + closure as isize,
        )
        .trim_left_space(source)
        .trim_right_space(source);
        ast.lines_mut(node).append(seg);
        ast.append_child(row, node);
        pos = closure + 1;
        i += 1;
    }
    while i < alignments.len() {
        let c = ast.new_node(NodeData::TableCell {
            alignment: Alignment::None,
        });
        ast.append_child(row, c);
        i += 1;
    }
    row
}

/// Port of `tableParagraphTransformer.Transform`.
pub(crate) fn transform_paragraph(ast: &mut Ast, node: NodeId, source: &[u8], pc: &mut Context) {
    let ppos = ast.pos(node);
    if ast.lines(node).len() < 2 {
        return;
    }
    let mut i = 1;
    while i < ast.lines(node).len() {
        let Some(alignments) = parse_delimiter(ast.lines(node).at(i), source) else {
            i += 1;
            continue;
        };
        let header_seg = ast.lines(node).at(i - 1);
        let header = parse_row(ast, header_seg, &alignments, true, source, pc);
        if alignments.len() as isize != ast.child_count(header) {
            return;
        }
        let table = ast.new_node(NodeData::Table {
            alignments: alignments.clone(),
        });
        ast.set_pos(table, ppos);
        // extension/ast.NewTableHeader: position of the row, children moved over.
        let th = ast.new_node(NodeData::TableHeader {
            alignments: Vec::new(),
        });
        let hp = ast.pos(header);
        ast.set_pos(th, hp);
        let mut c = ast.first_child(header);
        while let Some(cn) = c {
            let next = ast.next_sibling(cn);
            ast.append_child(th, cn);
            c = next;
        }
        ast.append_child(table, th);
        let mut j = i + 1;
        while j < ast.lines(node).len() {
            let seg = ast.lines(node).at(j);
            let row = parse_row(ast, seg, &alignments, false, source, pc);
            ast.append_child(table, row);
            j += 1;
        }
        ast.lines_mut(node).set_sliced(0, i - 1);
        if let Some(p) = ast.parent(node) {
            ast.insert_after(p, node, table);
            if ast.lines(node).is_empty() {
                ast.remove_child(p, node);
            } else {
                let mut last = ast.lines(node).at(i - 2);
                last.stop -= 1;
                ast.lines_mut(node).set(i - 2, last);
            }
        }
        i += 1;
    }
}

/// Port of `tableASTTransformer.Transform`: split the raw text of code spans in escaped-pipe
/// cells at each recorded backslash, dropping it.
pub(crate) fn transform_ast(ast: &mut Ast, pc: &mut Context) {
    let Some(mut lst) = pc.escaped_pipe_cells.take() else {
        return;
    };
    for vi in 0..lst.len() {
        if lst[vi].transformed {
            continue;
        }
        let cell = lst[vi].cell;
        let _ = ast.walk::<()>(cell, |ast, n, entering| {
            if !entering || ast.kind(n) != NodeKind::CodeSpan {
                return Ok(WalkStatus::Continue);
            }
            let mut c = ast.first_child(n);
            while let Some(cn) = c {
                let next = ast.next_sibling(cn);
                let Some(ts) = ast.text_segment(cn) else {
                    c = next;
                    continue;
                };
                let Some(parent) = ast.parent(cn) else {
                    c = next;
                    continue;
                };
                let mut cur = cn;
                for v in lst.iter_mut() {
                    for &pos in &v.pos {
                        if ts.start <= pos && pos < ts.stop {
                            let Some(segment) = ast.text_segment(cur) else {
                                continue;
                            };
                            let n1 = ast.new_raw_text_segment(segment.with_stop(pos));
                            let n2 = ast.new_raw_text_segment(segment.with_start(pos + 1));
                            ast.insert_after(parent, cur, n1);
                            ast.insert_after(parent, n1, n2);
                            ast.remove_child(parent, cur);
                            cur = n2;
                            v.transformed = true;
                        }
                    }
                }
                c = next;
            }
            Ok(WalkStatus::Continue)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiter_rows() {
        let src = b"|:--|--:|:-:|---|";
        let got = parse_delimiter(Segment::new(0, src.len() as isize), src);
        assert_eq!(
            got,
            Some(vec![
                Alignment::Left,
                Alignment::Right,
                Alignment::Center,
                Alignment::None
            ])
        );
        let src = b"|";
        assert_eq!(parse_delimiter(Segment::new(0, 1), src), None);
        let src = b"---";
        assert_eq!(parse_delimiter(Segment::new(0, 3), src), None);
        let src = b"- \x0b-|--";
        assert_eq!(
            parse_delimiter(Segment::new(0, src.len() as isize), src),
            None
        );
        let src = b"    --|--";
        assert_eq!(
            parse_delimiter(Segment::new(0, src.len() as isize), src),
            None
        );
    }
}
