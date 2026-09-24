//! Port of goldmark's `text` package (`text/segment.go`, `text/reader.go`).
//!
//! Positions are `isize` because Go's are `int` and use `-1` as a sentinel
//! (`invalidValue`, `HTMLBlock.ClosureLine`), and several computations briefly go negative.
//!
//! Go has one `Reader` interface with two implementations. Here they are two concrete types:
//! [`Reader`] walks the whole source line by line (the block phase), [`BlockReader`] walks the
//! lines of one block (the inline phase and link reference definitions). No goldmark code path
//! calls a method on the "other" reader, so nothing is lost by not sharing a trait.

use std::borrow::Cow;

use crate::util;

/// `text.EOF` — what `Peek` answers past the end.
pub const EOF: u8 = 0xff;

const INVALID_VALUE: isize = -1;

/// Port of `text.Segment` (segment.go:13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    /// Start position of the segment.
    pub start: isize,
    /// Stop position of the segment (exclusive).
    pub stop: isize,
    /// Number of leading spaces the segment stands for (tab expansion).
    pub padding: isize,
    /// Append a `\n` to the value if it does not already end with one.
    pub force_newline: bool,
}

impl Segment {
    /// Port of `text.NewSegment`.
    pub fn new(start: isize, stop: isize) -> Self {
        Segment {
            start,
            stop,
            padding: 0,
            force_newline: false,
        }
    }

    /// Port of `text.NewSegmentPadding`.
    pub fn new_padding(start: isize, stop: isize, padding: isize) -> Self {
        Segment {
            start,
            stop,
            padding,
            force_newline: false,
        }
    }

    /// Port of `Segment.Value`: padding spaces, then the bytes, then the forced newline.
    pub fn value<'a>(&self, buffer: &'a [u8]) -> Cow<'a, [u8]> {
        let body = slice(buffer, self.start, self.stop);
        let mut result: Cow<'a, [u8]> = if self.padding == 0 {
            Cow::Borrowed(body)
        } else {
            let mut v = Vec::with_capacity(self.padding.max(0) as usize + body.len() + 1);
            v.resize(self.padding.max(0) as usize, b' ');
            v.extend_from_slice(body);
            Cow::Owned(v)
        };
        if self.force_newline && result.last().is_some_and(|&c| c != b'\n') {
            result.to_mut().push(b'\n');
        }
        result
    }

    /// Port of `Segment.Len`.
    pub fn len(&self) -> isize {
        self.stop - self.start + self.padding
    }

    /// Port of `Segment.Between`. Go panics when the stops differ; this returns the same
    /// arithmetic regardless (no goldmark caller passes differing stops).
    pub fn between(&self, other: Segment) -> Segment {
        Segment::new_padding(self.start, other.start, self.padding - other.padding)
    }

    /// Port of `Segment.IsEmpty`.
    pub fn is_empty(&self) -> bool {
        self.start >= self.stop && self.padding == 0
    }

    /// Port of `Segment.TrimRightSpace` — note it drops the padding when everything is space.
    pub fn trim_right_space(&self, buffer: &[u8]) -> Segment {
        let v = slice(buffer, self.start, self.stop);
        let l = util::trim_right_space_length(v) as isize;
        if l == v.len() as isize {
            return Segment::new(self.start, self.start);
        }
        Segment::new_padding(self.start, self.stop - l, self.padding)
    }

    /// Port of `Segment.TrimLeftSpace` — note it drops the padding.
    pub fn trim_left_space(&self, buffer: &[u8]) -> Segment {
        let v = slice(buffer, self.start, self.stop);
        let l = util::trim_left_space_length(v) as isize;
        Segment::new(self.start + l, self.stop)
    }

    /// Port of `Segment.TrimLeftSpaceWidth`.
    pub fn trim_left_space_width(&self, mut width: isize, buffer: &[u8]) -> Segment {
        let mut padding = self.padding;
        while width > 0 {
            if padding == 0 {
                break;
            }
            padding -= 1;
            width -= 1;
        }
        if width == 0 {
            return Segment::new_padding(self.start, self.stop, padding);
        }
        let text = slice(buffer, self.start, self.stop);
        let mut start = self.start;
        for &c in text {
            if start >= self.stop - 1 || width <= 0 {
                break;
            }
            if c == b' ' {
                width -= 1;
            } else if c == b'\t' {
                width -= 4;
            } else {
                break;
            }
            start += 1;
        }
        if width < 0 {
            padding = -width;
        }
        Segment::new_padding(start, self.stop, padding)
    }

    /// Port of `Segment.WithStart` (keeps padding, drops `ForceNewline`).
    pub fn with_start(&self, v: isize) -> Segment {
        Segment::new_padding(v, self.stop, self.padding)
    }

    /// Port of `Segment.WithStop` (keeps padding, drops `ForceNewline`).
    pub fn with_stop(&self, v: isize) -> Segment {
        Segment::new_padding(self.start, v, self.padding)
    }

    /// Port of `Segment.ConcatPadding`.
    pub fn concat_padding(&self, v: &mut Vec<u8>) {
        if self.padding > 0 {
            v.extend(std::iter::repeat_n(b' ', self.padding as usize));
        }
    }
}

/// `buffer[start:stop]` with the bounds clamped; Go would panic where this clamps, and no
/// goldmark path reaches that.
pub(crate) fn slice(buffer: &[u8], start: isize, stop: isize) -> &[u8] {
    let len = buffer.len() as isize;
    let s = start.clamp(0, len) as usize;
    let e = stop.clamp(0, len) as usize;
    if s >= e { &[] } else { &buffer[s..e] }
}

/// Port of `text.Segments` (segment.go:170).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Segments {
    values: Vec<Segment>,
}

impl Segments {
    /// Port of `text.NewSegments`.
    pub fn new() -> Self {
        Segments::default()
    }
    /// Port of `Segments.Append`.
    pub fn append(&mut self, t: Segment) {
        self.values.push(t);
    }
    /// Port of `Segments.AppendAll`.
    pub fn append_all(&mut self, t: &[Segment]) {
        self.values.extend_from_slice(t);
    }
    /// Port of `Segments.Len`.
    pub fn len(&self) -> usize {
        self.values.len()
    }
    /// Whether there are no segments.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    /// Port of `Segments.At`.
    pub fn at(&self, i: usize) -> Segment {
        self.values[i]
    }
    /// Port of `Segments.Set`.
    pub fn set(&mut self, i: usize, v: Segment) {
        self.values[i] = v;
    }
    /// Port of `Segments.SetSliced`.
    pub fn set_sliced(&mut self, lo: usize, hi: usize) {
        self.values.truncate(hi);
        self.values.drain(..lo);
    }
    /// Port of `Segments.Sliced`.
    pub fn sliced(&self, lo: usize, hi: usize) -> &[Segment] {
        &self.values[lo..hi]
    }
    /// Port of `Segments.Clear`.
    pub fn clear(&mut self) {
        self.values.clear();
    }
    /// Port of `Segments.Unshift`.
    pub fn unshift(&mut self, v: Segment) {
        self.values.insert(0, v);
    }
    /// Port of `Segments.Value`.
    pub fn value(&self, buffer: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for v in &self.values {
            out.extend_from_slice(&v.value(buffer));
        }
        out
    }
    /// The segments as a slice.
    pub fn as_slice(&self) -> &[Segment] {
        &self.values
    }
}

/// Port of the options of `text.FindClosureOptions`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FindClosureOptions {
    pub code_span: bool,
    pub nesting: bool,
    pub newline: bool,
    pub advance: bool,
}

/// Port of `text.reader` (reader.go:110): walks the whole source line by line.
///
/// Go caches the peeked line in `peekedLine` and — faithfully reproduced here — neither
/// `SetPosition` nor `SetPadding` clears that cache; `Advance`'s fast path and `AdvanceToEOL`
/// read its length. The cache is kept as the segment it was computed from.
pub(crate) struct Reader<'a> {
    source: &'a [u8],
    source_length: isize,
    line: isize,
    peeked: Option<Segment>,
    pos: Segment,
    head: isize,
    line_offset: isize,
}

impl<'a> Reader<'a> {
    /// Port of `text.NewReader`.
    pub fn new(source: &'a [u8]) -> Self {
        let mut r = Reader {
            source,
            source_length: source.len() as isize,
            line: 0,
            peeked: None,
            pos: Segment::default(),
            head: 0,
            line_offset: -1,
        };
        r.reset_position();
        r
    }

    fn reset_position(&mut self) {
        self.line = -1;
        self.head = 0;
        self.line_offset = -1;
        self.advance_line();
    }

    pub fn source(&self) -> &'a [u8] {
        self.source
    }

    fn peeked_len(&self) -> isize {
        self.peeked
            .map_or(0, |s| s.value(self.source).len() as isize)
    }

    /// Port of `reader.PeekLine`. `None` stands for Go's nil line.
    pub fn peek_line(&mut self) -> (Option<Cow<'a, [u8]>>, Segment) {
        if self.pos.start >= 0 && self.pos.start < self.source_length {
            if self.peeked.is_none() {
                self.peeked = Some(self.pos);
            }
            let cached = self.peeked.unwrap_or(self.pos);
            return (Some(cached.value(self.source)), self.pos);
        }
        (None, self.pos)
    }

    /// Port of `reader.LineOffset`.
    pub fn line_offset(&mut self) -> isize {
        if self.line_offset < 0 {
            let mut v: isize = 0;
            let mut i = self.head;
            while i < self.pos.start {
                if self.source[i as usize] == b'\t' {
                    v += util::tab_width(v);
                } else {
                    v += 1;
                }
                i += 1;
            }
            self.line_offset = v - self.pos.padding;
        }
        self.line_offset
    }

    /// Port of `reader.Advance`.
    pub fn advance(&mut self, mut n: isize) {
        self.line_offset = -1;
        if n < self.peeked_len() && self.pos.padding == 0 {
            self.pos.start += n;
            self.peeked = None;
            return;
        }
        self.peeked = None;
        let l = self.source_length;
        while n > 0 && self.pos.start < l {
            if self.pos.padding != 0 {
                self.pos.padding -= 1;
                n -= 1;
                continue;
            }
            if self.source[self.pos.start as usize] == b'\n' {
                self.advance_line();
                n -= 1;
                continue;
            }
            self.pos.start += 1;
            n -= 1;
        }
    }

    /// Port of `reader.AdvanceAndSetPadding`.
    pub fn advance_and_set_padding(&mut self, n: isize, padding: isize) {
        self.advance(n);
        if padding > self.pos.padding {
            self.set_padding(padding);
        }
    }

    /// Port of `reader.AdvanceToEOL`.
    pub fn advance_to_eol(&mut self) {
        if self.pos.start >= self.source_length {
            return;
        }
        self.line_offset = -1;
        let mut i: isize = -1;
        if self.peeked.is_some() {
            self.pos.start += self.peeked_len() - self.pos.padding - 1;
            if self.source.get(self.pos.start as usize) == Some(&b'\n') {
                i = 0;
            }
        }
        if i == -1 {
            let rest = slice(self.source, self.pos.start, self.source_length);
            i = rest
                .iter()
                .position(|&c| c == b'\n')
                .map_or(-1, |p| p as isize);
        }
        self.peeked = None;
        if i != -1 {
            self.pos.start += i;
        } else {
            self.pos.start = self.source_length;
        }
        self.pos.padding = 0;
    }

    /// Port of `reader.AdvanceLine`.
    pub fn advance_line(&mut self) {
        self.line_offset = -1;
        self.peeked = None;
        self.pos.start = self.pos.stop;
        self.head = self.pos.start;
        if self.pos.start < 0 || self.pos.start >= self.source_length {
            return;
        }
        self.pos.stop = self.source_length;
        let mut i: isize = 0;
        if self.source[self.pos.start as usize] != b'\n' {
            i = slice(self.source, self.pos.start, self.source_length)
                .iter()
                .position(|&c| c == b'\n')
                .map_or(-1, |p| p as isize);
        }
        if i != -1 {
            self.pos.stop = self.pos.start + i + 1;
        }
        self.line += 1;
        self.pos.padding = 0;
    }

    /// Port of `reader.Position`.
    pub fn position(&self) -> (isize, Segment) {
        (self.line, self.pos)
    }

    /// Port of `reader.SetPosition` (does not clear the peeked-line cache, as in Go).
    pub fn set_position(&mut self, line: isize, pos: Segment) {
        self.line_offset = -1;
        self.line = line;
        self.pos = pos;
    }

    /// Port of `reader.SetPadding`.
    pub fn set_padding(&mut self, v: isize) {
        self.pos.padding = v;
    }

    /// Port of `skipBlankLinesReader`.
    pub fn skip_blank_lines(&mut self) -> (Segment, isize, bool) {
        let mut lines = 0;
        loop {
            let (line, seg) = self.peek_line();
            let Some(line) = line else {
                return (seg, lines, false);
            };
            if util::is_blank(&line) {
                lines += 1;
                self.advance_line();
            } else {
                return (seg, lines, true);
            }
        }
    }
}

/// Port of `text.blockReader` (reader.go:375): walks the lines of one block.
pub struct BlockReader<'a> {
    source: &'a [u8],
    segments: Vec<Segment>,
    line: isize,
    pos: Segment,
    head: isize,
    last: isize,
    line_offset: isize,
}

impl<'a> BlockReader<'a> {
    /// Port of `text.NewBlockReader`.
    pub(crate) fn new(source: &'a [u8], segments: &[Segment]) -> Self {
        let mut r = BlockReader {
            source,
            segments: Vec::new(),
            line: 0,
            pos: Segment::default(),
            head: 0,
            last: 0,
            line_offset: -1,
        };
        r.reset(segments);
        r
    }

    fn segments_length(&self) -> isize {
        self.segments.len() as isize
    }

    fn reset_position(&mut self) {
        self.line = -1;
        self.head = 0;
        self.last = 0;
        self.line_offset = -1;
        self.pos.start = -1;
        self.pos.stop = -1;
        self.pos.padding = 0;
        if let Some(last) = self.segments.last() {
            self.last = last.stop;
        }
        self.advance_line();
    }

    /// Port of `blockReader.Reset`.
    pub(crate) fn reset(&mut self, segments: &[Segment]) {
        self.segments = segments.to_vec();
        self.reset_position();
    }

    /// The source buffer.
    pub fn source(&self) -> &'a [u8] {
        self.source
    }

    /// Port of `blockReader.Value`: the bytes of `seg` as seen through this block's lines
    /// (paddings included).
    pub fn value(&self, seg: Segment) -> Vec<u8> {
        let n = self.segments_length();
        let mut line = n - 1;
        let mut ret = Vec::with_capacity((seg.stop - seg.start + 1).max(0) as usize);
        while line >= 0 {
            if seg.start >= self.segments[line as usize].start {
                break;
            }
            line -= 1;
        }
        // Go indexes At(-1) and panics here; clamp instead.
        let mut line = line.max(0);
        let mut i = seg.start;
        while line < n {
            let s = self.segments[line as usize];
            if i < 0 {
                i = s.start;
            }
            s.concat_padding(&mut ret);
            while i < seg.stop && i < s.stop {
                ret.push(self.source[i as usize]);
                i += 1;
            }
            i = -1;
            if s.stop > seg.stop {
                break;
            }
            line += 1;
        }
        ret
    }

    /// Port of `readRuneReader`: `None` is Go's `io.EOF` (end of block, an invalid byte, or a
    /// literal U+FFFD — Go cannot tell the last two apart).
    pub(crate) fn read_rune(&mut self) -> Option<(char, isize)> {
        let (line, _) = self.peek_line();
        let line = line?;
        let (r, size) = util::decode_rune(&line);
        if r == '\u{FFFD}' {
            return None;
        }
        self.advance(size as isize);
        Some((r, size as isize))
    }

    /// Port of `blockReader.PrecendingCharacter`.
    pub fn precending_character(&self) -> char {
        if self.pos.padding != 0 {
            return ' ';
        }
        let Some(first) = self.segments.first() else {
            return '\n';
        };
        if self.line == 0 && self.pos.start <= first.start {
            return '\n';
        }
        let l = self.source.len() as isize;
        let mut i = self.pos.start - 1;
        while i < l && i >= 0 {
            if util::rune_start(self.source[i as usize]) {
                break;
            }
            i -= 1;
        }
        if i < 0 || i >= l {
            return '\n';
        }
        util::decode_rune(&self.source[i as usize..]).0
    }

    /// Port of `blockReader.LineOffset`.
    pub fn line_offset(&mut self) -> isize {
        if self.line_offset < 0 {
            let mut v: isize = 0;
            let mut i = self.head;
            while i < self.pos.start {
                if self.source[i as usize] == b'\t' {
                    v += util::tab_width(v);
                } else {
                    v += 1;
                }
                i += 1;
            }
            self.line_offset = v - self.pos.padding;
        }
        self.line_offset
    }

    /// Port of `blockReader.Peek`.
    pub fn peek(&self) -> u8 {
        if self.line < self.segments_length() && self.pos.start >= 0 && self.pos.start < self.last {
            if self.pos.padding != 0 {
                return b' ';
            }
            return self.source[self.pos.start as usize];
        }
        EOF
    }

    /// Port of `blockReader.PeekLine`. `None` stands for Go's nil line.
    pub fn peek_line(&self) -> (Option<Cow<'a, [u8]>>, Segment) {
        if self.line < self.segments_length() && self.pos.start >= 0 && self.pos.start < self.last {
            return (Some(self.pos.value(self.source)), self.pos);
        }
        (None, self.pos)
    }

    /// Port of `blockReader.Advance`.
    pub fn advance(&mut self, mut n: isize) {
        self.line_offset = -1;
        if n < self.pos.stop - self.pos.start && self.pos.padding == 0 {
            self.pos.start += n;
            return;
        }
        while n > 0 {
            if self.pos.padding != 0 {
                self.pos.padding -= 1;
                n -= 1;
                continue;
            }
            if self.pos.start >= self.pos.stop - 1 && self.pos.stop < self.last {
                self.advance_line();
                n -= 1;
                continue;
            }
            self.pos.start += 1;
            n -= 1;
        }
    }

    /// Port of `blockReader.AdvanceAndSetPadding`.
    pub fn advance_and_set_padding(&mut self, n: isize, padding: isize) {
        self.advance(n);
        if padding > self.pos.padding {
            self.set_padding(padding);
        }
    }

    /// Port of `blockReader.AdvanceLine`.
    pub fn advance_line(&mut self) {
        self.set_position(self.line + 1, Segment::new(INVALID_VALUE, INVALID_VALUE));
        self.head = self.pos.start;
    }

    /// Port of `blockReader.Position`.
    pub fn position(&self) -> (isize, Segment) {
        (self.line, self.pos)
    }

    /// Port of `blockReader.SetPosition`.
    pub fn set_position(&mut self, line: isize, pos: Segment) {
        self.line_offset = -1;
        self.line = line;
        if pos.start == INVALID_VALUE {
            if self.line < self.segments_length() {
                let s = self.segments[line as usize];
                self.head = s.start;
                self.pos = s;
            }
        } else {
            self.pos = pos;
            if self.line < self.segments_length() {
                let s = self.segments[line as usize];
                self.head = s.start;
            }
        }
    }

    /// Port of `blockReader.SetPadding`.
    pub fn set_padding(&mut self, v: isize) {
        self.line_offset = -1;
        self.pos.padding = v;
    }

    /// Port of `skipSpacesReader`.
    pub fn skip_spaces(&mut self) -> (Segment, isize, bool) {
        let mut chars = 0;
        loop {
            let (line, segment) = self.peek_line();
            let Some(line) = line else {
                return (segment, chars, false);
            };
            for (i, &c) in line.iter().enumerate() {
                if util::is_space(c) {
                    chars += 1;
                    self.advance(1);
                    continue;
                }
                return (
                    segment.with_start(segment.start + i as isize + 1),
                    chars,
                    true,
                );
            }
            // Every byte of the line was a space; the loop re-peeks the (advanced) line.
        }
    }

    /// Port of `findClosureReader` (reader.go:604).
    pub(crate) fn find_closure(
        &mut self,
        opener: u8,
        closer: u8,
        opts: FindClosureOptions,
    ) -> Option<Segments> {
        let mut opened = 1;
        let mut code_span_opener = 0;
        let mut closed = false;
        let (orgline, orgpos) = self.position();
        let mut ret: Option<Segments> = None;

        'outer: loop {
            let (bs, seg) = self.peek_line();
            let Some(bs) = bs else {
                break 'outer;
            };
            let bs: &[u8] = &bs;
            let mut i: isize = 0;
            let len = bs.len() as isize;
            while i < len {
                let c = bs[i as usize];
                if opts.code_span && code_span_opener != 0 && c == b'`' {
                    let mut code_span_closer = 0;
                    while i < len {
                        if bs[i as usize] == b'`' {
                            code_span_closer += 1;
                        } else {
                            i -= 1;
                            break;
                        }
                        i += 1;
                    }
                    if code_span_closer == code_span_opener {
                        code_span_opener = 0;
                    }
                } else if code_span_opener == 0
                    && c == b'\\'
                    && i < len - 1
                    && util::is_punct(bs[(i + 1) as usize])
                {
                    i += 2;
                    continue;
                } else if opts.code_span && code_span_opener == 0 && c == b'`' {
                    while i < len {
                        if bs[i as usize] == b'`' {
                            code_span_opener += 1;
                        } else {
                            i -= 1;
                            break;
                        }
                        i += 1;
                    }
                } else if (opts.code_span && code_span_opener == 0) || !opts.code_span {
                    if c == closer {
                        opened -= 1;
                        if opened == 0 {
                            ret.get_or_insert_with(Segments::new)
                                .append(seg.with_stop(seg.start + i));
                            self.advance(i + 1);
                            closed = true;
                            break 'outer;
                        }
                    } else if c == opener {
                        if !opts.nesting {
                            break 'outer;
                        }
                        opened += 1;
                    }
                }
                i += 1;
            }
            if !opts.newline {
                break 'outer;
            }
            self.advance_line();
            ret.get_or_insert_with(Segments::new).append(seg);
        }
        if !opts.advance {
            self.set_position(orgline, orgpos);
        }
        if closed { ret } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_value_padding_and_force_newline() {
        let src = b"abc\ndef";
        let mut s = Segment::new_padding(4, 7, 2);
        assert_eq!(&*s.value(src), b"  def");
        s.force_newline = true;
        assert_eq!(&*s.value(src), b"  def\n");
        let e = Segment {
            force_newline: true,
            ..Segment::new(1, 1)
        };
        // An empty value stays empty even when a newline is forced.
        assert_eq!(&*e.value(src), b"");
    }

    #[test]
    fn segment_trims() {
        let src = b"  ab  \n";
        let s = Segment::new_padding(0, 7, 3);
        assert_eq!(s.trim_left_space(src), Segment::new(2, 7));
        assert_eq!(s.trim_right_space(src), Segment::new_padding(0, 4, 3));
        let blank = Segment::new_padding(0, 2, 1);
        assert_eq!(blank.trim_right_space(src), Segment::new(0, 0));
        assert!(Segment::new(3, 3).is_empty());
        assert!(!Segment::new_padding(3, 3, 1).is_empty());
    }

    #[test]
    fn trim_left_space_width_consumes_padding_then_tabs() {
        let src = b"\t\tx\n";
        let s = Segment::new_padding(0, 4, 2);
        assert_eq!(
            s.trim_left_space_width(1, src),
            Segment::new_padding(0, 4, 1)
        );
        assert_eq!(
            s.trim_left_space_width(3, src),
            Segment::new_padding(1, 4, 3)
        );
    }

    #[test]
    fn reader_walks_lines() {
        let src = b"ab\n\ncd";
        let mut r = Reader::new(src);
        let (l, s) = r.peek_line();
        assert_eq!(l.as_deref(), Some(&b"ab\n"[..]));
        assert_eq!(s, Segment::new(0, 3));
        r.advance_line();
        assert_eq!(r.peek_line().0.as_deref(), Some(&b"\n"[..]));
        r.advance_line();
        assert_eq!(r.peek_line().0.as_deref(), Some(&b"cd"[..]));
        r.advance_line();
        assert!(r.peek_line().0.is_none());
    }

    #[test]
    fn block_reader_value_spans_lines() {
        let src = b"> ab\n> cd\n";
        let segs = [Segment::new(2, 5), Segment::new_padding(7, 10, 1)];
        let r = BlockReader::new(src, &segs);
        assert_eq!(r.value(Segment::new(3, 9)), b"b\n cd".to_vec());
    }

    #[test]
    fn find_closure_honours_escapes_and_nesting() {
        let src = b"a\\]b]c";
        let segs = [Segment::new(0, 6)];
        let mut r = BlockReader::new(src, &segs);
        let opts = FindClosureOptions {
            newline: true,
            advance: true,
            ..Default::default()
        };
        let got = r.find_closure(b'[', b']', opts);
        assert_eq!(
            got.map(|s| s.as_slice().to_vec()),
            Some(vec![Segment::new(0, 4)])
        );
        assert_eq!(r.peek(), b'c');
        let mut r = BlockReader::new(b"a[b]", &[Segment::new(0, 4)]);
        assert!(r.find_closure(b'[', b']', opts).is_none());
    }
}
