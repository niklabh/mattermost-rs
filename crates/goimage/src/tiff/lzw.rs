//! Port of `golang.org/x/image/tiff/lzw` (tiff/lzw/reader.go): the LZW variant TIFF files use.
//!
//! This is **not** `compress/lzw`. That package's reader was branched to make this one, and the
//! branch kept one deliberate difference: TIFF (following Aldus, not the LZW paper) widens the
//! code one code earlier than real LZW does. The whole difference is the `+1` in
//! [`Reader::decode`]'s `hi + 1 >= overflow` — get it wrong and a stream longer than 254 codes
//! decodes to rubbish, while every shorter stream still passes.
//!
//! `tiff.Decode` only ever constructs `NewReader(r, MSB, 8)`, but the LSB order and the argument
//! checks are ported too because they decide what a caller that asks for them gets.

use crate::goread::{ByteRead, Error as IoError};

/// `lzw.Order`: the bit ordering of a code stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// `lzw.LSB` — least significant bits first, as GIF uses.
    Lsb,
    /// `lzw.MSB` — most significant bits first, as TIFF and PDF use.
    Msb,
}

/// Everything this reader can fail with, rendered with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// An error from the source reader, `io.EOF` included.
    #[error("{0}")]
    Io(#[from] IoError),
    /// `errors.New("lzw: invalid code")`.
    #[error("lzw: invalid code")]
    InvalidCode,
    /// `errClosed`.
    #[error("lzw: reader/writer is closed")]
    Closed,
    /// `errors.New("lzw: unknown order")`.
    #[error("lzw: unknown order")]
    UnknownOrder,
    /// `fmt.Errorf("lzw: litWidth %d out of range", litWidth)`.
    #[error("lzw: litWidth {0} out of range")]
    LitWidth(i64),
}

impl Error {
    /// Whether this is the `io.EOF` that ends a stream, which `io.ReadAll` and
    /// `bytes.Buffer.ReadFrom` treat as success.
    pub fn is_eof(&self) -> bool {
        *self == Error::Io(IoError::Eof)
    }
}

const MAX_WIDTH: u32 = 12;
const DECODER_INVALID_CODE: u16 = 0xffff;
const FLUSH_BUFFER: usize = 1 << MAX_WIDTH;
/// `1 << maxWidth`, the size of the `suffix`/`prefix` tables.
const TABLE_SIZE: usize = 1 << MAX_WIDTH;

/// Port of `lzw.decoder` (reader.go:62), as an `io.ReadCloser` over `r`.
///
/// Go's `NewReader` puts a `bufio.Reader` in front of a source without `ReadByte`; here the
/// caller does that, so the read pattern below the decoder is the caller's to match.
pub struct Reader<R> {
    r: R,
    bits: u32,
    n_bits: u32,
    width: u32,
    order: Order,
    lit_width: u32,
    err: Option<Error>,

    clear: u16,
    eof: u16,
    hi: u16,
    overflow: u16,
    last: u16,

    suffix: Box<[u8; TABLE_SIZE]>,
    prefix: Box<[u16; TABLE_SIZE]>,

    /// `output`, the 2×`1<<maxWidth` scratch buffer.
    output: Box<[u8; 2 * TABLE_SIZE]>,
    o: usize,
    /// `toRead`, as a range into `output`.
    to_read: std::ops::Range<usize>,
}

impl<R: ByteRead> Reader<R> {
    /// Port of `lzw.NewReader` (reader.go:246). Go reports a bad `order` or `litWidth` from the
    /// first `Read` rather than from the constructor, and so does this.
    pub fn new(r: R, order: Order, lit_width: i64) -> Self {
        let mut d = Reader {
            r,
            bits: 0,
            n_bits: 0,
            width: 0,
            order,
            lit_width: 0,
            err: None,
            clear: 0,
            eof: 0,
            hi: 0,
            overflow: 0,
            last: DECODER_INVALID_CODE,
            suffix: Box::new([0; TABLE_SIZE]),
            prefix: Box::new([0; TABLE_SIZE]),
            output: Box::new([0; 2 * TABLE_SIZE]),
            o: 0,
            to_read: 0..0,
        };
        // Go's `default: d.err = errors.New("lzw: unknown order")` arm switches on an `int`, which
        // can hold a third value; [`Order`] cannot, so [`Error::UnknownOrder`] is unreachable here
        // and exists only to carry Go's text for a caller that renders it.
        if !(2..=8).contains(&lit_width) {
            d.err = Some(Error::LitWidth(lit_width));
            return d;
        }
        d.lit_width = lit_width as u32;
        d.width = 1 + d.lit_width;
        d.clear = 1u16 << d.lit_width;
        d.eof = d.clear + 1;
        d.hi = d.clear + 1;
        d.overflow = 1u16 << d.width;
        d
    }

    /// `readLSB` (reader.go:100).
    fn read_lsb(&mut self) -> Result<u16, IoError> {
        while self.n_bits < self.width {
            let x = self.r.read_byte()?;
            self.bits |= u32::from(x) << self.n_bits;
            self.n_bits += 8;
        }
        let code = (self.bits & ((1 << self.width) - 1)) as u16;
        self.bits >>= self.width;
        self.n_bits -= self.width;
        Ok(code)
    }

    /// `readMSB` (reader.go:115).
    fn read_msb(&mut self) -> Result<u16, IoError> {
        while self.n_bits < self.width {
            let x = self.r.read_byte()?;
            self.bits |= u32::from(x) << (24 - self.n_bits);
            self.n_bits += 8;
        }
        let code = (self.bits >> (32 - self.width)) as u16;
        self.bits <<= self.width;
        self.n_bits -= self.width;
        Ok(code)
    }

    fn read_code(&mut self) -> Result<u16, IoError> {
        match self.order {
            Order::Lsb => self.read_lsb(),
            Order::Msb => self.read_msb(),
        }
    }

    /// Port of `decoder.Read` (reader.go:130).
    pub fn read(&mut self, b: &mut [u8]) -> (usize, Option<Error>) {
        loop {
            if !self.to_read.is_empty() {
                let n = b.len().min(self.to_read.len());
                b[..n].copy_from_slice(&self.output[self.to_read.start..self.to_read.start + n]);
                self.to_read.start += n;
                return (n, None);
            }
            if let Some(e) = &self.err {
                return (0, Some(e.clone()));
            }
            self.decode();
        }
    }

    /// Port of `decoder.Close` (reader.go:232).
    pub fn close(&mut self) {
        self.err = Some(Error::Closed);
    }

    /// Port of `decoder.decode` (reader.go:145): codes in, decompressed bytes into `output`.
    fn decode(&mut self) {
        loop {
            let code = match self.read_code() {
                Ok(c) => c,
                Err(e) => {
                    // A truncated code stream is `io.ErrUnexpectedEOF`, never `io.EOF`: only the
                    // EOF *code* ends a stream cleanly.
                    self.err = Some(Error::Io(if e == IoError::Eof {
                        IoError::UnexpectedEof
                    } else {
                        e
                    }));
                    break;
                }
            };
            if code < self.clear {
                // A literal code.
                self.output[self.o] = code as u8;
                self.o += 1;
                if self.last != DECODER_INVALID_CODE {
                    self.suffix[usize::from(self.hi)] = code as u8;
                    self.prefix[usize::from(self.hi)] = self.last;
                }
            } else if code == self.clear {
                self.width = 1 + self.lit_width;
                self.hi = self.eof;
                self.overflow = 1u16 << self.width;
                self.last = DECODER_INVALID_CODE;
                continue;
            } else if code == self.eof {
                self.err = Some(Error::Io(IoError::Eof));
                break;
            } else if code <= self.hi {
                let mut c = code;
                let mut i = self.output.len() - 1;
                if code == self.hi && self.last != DECODER_INVALID_CODE {
                    // `code == hi` expands to the last expansion followed by its own head, so
                    // walk the prefix chain down to a literal to find that head.
                    c = self.last;
                    while c >= self.clear {
                        c = self.prefix[usize::from(c)];
                    }
                    self.output[i] = c as u8;
                    i -= 1;
                    c = self.last;
                }
                while c >= self.clear {
                    self.output[i] = self.suffix[usize::from(c)];
                    i -= 1;
                    c = self.prefix[usize::from(c)];
                }
                self.output[i] = c as u8;
                // `d.o += copy(d.output[d.o:], d.output[i:])`: `o < 1<<maxWidth <= i`, so the
                // ranges can overlap and Go's `copy` is a memmove.
                let n = self.output.len() - i;
                self.output.copy_within(i.., self.o);
                self.o += n;
                if self.last != DECODER_INVALID_CODE {
                    self.suffix[usize::from(self.hi)] = c as u8;
                    self.prefix[usize::from(self.hi)] = self.last;
                }
            } else {
                self.err = Some(Error::InvalidCode);
                break;
            }
            self.last = code;
            // Go's `uint16` wraps; past `maxWidth` `hi` runs on without ever being used as an
            // index, because `last` is invalidated on every pass below.
            self.hi = self.hi.wrapping_add(1);
            // NOTE: the "+1" is where TIFF's LZW differs from the standard algorithm.
            if self.hi.wrapping_add(1) >= self.overflow {
                if self.width == MAX_WIDTH {
                    self.last = DECODER_INVALID_CODE;
                } else {
                    self.width += 1;
                    self.overflow <<= 1;
                }
            }
            if self.o >= FLUSH_BUFFER {
                break;
            }
        }
        self.to_read = 0..self.o;
        self.o = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goread::BytesReader;

    /// Read a whole stream the way `io.ReadAll` would.
    fn all(data: &[u8], order: Order, lit_width: i64) -> (Vec<u8>, Option<Error>) {
        let mut d = Reader::new(BytesReader::new(data), order, lit_width);
        let mut out = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let (n, err) = d.read(&mut chunk);
            out.extend_from_slice(&chunk[..n]);
            match err {
                None => {}
                Some(e) if e.is_eof() => return (out, None),
                Some(e) => return (out, Some(e)),
            }
        }
    }

    /// An MSB-first bit writer, so the tests can spell a code stream out by hand.
    #[derive(Default)]
    struct BitWriter {
        out: Vec<u8>,
        acc: u32,
        n: u32,
    }

    impl BitWriter {
        fn put(&mut self, code: u16, width: u32) {
            self.acc = (self.acc << width) | u32::from(code);
            self.n += width;
            while self.n >= 8 {
                self.n -= 8;
                self.out.push((self.acc >> self.n) as u8);
            }
        }
        fn done(mut self) -> Vec<u8> {
            if self.n > 0 {
                self.out.push((self.acc << (8 - self.n)) as u8);
            }
            self.out
        }
    }

    #[test]
    fn literals_and_a_back_reference_decode() {
        let mut w = BitWriter::default();
        // clear, 'A', 'B', code 258 ("AB"), EOF.
        for c in [256u16, b'A' as u16, b'B' as u16, 258, 257] {
            w.put(c, 9);
        }
        assert_eq!(all(&w.done(), Order::Msb, 8), (b"ABAB".to_vec(), None));
    }

    /// The `code == hi` special case: a code that names the entry being created right now.
    #[test]
    fn the_kwkwk_case_expands_to_the_last_expansion_plus_its_head() {
        let mut w = BitWriter::default();
        // 'A', then 258 = the entry about to be defined as "AA".
        for c in [256u16, b'A' as u16, 258, 257] {
            w.put(c, 9);
        }
        assert_eq!(all(&w.done(), Order::Msb, 8), (b"AAA".to_vec(), None));
    }

    /// TIFF widens one code before real LZW does: the 254th code after a clear is already ten
    /// bits wide. A reader written against `compress/lzw` reads it as nine and desynchronises.
    #[test]
    fn the_code_width_grows_one_code_early() {
        let mut w = BitWriter::default();
        w.put(256, 9);
        // 254 literal codes at nine bits; after the 254th, hi == 511 and the width becomes ten.
        for i in 0..254u16 {
            w.put(i % 256, 9);
        }
        w.put(b'Z' as u16, 10);
        w.put(257, 10);
        let (out, err) = all(&w.done(), Order::Msb, 8);
        assert_eq!(err, None);
        assert_eq!(out.len(), 255);
        assert_eq!(out[254], b'Z');
        // Standard LZW would still be at nine bits here, and would read a different code.
        let mut w = BitWriter::default();
        w.put(256, 9);
        for i in 0..254u16 {
            w.put(i % 256, 9);
        }
        w.put(b'Z' as u16, 9);
        w.put(257, 9);
        assert_ne!(all(&w.done(), Order::Msb, 8), (out, None));
    }

    #[test]
    fn a_code_past_hi_is_invalid_and_a_truncated_stream_is_unexpected_eof() {
        let mut w = BitWriter::default();
        w.put(256, 9);
        w.put(b'A' as u16, 9);
        w.put(300, 9);
        assert_eq!(all(&w.done(), Order::Msb, 8).1, Some(Error::InvalidCode));

        let mut w = BitWriter::default();
        w.put(256, 9);
        w.put(b'A' as u16, 9);
        let mut s = w.done();
        s.truncate(2);
        assert_eq!(
            all(&s, Order::Msb, 8).1,
            Some(Error::Io(IoError::UnexpectedEof))
        );
        // An empty stream is a truncated one too, not a clean EOF.
        assert_eq!(
            all(&[], Order::Msb, 8).1,
            Some(Error::Io(IoError::UnexpectedEof))
        );
    }

    #[test]
    fn a_clear_code_resets_the_table_and_the_width() {
        let mut w = BitWriter::default();
        w.put(256, 9);
        for i in 0..254u16 {
            w.put(i % 256, 9);
        }
        // Width is ten now; a clear takes it back to nine.
        w.put(256, 10);
        w.put(b'Q' as u16, 9);
        w.put(257, 9);
        let (out, err) = all(&w.done(), Order::Msb, 8);
        assert_eq!(err, None);
        assert_eq!(out.len(), 255);
        assert_eq!(out[254], b'Q');
    }

    #[test]
    fn a_bad_lit_width_is_reported_from_the_first_read() {
        let mut d = Reader::new(BytesReader::new(&[0u8; 4]), Order::Msb, 9);
        assert_eq!(d.read(&mut [0u8; 4]).1, Some(Error::LitWidth(9)));
        let mut d = Reader::new(BytesReader::new(&[0u8; 4]), Order::Msb, 1);
        assert_eq!(d.read(&mut [0u8; 4]).1, Some(Error::LitWidth(1)));
        // Go's third `Order` value has no Rust spelling, so only its text is checked.
        assert_eq!(Error::UnknownOrder.to_string(), "lzw: unknown order");
    }

    #[test]
    fn close_makes_every_later_read_fail() {
        let mut w = BitWriter::default();
        for c in [256u16, b'A' as u16, 257] {
            w.put(c, 9);
        }
        let s = w.done();
        let mut d = Reader::new(BytesReader::new(&s), Order::Msb, 8);
        d.close();
        assert_eq!(d.read(&mut [0u8; 4]), (0, Some(Error::Closed)));
    }

    /// LSB order is the same algorithm with the bits the other way up.
    #[test]
    fn lsb_order_reads_the_same_codes_bit_reversed() {
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut n = 0u32;
        for c in [256u16, b'A' as u16, b'B' as u16, 258, 257] {
            acc |= u32::from(c) << n;
            n += 9;
            while n >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                n -= 8;
            }
        }
        if n > 0 {
            out.push(acc as u8);
        }
        assert_eq!(all(&out, Order::Lsb, 8), (b"ABAB".to_vec(), None));
    }
}
