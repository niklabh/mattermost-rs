//! Port of `compress/flate`'s `dictDecoder` (compress/flate/dict_decoder.go): the LZ77 sliding
//! window the decompressor writes into and flushes out of.

/// `dictDecoder`.
#[derive(Debug, Default)]
pub(crate) struct DictDecoder {
    /// Sliding window history.
    pub(crate) hist: Vec<u8>,
    /// Current output position in buffer.
    wr_pos: usize,
    /// Have emitted `hist[..rd_pos]` already.
    rd_pos: usize,
    /// Has a full window length been written yet?
    full: bool,
}

impl DictDecoder {
    /// `init` (dict_decoder.go:39).
    pub(crate) fn init(&mut self, size: usize, dict: Option<&[u8]>) {
        let mut hist = std::mem::take(&mut self.hist);
        hist.resize(size, 0);
        *self = DictDecoder {
            hist,
            ..DictDecoder::default()
        };
        let mut dict = dict.unwrap_or(&[]);
        if dict.len() > self.hist.len() {
            dict = &dict[dict.len() - self.hist.len()..];
        }
        self.hist[..dict.len()].copy_from_slice(dict);
        self.wr_pos = dict.len();
        if self.wr_pos == self.hist.len() {
            self.wr_pos = 0;
            self.full = true;
        }
        self.rd_pos = self.wr_pos;
    }

    /// `histSize`: the total amount of historical data in the dictionary.
    pub(crate) fn hist_size(&self) -> usize {
        if self.full {
            return self.hist.len();
        }
        self.wr_pos
    }

    /// `availRead`: the bytes that can be flushed by `read_flush`.
    pub(crate) fn avail_read(&self) -> usize {
        self.wr_pos - self.rd_pos
    }

    /// `availWrite`: the space left in the output buffer.
    pub(crate) fn avail_write(&self) -> usize {
        self.hist.len() - self.wr_pos
    }

    /// `writeSlice`'s range.
    pub(crate) fn write_range(&self) -> std::ops::Range<usize> {
        self.wr_pos..self.hist.len()
    }

    /// `writeMark`.
    pub(crate) fn write_mark(&mut self, cnt: usize) {
        self.wr_pos += cnt;
    }

    /// `writeByte`. The caller has checked `avail_write() > 0`.
    pub(crate) fn write_byte(&mut self, c: u8) {
        if let Some(slot) = self.hist.get_mut(self.wr_pos) {
            *slot = c;
        }
        self.wr_pos += 1;
    }

    /// `writeCopy` (dict_decoder.go:104): copies `length` bytes from `dist` back, possibly
    /// wrapping around the window, and returns how many were copied.
    pub(crate) fn write_copy(&mut self, dist: usize, length: usize) -> usize {
        let dst_base = self.wr_pos;
        let mut dst_pos = dst_base;
        let mut src_pos = dst_pos as isize - dist as isize;
        let end_pos = (dst_pos + length).min(self.hist.len());

        if src_pos < 0 {
            let src = (src_pos + self.hist.len() as isize) as usize;
            let n = (end_pos - dst_pos).min(self.hist.len() - src);
            self.hist.copy_within(src..src + n, dst_pos);
            dst_pos += n;
            src_pos = 0;
        }
        let src_pos = src_pos as usize;
        while dst_pos < end_pos {
            // copy(hist[dstPos:endPos], hist[srcPos:dstPos])
            let n = (end_pos - dst_pos).min(dst_pos - src_pos);
            self.hist.copy_within(src_pos..src_pos + n, dst_pos);
            dst_pos += n;
        }
        self.wr_pos = dst_pos;
        dst_pos - dst_base
    }

    /// `tryWriteCopy` (dict_decoder.go:137): the fast path when no wrap is involved.
    pub(crate) fn try_write_copy(&mut self, dist: usize, length: usize) -> usize {
        let mut dst_pos = self.wr_pos;
        let end_pos = dst_pos + length;
        if dst_pos < dist || end_pos > self.hist.len() {
            return 0;
        }
        let dst_base = dst_pos;
        let src_pos = dst_pos - dist;
        while dst_pos < end_pos {
            let n = (end_pos - dst_pos).min(dst_pos - src_pos);
            self.hist.copy_within(src_pos..src_pos + n, dst_pos);
            dst_pos += n;
        }
        self.wr_pos = dst_pos;
        dst_pos - dst_base
    }

    /// `readFlush`: the range of `hist` ready to be emitted. The range stays valid until the next
    /// write, which is all Go's returned slice promises too.
    pub(crate) fn read_flush(&mut self) -> std::ops::Range<usize> {
        let to_read = self.rd_pos..self.wr_pos;
        self.rd_pos = self.wr_pos;
        if self.wr_pos == self.hist.len() {
            self.wr_pos = 0;
            self.rd_pos = 0;
            self.full = true;
        }
        to_read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_overlapping_and_wrapping() {
        let mut d = DictDecoder::default();
        d.init(8, None);
        for c in b"abc" {
            d.write_byte(*c);
        }
        assert_eq!(d.try_write_copy(3, 4), 4);
        assert_eq!(&d.hist[..7], b"abcabca");
        // Fill to the end, flush, and copy across the wrap.
        d.write_byte(b'z');
        assert_eq!(d.avail_write(), 0);
        assert_eq!(d.read_flush(), 0..8);
        assert_eq!(d.try_write_copy(2, 3), 0);
        assert_eq!(d.write_copy(2, 3), 3);
        assert_eq!(&d.hist[..3], b"aza");
        assert_eq!(d.hist_size(), 8);
    }

    #[test]
    fn init_with_a_dictionary_longer_than_the_window() {
        let mut d = DictDecoder::default();
        d.init(4, Some(b"123456"));
        assert_eq!(&d.hist, b"3456");
        assert_eq!((d.wr_pos, d.full), (0, true));
    }
}
