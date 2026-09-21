//! Port of `compress/lzw`'s decompressor (compress/lzw/reader.go, go1.26.4) in the form
//! `image/gif` uses it: `lzw.LSB` order and a `litWidth` in [2, 8].
//!
//! Only the reader is here. The MSB order (TIFF, PDF) is not ported because nothing in this crate
//! asks for it, and `Reset`/`NewReader`'s `bufio` fallback are not ported because the GIF decoder
//! always hands it a `blockReader`, which is already an `io.ByteReader`.
//!
//! # Why the pacing matters
//!
//! The GIF decoder reads exactly `len(Pix)` bytes out of this reader and then reads *one more* to
//! decide between "too much image data" and a clean end. Which error that second read produces —
//! `io.EOF` from an explicit end code, `io.ErrUnexpectedEOF` from a stream that simply stopped, or
//! a byte — is the whole difference between a GIF that decodes and one that does not, so `decode`
//! keeps Go's flush boundary (`o >= 1<<12`) and its sticky error rather than decoding eagerly.

use crate::goread::{ByteRead, Error as IoError};

/// `maxWidth`.
const MAX_WIDTH: u32 = 12;
/// `decoderInvalidCode`.
const DECODER_INVALID_CODE: u16 = 0xffff;
/// `flushBuffer`.
const FLUSH_BUFFER: usize = 1 << MAX_WIDTH;
/// `len(Reader.output)`.
const OUTPUT_LEN: usize = 2 << MAX_WIDTH;

/// Every error `compress/lzw`'s reader can produce, with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Whatever the underlying `io.ByteReader` returned, including `io.EOF` for the end code.
    #[error(transparent)]
    Io(#[from] IoError),
    /// `errors.New("lzw: invalid code")` (reader.go:198).
    #[error("lzw: invalid code")]
    InvalidCode,
    /// `errClosed` (reader.go:226).
    #[error("lzw: reader/writer is closed")]
    Closed,
    /// `fmt.Errorf("lzw: litWidth %d out of range", litWidth)` (reader.go:275). Unreachable from
    /// the GIF decoder, which rejects the same range first with its own message.
    #[error("lzw: litWidth {0} out of range")]
    LitWidth(i64),
}

/// `io.EOF`, as `Reader.decode` and the GIF decoder compare against it.
pub const EOF: Error = Error::Io(IoError::Eof);
/// `io.ErrUnexpectedEOF`.
pub const UNEXPECTED_EOF: Error = Error::Io(IoError::UnexpectedEof);

/// Port of `lzw.Reader` (reader.go:47) for `LSB` order.
pub struct Reader<R: ByteRead> {
    r: R,
    bits: u32,
    n_bits: u32,
    width: u32,
    lit_width: u32,
    err: Option<Error>,

    clear: u16,
    eof: u16,
    hi: u16,
    overflow: u16,
    last: u16,

    suffix: [u8; 1 << MAX_WIDTH],
    prefix: [u16; 1 << MAX_WIDTH],

    output: [u8; OUTPUT_LEN],
    o: usize,
    /// `toRead` as a range into `output`; `decode` refills `output` only once it is drained.
    to_read: (usize, usize),
}

impl<R: ByteRead> Reader<R> {
    /// Port of `newReader` + `Reader.init` (reader.go:258) for `lzw.LSB`.
    pub fn new(r: R, lit_width: i64) -> Reader<R> {
        let mut z = Reader {
            r,
            bits: 0,
            n_bits: 0,
            width: 0,
            lit_width: 0,
            err: None,
            clear: 0,
            eof: 0,
            hi: 0,
            overflow: 0,
            last: DECODER_INVALID_CODE,
            suffix: [0; 1 << MAX_WIDTH],
            prefix: [0; 1 << MAX_WIDTH],
            output: [0; OUTPUT_LEN],
            o: 0,
            to_read: (0, 0),
        };
        if !(2..=8).contains(&lit_width) {
            z.err = Some(Error::LitWidth(lit_width));
            return z;
        }
        let lw = lit_width as u32;
        z.lit_width = lw;
        z.width = 1 + lw;
        z.clear = 1u16 << lw;
        z.eof = z.clear + 1;
        z.hi = z.clear + 1;
        z.overflow = 1u16 << z.width;
        z
    }

    /// The underlying reader — `image/gif` keeps its own handle on the `blockReader` in Go.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.r
    }

    /// Port of `Reader.Close` (reader.go:230): every later read fails with `errClosed`.
    pub fn close(&mut self) {
        self.err = Some(Error::Closed);
    }

    /// Port of `readLSB` (reader.go:90).
    fn read_lsb(&mut self) -> Result<u16, IoError> {
        while self.n_bits < self.width {
            let x = self.r.read_byte()?;
            self.bits |= u32::from(x) << self.n_bits;
            self.n_bits += 8;
        }
        let code = (self.bits & ((1u32 << self.width) - 1)) as u16;
        self.bits >>= self.width;
        self.n_bits -= self.width;
        Ok(code)
    }

    /// Port of `Reader.Read` (reader.go:122).
    pub fn read(&mut self, b: &mut [u8]) -> (usize, Option<Error>) {
        loop {
            let (lo, hi) = self.to_read;
            if lo < hi {
                let n = b.len().min(hi - lo);
                b[..n].copy_from_slice(&self.output[lo..lo + n]);
                self.to_read = (lo + n, hi);
                return (n, None);
            }
            if let Some(e) = self.err.clone() {
                return (0, Some(e));
            }
            self.decode();
        }
    }

    /// Port of `Reader.decode` (reader.go:139).
    fn decode(&mut self) {
        loop {
            let code = match self.read_lsb() {
                Ok(c) => c,
                Err(e) => {
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
                if let Some(slot) = self.output.get_mut(self.o) {
                    *slot = code as u8;
                }
                self.o += 1;
                if self.last != DECODER_INVALID_CODE {
                    // Save what the hi code expands to.
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
                self.err = Some(EOF);
                break;
            } else if code <= self.hi {
                let mut c = code;
                let mut i = OUTPUT_LEN - 1;
                if code == self.hi && self.last != DECODER_INVALID_CODE {
                    // code == hi expands to the last expansion followed by its own head; walk the
                    // prefix chain down to a literal to find that head.
                    c = self.last;
                    while c >= self.clear && i > 0 {
                        c = self.prefix[usize::from(c)];
                    }
                    self.output[i] = c as u8;
                    i -= 1;
                    c = self.last;
                }
                // `prefix[c] < c` holds by construction, so the chain is at most 4095 long and `i`
                // cannot walk past the halfway point of `output`; the `i > 0` guards say so
                // without trusting it.
                while c >= self.clear && i > 0 {
                    self.output[i] = self.suffix[usize::from(c)];
                    i -= 1;
                    c = self.prefix[usize::from(c)];
                }
                self.output[i] = c as u8;
                // `r.o += copy(r.output[r.o:], r.output[i:])`.
                let n = (OUTPUT_LEN - i).min(OUTPUT_LEN - self.o);
                self.output.copy_within(i..i + n, self.o);
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
            self.hi += 1;
            if self.hi >= self.overflow {
                if self.width == MAX_WIDTH {
                    self.last = DECODER_INVALID_CODE;
                    // Undo the hi++ above, keeping the invariant hi < overflow.
                    self.hi -= 1;
                } else {
                    self.width += 1;
                    self.overflow = 1u16 << self.width;
                }
            }
            if self.o >= FLUSH_BUFFER {
                break;
            }
        }
        // Flush pending output.
        self.to_read = (0, self.o);
        self.o = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goread::BytesReader;

    /// A `bytes.Reader` is an `io.ByteReader`, which is what `lzw.NewReader` wants.
    fn decode_all(lit_width: i64, data: &[u8]) -> (Vec<u8>, Option<Error>) {
        let mut r = Reader::new(BytesReader::new(data), lit_width);
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let (n, err) = r.read(&mut buf);
            out.extend_from_slice(&buf[..n]);
            if let Some(e) = err {
                return (out, Some(e));
            }
        }
    }

    /// Four literal codes at litWidth 2: three at width 3, then the table overflows and the
    /// fourth is read at width 4. A reader that widens one code early or late reads garbage.
    #[test]
    fn the_code_width_widens_exactly_when_hi_reaches_overflow() {
        // 0b00, 0b01, 0b10 at 3 bits then 0b0011 at 4 bits, LSB first:
        //   bits (LSB first): 000 100 010 | 0 1100 -> bytes 0x88, 0x06 (13 bits, zero-padded).
        let data = [0b1000_1000u8, 0b0000_0110];
        let (out, err) = decode_all(2, &data);
        assert_eq!(out, vec![0, 1, 2, 3]);
        assert_eq!(err, Some(UNEXPECTED_EOF));
    }

    /// The end code stops the stream with `io.EOF`, which the GIF decoder reads as a clean end.
    #[test]
    fn the_end_code_is_reported_as_eof() {
        // literal 0 (3 bits, 0b000), end code 5 (3 bits, 0b101): 000 101 -> 0b00101000 = 0x28.
        let (out, err) = decode_all(2, &[0x28]);
        assert_eq!(out, vec![0]);
        assert_eq!(err, Some(EOF));
    }

    /// A code past `hi` is `lzw: invalid code`, not a silent zero.
    #[test]
    fn a_code_past_hi_is_rejected() {
        // literal 0, then code 7 (> hi = 5): 000 111 -> 0b00111000 = 0x38.
        let (out, err) = decode_all(2, &[0x38]);
        assert_eq!(out, vec![0]);
        assert_eq!(err, Some(Error::InvalidCode));
    }

    #[test]
    fn a_closed_reader_refuses_every_read() {
        let mut r = Reader::new(BytesReader::new(&[0x28]), 2);
        r.close();
        assert_eq!(r.read(&mut [0u8; 4]), (0, Some(Error::Closed)));
    }

    #[test]
    fn a_litwidth_outside_two_to_eight_is_rejected_before_any_read() {
        for lw in [-1i64, 0, 1, 9, 300] {
            let mut r = Reader::new(BytesReader::new(&[0x28]), lw);
            assert_eq!(r.read(&mut [0u8; 4]), (0, Some(Error::LitWidth(lw))));
        }
    }
}
