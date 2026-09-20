//! Go's reader semantics, for the decoders: `io.Reader`/`io.ByteReader` as `(n, err)` pairs,
//! `bytes.Reader`, `bufio.Reader`, `io.ReadFull`, and the error values whose *text* the decoders
//! surface.
//!
//! Why this is not `std::io::Read`: Go's decoders are chains of readers, and *how much* each
//! layer pulls from the one below is observable. The PNG decoder reads IDAT data through a
//! `bufio.Reader` that zlib puts in front of it; how far that buffer reads ahead decides whether
//! trailing bytes in an IDAT chunk are "too much pixel data" or silently swallowed. So every layer
//! is a port of the Go type it stands for, with Go's short-read behaviour, and errors travel with
//! the byte count exactly as Go returns them.

/// Every error value the decode chain can produce, rendered with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `io.EOF`.
    #[error("EOF")]
    Eof,
    /// `io.ErrUnexpectedEOF`.
    #[error("unexpected EOF")]
    UnexpectedEof,
    /// `io.ErrNoProgress`.
    #[error("multiple Read calls return no data or error")]
    NoProgress,
    /// `flate.CorruptInputError` (compress/flate/inflate.go:32).
    #[error("flate: corrupt input before offset {0}")]
    FlateCorrupt(i64),
    /// `flate.InternalError` (compress/flate/inflate.go:40).
    #[error("flate: internal error: {0}")]
    FlateInternal(&'static str),
    /// `zlib.ErrChecksum`.
    #[error("zlib: invalid checksum")]
    ZlibChecksum,
    /// `zlib.ErrDictionary`.
    #[error("zlib: invalid dictionary")]
    ZlibDictionary,
    /// `zlib.ErrHeader`.
    #[error("zlib: invalid header")]
    ZlibHeader,
    /// `png.FormatError` (image/png/reader.go:127).
    #[error("png: invalid format: {0}")]
    PngFormat(String),
    /// `png.UnsupportedError` (image/png/reader.go:134).
    #[error("png: unsupported feature: {0}")]
    PngUnsupported(String),
}

/// `io.Reader`: `Read(p) (n int, err error)`. The error may accompany data.
pub trait Read {
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>);
}

/// `io.ByteReader` on top of `io.Reader` — `flate.Reader`.
pub trait ByteRead: Read {
    fn read_byte(&mut self) -> Result<u8, Error>;
}

impl<R: Read + ?Sized> Read for &mut R {
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        (**self).read(p)
    }
}

impl<R: ByteRead + ?Sized> ByteRead for &mut R {
    fn read_byte(&mut self) -> Result<u8, Error> {
        (**self).read_byte()
    }
}

/// `bytes.Reader` over a slice.
#[derive(Debug)]
pub struct BytesReader<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> BytesReader<'a> {
    pub fn new(s: &'a [u8]) -> Self {
        BytesReader { s, i: 0 }
    }
}

impl Read for BytesReader<'_> {
    /// `bytes.Reader.Read`: `(0, EOF)` at the end even for an empty `p`.
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        if self.i >= self.s.len() {
            return (0, Some(Error::Eof));
        }
        let n = p.len().min(self.s.len() - self.i);
        p[..n].copy_from_slice(&self.s[self.i..self.i + n]);
        self.i += n;
        (n, None)
    }
}

impl ByteRead for BytesReader<'_> {
    fn read_byte(&mut self) -> Result<u8, Error> {
        let b = *self.s.get(self.i).ok_or(Error::Eof)?;
        self.i += 1;
        Ok(b)
    }
}

/// `bufio.defaultBufSize`.
pub const DEFAULT_BUF_SIZE: usize = 4096;
/// `bufio.maxConsecutiveEmptyReads`.
const MAX_CONSECUTIVE_EMPTY_READS: usize = 100;

/// Port of `bufio.Reader` (bufio/bufio.go) — the read paths only.
#[derive(Debug)]
pub struct BufReader<R> {
    buf: Vec<u8>,
    rd: R,
    r: usize,
    w: usize,
    err: Option<Error>,
}

impl<R: Read> BufReader<R> {
    /// `bufio.NewReader`: a 4096-byte buffer.
    pub fn new(rd: R) -> Self {
        BufReader {
            buf: vec![0; DEFAULT_BUF_SIZE],
            rd,
            r: 0,
            w: 0,
            err: None,
        }
    }

    /// The wrapped reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.rd
    }

    fn read_err(&mut self) -> Option<Error> {
        self.err.take()
    }

    /// `Reader.fill` (bufio/bufio.go).
    fn fill(&mut self) {
        if self.r > 0 {
            self.buf.copy_within(self.r..self.w, 0);
            self.w -= self.r;
            self.r = 0;
        }
        if self.w >= self.buf.len() {
            // Go panics "bufio: tried to fill full buffer"; every caller fills an empty buffer.
            return;
        }
        for _ in 0..MAX_CONSECUTIVE_EMPTY_READS {
            let (n, err) = self.rd.read(&mut self.buf[self.w..]);
            self.w += n;
            if let Some(e) = err {
                self.err = Some(e);
                return;
            }
            if n > 0 {
                return;
            }
        }
        self.err = Some(Error::NoProgress);
    }
}

impl<R: Read> Read for BufReader<R> {
    /// `Reader.Read` (bufio/bufio.go): at most one read of the underlying reader per call.
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        if p.is_empty() {
            if self.w > self.r {
                return (0, None);
            }
            return (0, self.read_err());
        }
        if self.r == self.w {
            if self.err.is_some() {
                return (0, self.read_err());
            }
            if p.len() >= self.buf.len() {
                let (n, err) = self.rd.read(p);
                self.err = err;
                return (n, self.read_err());
            }
            self.r = 0;
            self.w = 0;
            let (n, err) = self.rd.read(&mut self.buf);
            self.err = err;
            if n == 0 {
                return (0, self.read_err());
            }
            self.w += n;
        }
        let n = p.len().min(self.w - self.r);
        p[..n].copy_from_slice(&self.buf[self.r..self.r + n]);
        self.r += n;
        (n, None)
    }
}

impl<R: Read> ByteRead for BufReader<R> {
    /// `Reader.ReadByte`.
    fn read_byte(&mut self) -> Result<u8, Error> {
        while self.r == self.w {
            if self.err.is_some() {
                return Err(self.read_err().unwrap_or(Error::Eof));
            }
            self.fill();
        }
        let c = self.buf[self.r];
        self.r += 1;
        Ok(c)
    }
}

/// `io.ReadFull` (`io.ReadAtLeast(r, buf, len(buf))`).
pub fn read_full<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> (usize, Option<Error>) {
    let min = buf.len();
    let mut n = 0;
    let mut err = None;
    while n < min && err.is_none() {
        let (nn, e) = r.read(&mut buf[n..]);
        n += nn;
        err = e;
    }
    if n >= min {
        err = None;
    } else if n > 0 && err == Some(Error::Eof) {
        err = Some(Error::UnexpectedEof);
    }
    (n, err)
}

/// `io.ReadFull` for callers that only need the error.
pub fn read_full_err<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> Result<(), Error> {
    match read_full(r, buf) {
        (_, None) => Ok(()),
        (_, Some(e)) => Err(e),
    }
}

/// `io.ReadAll`: everything up to the first error; `io.EOF` is success.
pub fn read_all<R: Read + ?Sized>(r: &mut R) -> (Vec<u8>, Option<Error>) {
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 512];
    loop {
        let (n, err) = r.read(&mut chunk);
        out.extend_from_slice(&chunk[..n]);
        match err {
            None => {}
            Some(Error::Eof) => return (out, None),
            Some(e) => return (out, Some(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out fixed-size pieces, to see bufio's short reads.
    struct Pieces<'a>(&'a [u8], usize);

    impl Read for Pieces<'_> {
        fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
            if self.0.is_empty() {
                return (0, Some(Error::Eof));
            }
            let n = p.len().min(self.1).min(self.0.len());
            p[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            (n, None)
        }
    }

    #[test]
    fn bufio_read_is_one_underlying_read() {
        let data = [7u8; 100];
        let mut b = BufReader::new(Pieces(&data, 30));
        let mut p = [0u8; 50];
        assert_eq!(b.read(&mut p), (30, None));
        assert_eq!(b.read(&mut p), (30, None));
        assert_eq!(read_full(&mut b, &mut p), (40, Some(Error::UnexpectedEof)));
        assert_eq!(b.read(&mut p), (0, Some(Error::Eof)));
    }

    #[test]
    fn read_full_zero_length_does_not_read() {
        let mut r = BytesReader::new(&[]);
        assert_eq!(read_full(&mut r, &mut []), (0, None));
        assert_eq!(read_full(&mut r, &mut [0]), (0, Some(Error::Eof)));
    }

    #[test]
    fn bufio_large_read_bypasses_the_buffer_and_readbyte_fills() {
        let data: Vec<u8> = (0..10000u32).map(|i| i as u8).collect();
        let mut b = BufReader::new(BytesReader::new(&data));
        assert_eq!(b.read_byte(), Ok(0));
        let mut big = vec![0u8; 5000];
        // Buffer holds 4095 bytes: a read copies from it.
        assert_eq!(b.read(&mut big).0, 4095);
        assert_eq!(b.read(&mut big).0, 5000);
        assert_eq!(big[0], 4096u32 as u8);
    }
}
