//! Port of `bufio.Writer` (bufio/bufio.go): the buffer whose flush boundaries become the PNG
//! encoder's IDAT chunk boundaries.

use crate::sink::Sink;

/// Port of `bufio.Writer`. Writes cannot fail (see [`crate::sink`]), so Go's sticky error and
/// `io.ErrShortWrite` have nothing to hold.
pub struct Writer<W: Sink> {
    buf: Vec<u8>,
    n: usize,
    wr: W,
}

impl<W: Sink> Writer<W> {
    /// `bufio.NewWriterSize(w, size)`. Go substitutes 4096 for a size <= 0; a `usize` of 0 does
    /// the same here.
    pub fn with_capacity(size: usize, wr: W) -> Self {
        let size = if size == 0 { 4096 } else { size };
        Writer {
            buf: vec![0; size],
            n: 0,
            wr,
        }
    }

    /// `Writer.Available`.
    pub fn available(&self) -> usize {
        self.buf.len() - self.n
    }

    /// `Writer.Buffered`.
    pub fn buffered(&self) -> usize {
        self.n
    }

    /// Port of `Writer.Flush` (bufio.go:635).
    pub fn flush(&mut self) {
        if self.n == 0 {
            return;
        }
        self.wr.write(&self.buf[..self.n]);
        self.n = 0;
    }

    /// The underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.wr
    }

    /// Consume the buffer, returning the underlying writer. Does not flush.
    pub fn into_inner(self) -> W {
        self.wr
    }
}

impl<W: Sink> Sink for Writer<W> {
    /// Port of `Writer.Write` (bufio.go:676): a write larger than the free space goes straight
    /// through when nothing is buffered, otherwise it tops the buffer up and flushes.
    fn write(&mut self, mut p: &[u8]) {
        while p.len() > self.available() {
            let n = if self.buffered() == 0 {
                self.wr.write(p);
                p.len()
            } else {
                let n = self.available();
                self.buf[self.n..self.n + n].copy_from_slice(&p[..n]);
                self.n += n;
                self.flush();
                n
            };
            p = &p[n..];
        }
        self.buf[self.n..self.n + p.len()].copy_from_slice(p);
        self.n += p.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each Write call separately.
    #[derive(Default)]
    struct Calls(Vec<Vec<u8>>);
    impl Sink for Calls {
        fn write(&mut self, p: &[u8]) {
            self.0.push(p.to_vec());
        }
    }

    #[test]
    fn small_writes_fill_then_flush_whole_buffers() {
        let mut w = Writer::with_capacity(4, Calls::default());
        w.write(b"ab");
        w.write(b"cde");
        w.write(b"f");
        w.flush();
        assert_eq!(w.into_inner().0, vec![b"abcd".to_vec(), b"ef".to_vec()]);
    }

    #[test]
    fn a_large_write_into_an_empty_buffer_goes_straight_through() {
        let mut w = Writer::with_capacity(4, Calls::default());
        w.write(b"0123456789");
        w.write(b"xy");
        w.flush();
        assert_eq!(
            w.into_inner().0,
            vec![b"0123456789".to_vec(), b"xy".to_vec()]
        );
    }

    #[test]
    fn exactly_the_free_space_is_buffered_not_flushed() {
        let mut w = Writer::with_capacity(4, Calls::default());
        w.write(b"abcd");
        assert_eq!(w.buffered(), 4);
        assert!(w.get_mut().0.is_empty());
    }
}
