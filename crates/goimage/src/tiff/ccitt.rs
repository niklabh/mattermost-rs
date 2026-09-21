//! Port of `golang.org/x/image/ccitt`'s decoder (ccitt/reader.go and the generated decode tables
//! of ccitt/table.go at v0.44.0): Group 3 and Group 4 fax coding, which `tiff.Decode` uses for
//! compression values 3 and 4.
//!
//! Only the `NewReader` path is ported. `DecodeIntoGray` decodes the same bits into an
//! `*image.Gray` a caller supplies, and nothing in Mattermost's decode path calls it; the
//! encoder (ccitt/writer.go) has no caller either. The three decode tables below are transcribed
//! mechanically from the generated `table.go`; the encode tables belong to the writer.
//!
//! The reader is a bit-at-a-time walk of a binary tree. Each table entry is a branch node: the
//! next bit picks one of its two `i16`s, zero means an invalid code, a positive value names the
//! next branch, and a negative one is a leaf holding `!value`.

use crate::goread::{Error as IoError, Read};

use super::ccitt_tables::{BLACK_DECODE_TABLE, MODE_DECODE_TABLE, WHITE_DECODE_TABLE};

/// `ccitt.Order`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// `ccitt.LSB`.
    Lsb,
    /// `ccitt.MSB`.
    Msb,
}

/// `ccitt.SubFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubFormat {
    Group3,
    Group4,
}

/// `ccitt.Options`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Some variable-bit-width codes are byte-aligned.
    pub align: bool,
    /// Black is the 1 bit / 0xFF byte and white is 0.
    pub invert: bool,
}

/// Every error `ccitt`'s reader can produce, rendered with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// An error from the source reader, `io.EOF` included.
    #[error("{0}")]
    Io(#[from] IoError),
    #[error("ccitt: incomplete code")]
    IncompleteCode,
    #[error("ccitt: invalid bounds")]
    InvalidBounds,
    #[error("ccitt: invalid code")]
    InvalidCode,
    #[error("ccitt: invalid mode")]
    InvalidMode,
    #[error("ccitt: invalid offset")]
    InvalidOffset,
    #[error("ccitt: missing End-of-Line")]
    MissingEol,
    #[error("ccitt: run length overflows width")]
    RunLengthOverflowsWidth,
    #[error("ccitt: run length too long")]
    RunLengthTooLong,
    #[error("ccitt: unsupported mode")]
    UnsupportedMode,
    /// `errUnsupportedSubFormat`. Unreachable here: [`SubFormat`] has only the two Go names,
    /// while Go's `SubFormat` is a `uint32` that can hold a third value.
    #[error("ccitt: unsupported sub-format")]
    UnsupportedSubFormat,
    #[error("ccitt: unsupported width")]
    UnsupportedWidth,
}

impl Error {
    /// Whether this is the `io.EOF` that ends the stream.
    pub fn is_eof(&self) -> bool {
        *self == Error::Io(IoError::Eof)
    }
}

/// `maxWidth`: a limit of this implementation against integer overflow, not of the format.
const MAX_WIDTH: i64 = 1 << 20;

// Mode codes (table.go's "COPY PASTE" block).
const MODE_PASS: u32 = 0;
const MODE_H: u32 = 1;
const MODE_V0: u32 = 2;
const MODE_VR1: u32 = 3;
const MODE_VR2: u32 = 4;
const MODE_VR3: u32 = 5;
const MODE_VL1: u32 = 6;
const MODE_VL2: u32 = 7;
const MODE_VL3: u32 = 8;
const MODE_EXT: u32 = 9;

fn invert_bytes(b: &mut [u8]) {
    for c in b {
        *c = !*c;
    }
}

/// Port of `highBits` (reader.go:95): pack the high (0x80) bit of each `src` byte into `dst`,
/// most significant bit first, and report how many bytes were written and read.
fn high_bits(dst: &mut [u8], src: &[u8], invert: bool) -> (usize, usize) {
    // Pack as many complete groups of 8 src bytes as we can.
    let n = (src.len() / 8).min(dst.len());
    for i in 0..n {
        let s = &src[i * 8..i * 8 + 8];
        dst[i] = (s[0] & 0x80)
            | ((s[1] & 0x80) >> 1)
            | ((s[2] & 0x80) >> 2)
            | ((s[3] & 0x80) >> 3)
            | ((s[4] & 0x80) >> 4)
            | ((s[5] & 0x80) >> 5)
            | ((s[6] & 0x80) >> 6)
            | ((s[7] & 0x80) >> 7);
    }
    let (mut d, mut s) = (n, 8 * n);
    let (dst, src) = (&mut dst[d..], &src[s..]);

    // Pack up to 7 remaining src bytes, if there is room in dst. A partial byte is padded with
    // 1 bits when inverting, which `reader.Read`'s `invertBytes` then flips back to 0.
    if !dst.is_empty() && !src.is_empty() {
        let mut dst_byte = if invert { 0xFFu8 >> src.len() } else { 0 };
        for (n, src_byte) in src.iter().enumerate() {
            dst_byte |= (src_byte & 0x80) >> n;
        }
        dst[0] = dst_byte;
        d += 1;
        s += src.len();
    }
    (d, s)
}

/// Port of `bitReader` (reader.go:131).
struct BitReader<R> {
    r: R,
    /// The error from the most recent `r.Read`; its bytes are processed before it is.
    read_err: Option<IoError>,
    order: Order,
    /// The high `n_bits` bits hold upcoming bits in MSB order.
    bits: u64,
    n_bits: u32,
    /// `bytes[br..bw]` was read from `r` but is not yet in `bits`.
    br: u32,
    bw: u32,
    bytes: Box<[u8; 1024]>,
}

impl<R: Read> BitReader<R> {
    fn align_to_byte_boundary(&mut self) {
        let n = self.n_bits & 7;
        self.bits <<= n;
        self.n_bits -= n;
    }

    /// Port of `bitReader.nextBit` (reader.go:166).
    fn next_bit(&mut self) -> Result<u64, IoError> {
        loop {
            if self.n_bits > 0 {
                let bit = self.bits >> 63;
                self.bits <<= 1;
                self.n_bits -= 1;
                return Ok(bit);
            }

            let available = self.bw - self.br;
            if available >= 4 {
                // Read 32 bits although `bits` is 64, because `decode` may unread up to
                // `maxCodeLength` bits back into the other half.
                //
                // This is a speed choice, not a semantic one: forcing every load down the
                // one-byte path below changes nothing observable, so a mutation of the `>= 4`
                // is equivalent and survives the suite (measured).
                let i = self.br as usize;
                let w = u32::from_be_bytes([
                    self.bytes[i],
                    self.bytes[i + 1],
                    self.bytes[i + 2],
                    self.bytes[i + 3],
                ]);
                self.bits = u64::from(w) << 32;
                self.br += 4;
                self.n_bits = 32;
                continue;
            } else if available > 0 {
                self.bits = u64::from(self.bytes[self.br as usize]) << (7 * 8);
                self.br += 1;
                self.n_bits = 8;
                continue;
            }

            if let Some(e) = &self.read_err {
                return Err(e.clone());
            }

            let (n, err) = self.r.read(&mut self.bytes[..]);
            self.br = 0;
            self.bw = n as u32;
            self.read_err = err;

            if self.order != Order::Msb {
                for b in &mut self.bytes[..n] {
                    *b = b.reverse_bits();
                }
            }
        }
    }
}

/// Port of `decode` (reader.go:208): walk `decode_table` one bit at a time.
fn decode<R: Read>(b: &mut BitReader<R>, decode_table: &[[i16; 2]]) -> Result<u32, Error> {
    let (mut n_bits_read, mut bits_read, mut state) = (0u32, 0u64, 1i32);
    loop {
        let bit = match b.next_bit() {
            Ok(bit) => bit,
            Err(IoError::Eof) => return Err(Error::IncompleteCode),
            Err(e) => return Err(Error::Io(e)),
        };
        bits_read |= bit << (63 - n_bits_read);
        n_bits_read += 1;

        state = i32::from(decode_table[state as usize][(bit & 1) as usize]);
        if state < 0 {
            return Ok(!state as u32);
        } else if state == 0 {
            // Unread the bits we have read, then report an invalid code.
            //
            // This unread is a dead store, in Go as here: every caller of `decode` propagates
            // `errInvalidCode` straight up to `reader.Read`, which makes it sticky, so nothing
            // ever reads the restored bits. (`decodeEOL`'s unread, by contrast, is load-bearing:
            // a missing EOL there means "another row of pixel data".) Mutating either line is
            // therefore equivalent and survives the suite — measured, not assumed.
            b.bits = (b.bits >> n_bits_read) | bits_read;
            b.n_bits += n_bits_read;
            return Err(Error::InvalidCode);
        }
    }
}

/// Port of `decodeEOL` (reader.go:235): the 12-bit code 0000_0000_0001.
fn decode_eol<R: Read>(b: &mut BitReader<R>) -> Result<(), Error> {
    let (mut n_bits_read, mut bits_read) = (0u32, 0u64);
    loop {
        let bit = match b.next_bit() {
            Ok(bit) => bit,
            Err(IoError::Eof) => return Err(Error::MissingEol),
            Err(e) => return Err(Error::Io(e)),
        };
        bits_read |= bit << (63 - n_bits_read);
        n_bits_read += 1;

        if n_bits_read < 12 {
            if bit & 1 == 0 {
                continue;
            }
        } else if bit & 1 != 0 {
            return Ok(());
        }

        b.bits = (b.bits >> n_bits_read) | bits_read;
        b.n_bits += n_bits_read;
        return Err(Error::MissingEol);
    }
}

const FIND_B1: bool = false;
const FIND_B2: bool = true;

/// Port of `reader` (reader.go:263), as `NewReader` builds it.
pub struct Reader<R> {
    br: BitReader<R>,
    sub_format: SubFormat,
    width: i64,
    /// The rows left to decode, or negative when the height is not known in advance.
    rows_remaining: i64,
    /// The current and previous rows, one byte per pixel: 0x00 black, 0xFF white. `prev` is
    /// `None` — Go's nil — while the first row is being processed.
    curr: Option<Vec<u8>>,
    prev: Option<Vec<u8>>,
    /// `curr[..ri]` has been handed to `Read`.
    ri: usize,
    /// `curr[..wi]` has been decoded. Roughly the spec's a0 index.
    wi: usize,
    align: bool,
    invert: bool,
    /// Parts of the spec treat the start of a row as if `wi == -1`.
    at_start_of_row: bool,
    pen_color_is_white: bool,
    seen_start_of_image: bool,
    /// The input is missing the trailing 6 (Group 3) or 2 (Group 4) EOLs. Invalid per the spec,
    /// but Adobe Acrobat writes it, and this package accepts it silently.
    truncated: bool,
    read_err: Option<Error>,
}

impl<R: Read> Reader<R> {
    /// Port of `ccitt.NewReader` (reader.go:778). A bad width is reported from the first read,
    /// as Go's is.
    pub fn new(
        r: R,
        order: Order,
        sf: SubFormat,
        width: i64,
        height: i64,
        opts: Options,
    ) -> Reader<R> {
        let read_err = if width < 0 {
            Some(Error::InvalidBounds)
        } else if width > MAX_WIDTH {
            Some(Error::UnsupportedWidth)
        } else {
            None
        };
        Reader {
            br: BitReader {
                r,
                read_err: None,
                order,
                bits: 0,
                n_bits: 0,
                br: 0,
                bw: 0,
                bytes: Box::new([0; 1024]),
            },
            sub_format: sf,
            width,
            rows_remaining: height,
            curr: None,
            prev: None,
            ri: 0,
            wi: 0,
            align: opts.align,
            invert: opts.invert,
            at_start_of_row: false,
            pen_color_is_white: false,
            seen_start_of_image: false,
            truncated: false,
            read_err,
        }
    }

    fn curr_len(&self) -> usize {
        self.curr.as_ref().map_or(0, |c| c.len())
    }

    /// Port of `reader.Read` (reader.go:330). One bit per output pixel, MSB first, each row
    /// byte-aligned; the error accompanies the bytes, `io.EOF` included.
    pub fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        if let Some(e) = &self.read_err {
            return (0, Some(e.clone()));
        }
        let mut pos = 0usize;

        while pos < p.len() {
            // Allocate buffers (and decode any start-of-image codes) for the first row.
            if self.curr.is_none() {
                if !self.seen_start_of_image {
                    if let Err(e) = self.start_decode() {
                        self.read_err = Some(e);
                        break;
                    }
                    self.at_start_of_row = true;
                }
                self.curr = Some(vec![0u8; self.width.max(0) as usize]);
            }

            // Decode the next row, if necessary.
            if self.at_start_of_row {
                if self.rows_remaining < 0 {
                    // The height is unknown. If the next code is an EOL it is consumed and the
                    // image ends; if it is not, the bit reader has not advanced and this is
                    // another row of pixels. Group 3's previous row already consumed one of the
                    // six EOLs, so only Group 4 aligns here.
                    if self.align && self.sub_format == SubFormat::Group4 {
                        self.br.align_to_byte_boundary();
                    }
                    match decode_eol(&mut self.br) {
                        Err(Error::MissingEol) => {}
                        Err(e) => {
                            self.read_err = Some(e);
                            break;
                        }
                        Ok(()) => {
                            if let Err(e) = self.finish_decode(true) {
                                self.read_err = Some(e);
                                break;
                            }
                            self.read_err = Some(Error::Io(IoError::Eof));
                            break;
                        }
                    }
                } else if self.rows_remaining == 0 {
                    // The height was known and exactly that many rows are decoded.
                    if let Err(e) = self.finish_decode(false) {
                        self.read_err = Some(e);
                        break;
                    }
                    self.read_err = Some(Error::Io(IoError::Eof));
                    break;
                } else {
                    self.rows_remaining -= 1;
                }

                let final_row = self.rows_remaining == 0;
                if let Err(e) = self.decode_row(final_row) {
                    self.read_err = Some(e);
                    break;
                }
            }

            // Pack from curr (one byte per pixel) into p (one bit per pixel).
            let (pack_d, pack_s) = {
                let curr = self.curr.as_deref().unwrap_or(&[]);
                high_bits(&mut p[pos..], &curr[self.ri..], self.invert)
            };
            pos += pack_d;
            self.ri += pack_s;

            // Prepare to decode the next row, if necessary.
            if self.ri == self.curr_len() {
                self.ri = 0;
                std::mem::swap(&mut self.curr, &mut self.prev);
                self.at_start_of_row = true;
            }
        }

        if self.invert {
            invert_bytes(&mut p[..pos]);
        }
        (pos, self.read_err.clone())
    }

    fn pen_color(&self) -> u8 {
        if self.pen_color_is_white { 0xFF } else { 0x00 }
    }

    /// Port of `reader.startDecode` (reader.go:423).
    fn start_decode(&mut self) -> Result<(), Error> {
        if self.sub_format == SubFormat::Group3 {
            decode_eol(&mut self.br)?;
        }
        self.seen_start_of_image = true;
        Ok(())
    }

    /// Port of `reader.finishDecode` (reader.go:441).
    fn finish_decode(&mut self, already_seen_eol: bool) -> Result<(), Error> {
        let mut number_of_eols;
        match self.sub_format {
            SubFormat::Group3 => {
                if self.truncated {
                    return Ok(());
                }
                // The stream ends in a Return To Control of six consecutive EOLs, one of which
                // `startDecode` or `decodeRow` has already consumed.
                number_of_eols = 5;
            }
            SubFormat::Group4 => {
                let auto_detect_height = self.rows_remaining < 0;
                if auto_detect_height {
                    // `Read` has already aligned to a byte boundary.
                } else if self.align {
                    self.br.align_to_byte_boundary();
                }
                // Two EOLs end the stream. A missing first one, with an explicit height, is
                // taken as a truncated trailer rather than an error.
                if let Err(e) = decode_eol(&mut self.br) {
                    if e == Error::MissingEol && !auto_detect_height {
                        self.truncated = true;
                        return Ok(());
                    }
                    return Err(e);
                }
                number_of_eols = 1;
            }
        }
        if already_seen_eol {
            number_of_eols -= 1;
        }
        while number_of_eols > 0 {
            decode_eol(&mut self.br)?;
            number_of_eols -= 1;
        }
        Ok(())
    }

    /// Port of `reader.decodeRow` (reader.go:491).
    fn decode_row(&mut self, final_row: bool) -> Result<(), Error> {
        self.wi = 0;
        self.at_start_of_row = true;
        self.pen_color_is_white = true;

        if self.align {
            self.br.align_to_byte_boundary();
        }

        match self.sub_format {
            SubFormat::Group3 => {
                while self.wi < self.curr_len() {
                    self.decode_run()?;
                    self.at_start_of_row = false;
                }
                match decode_eol(&mut self.br) {
                    Err(Error::MissingEol) if final_row => {
                        self.truncated = true;
                        Ok(())
                    }
                    other => other,
                }
            }
            SubFormat::Group4 => {
                while self.wi < self.curr_len() {
                    let mode = decode(&mut self.br, &MODE_DECODE_TABLE)?;
                    match mode {
                        MODE_PASS => self.mode_pass()?,
                        MODE_H => self.mode_h()?,
                        MODE_V0 => self.mode_v(0)?,
                        MODE_VR1 => self.mode_v(1)?,
                        MODE_VR2 => self.mode_v(2)?,
                        MODE_VR3 => self.mode_v(3)?,
                        MODE_VL1 => self.mode_v(-1)?,
                        MODE_VL2 => self.mode_v(-2)?,
                        MODE_VL3 => self.mode_v(-3)?,
                        MODE_EXT => return Err(Error::UnsupportedMode),
                        // Go's `readerModes` array has a nil function for anything else.
                        _ => return Err(Error::InvalidMode),
                    }
                    self.at_start_of_row = false;
                }
                Ok(())
            }
        }
    }

    /// Port of `reader.decodeRun` (reader.go:537): make-up codes then one terminal code.
    fn decode_run(&mut self) -> Result<(), Error> {
        let table: &[[i16; 2]] = if self.pen_color_is_white {
            &WHITE_DECODE_TABLE
        } else {
            &BLACK_DECODE_TABLE
        };

        let mut total: i64 = 0;
        loop {
            let n = decode(&mut self.br, table)?;
            total += i64::from(n);
            if total > MAX_WIDTH {
                return Err(Error::RunLengthTooLong);
            }
            // Anything 0x3F or below is a terminal code.
            if n <= 0x3F {
                break;
            }
        }

        let room = self.curr_len() as i64 - self.wi as i64;
        if total > room {
            return Err(Error::RunLengthOverflowsWidth);
        }
        let pen_color = self.pen_color();
        let (wi, total) = (self.wi, total as usize);
        if let Some(curr) = self.curr.as_mut() {
            for slot in &mut curr[wi..wi + total] {
                *slot = pen_color;
            }
        }
        self.wi += total;
        self.pen_color_is_white = !self.pen_color_is_white;
        Ok(())
    }

    /// Port of `reader.findB` (reader.go:618): b1 or b2, the changing elements on the row above.
    fn find_b(&self, which_b: bool) -> i64 {
        let curr_len = self.curr_len();
        let prev = self.prev.as_deref().unwrap_or(&[]);
        // The first row is a special case: the row above is implicitly all white, so there are
        // no changing elements and b1 and b2 are at the end of the row.
        if prev.len() != curr_len {
            return curr_len as i64;
        }

        let mut i = self.wi;

        if self.at_start_of_row {
            // a0 is implicitly at -1 on a white pixel: b1 is the first black pixel above, and
            // b2 the first white pixel after that.
            while i < prev.len() && prev[i] == 0xFF {
                i += 1;
            }
            if which_b == FIND_B2 {
                while i < prev.len() && prev[i] == 0x00 {
                    i += 1;
                }
            }
            return i as i64;
        }

        // Assuming the pen is white: walk past every contiguous black pixel above from a0,
        let opposite_color = !self.pen_color();
        while i < prev.len() && prev[i] == opposite_color {
            i += 1;
        }
        // then past every contiguous white one. What follows is b1.
        let pen_color = !opposite_color;
        while i < prev.len() && prev[i] == pen_color {
            i += 1;
        }
        if which_b == FIND_B2 {
            let opposite_color = !pen_color;
            while i < prev.len() && prev[i] == opposite_color {
                i += 1;
            }
        }
        i as i64
    }

    /// Port of `readerModePass` (reader.go:681).
    fn mode_pass(&mut self) -> Result<(), Error> {
        let b2 = self.find_b(FIND_B2);
        // `find_b` starts at `wi` and only walks forward, stopping at `prev.len()`, which equals
        // `curr.len()` on every row it does not short-circuit. So this guard — Go's, kept — can
        // never fire, and a mutation of it is equivalent. `mode_v`'s identical-looking guard
        // *can* fire, because of its -3..+3 adjustment, and the suite catches mutations there.
        if b2 < self.wi as i64 || (self.curr_len() as i64) < b2 {
            return Err(Error::InvalidOffset);
        }
        self.fill_to(b2 as usize);
        self.wi = b2 as usize;
        Ok(())
    }

    /// Port of `readerModeH` (reader.go:695): the first run finds a1, the second a2.
    fn mode_h(&mut self) -> Result<(), Error> {
        self.decode_run()?;
        self.decode_run()
    }

    /// Port of `readerModeV` (reader.go:705).
    fn mode_v(&mut self, arg: i64) -> Result<(), Error> {
        let a1 = self.find_b(FIND_B1) + arg;
        if a1 < self.wi as i64 || (self.curr_len() as i64) < a1 {
            return Err(Error::InvalidOffset);
        }
        self.fill_to(a1 as usize);
        self.wi = a1 as usize;
        self.pen_color_is_white = !self.pen_color_is_white;
        Ok(())
    }

    /// `curr[wi..end] = penColor`, shared by the pass and vertical modes.
    fn fill_to(&mut self, end: usize) {
        let pen_color = self.pen_color();
        let wi = self.wi;
        if let Some(curr) = self.curr.as_mut() {
            for slot in &mut curr[wi..end] {
                *slot = pen_color;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `highBits` packs the 0x80 bit of each byte, MSB first, and pads a final partial byte with
    /// 1s only when inverting — `reader.Read` flips those back.
    #[test]
    fn high_bits_packs_the_top_bit_of_each_byte() {
        let src = [0x7D, 0x7E, 0x7F, 0x80, 0x81, 0x82, 0x00, 0xFF];
        let mut dst = [0u8; 4];
        assert_eq!(high_bits(&mut dst, &src, false), (1, 8));
        assert_eq!(dst[0], 0x1D);
        // Three leftover bytes, all high: the tail of the byte is padded with 0s, or with 1s
        // when inverting.
        let mut dst = [0u8; 4];
        assert_eq!(high_bits(&mut dst, &src[3..6], false), (1, 3));
        assert_eq!(dst[0], 0b1110_0000);
        let mut dst = [0u8; 4];
        assert_eq!(high_bits(&mut dst, &src[3..6], true), (1, 3));
        assert_eq!(dst[0], 0b1111_1111);
        // Three leftover bytes, none high.
        let mut dst = [0u8; 4];
        assert_eq!(high_bits(&mut dst, &src[..3], false), (1, 3));
        assert_eq!(dst[0], 0);
        // No room in dst for the remainder: it is neither written nor counted as read.
        let mut nine = src.to_vec();
        nine.push(0xff);
        let mut dst = [0u8; 1];
        assert_eq!(high_bits(&mut dst, &nine, false), (1, 8));
    }

    #[test]
    fn decode_eol_wants_eleven_zeros_and_a_one() {
        use crate::goread::BytesReader;
        let b = |s: &[u8]| -> Result<(), Error> {
            let mut br = BitReader {
                r: BytesReader::new(s),
                read_err: None,
                order: Order::Msb,
                bits: 0,
                n_bits: 0,
                br: 0,
                bw: 0,
                bytes: Box::new([0; 1024]),
            };
            decode_eol(&mut br)
        };
        assert_eq!(b(&[0x00, 0x10]), Ok(()));
        // A one before the twelfth bit, and a zero at it, are both a missing EOL.
        assert_eq!(b(&[0x00, 0x20]), Err(Error::MissingEol));
        assert_eq!(b(&[0x00, 0x00]), Err(Error::MissingEol));
        // Run out of bits: also a missing EOL, never `io.EOF`.
        assert_eq!(b(&[0x00]), Err(Error::MissingEol));
        assert_eq!(b(&[]), Err(Error::MissingEol));
    }

    /// `NewReader` reports a bad width from the first read, as Go does, not from the call.
    #[test]
    fn a_bad_width_is_reported_from_the_first_read() {
        use crate::goread::BytesReader;
        let mut r = Reader::new(
            BytesReader::new(&[]),
            Order::Msb,
            SubFormat::Group4,
            -1,
            1,
            Options::default(),
        );
        assert_eq!(r.read(&mut [0u8; 4]), (0, Some(Error::InvalidBounds)));
        let mut r = Reader::new(
            BytesReader::new(&[]),
            Order::Msb,
            SubFormat::Group4,
            (1 << 20) + 1,
            1,
            Options::default(),
        );
        assert_eq!(r.read(&mut [0u8; 4]), (0, Some(Error::UnsupportedWidth)));
        // `errUnsupportedSubFormat` cannot be produced by this port; only its text is checked.
        assert_eq!(
            Error::UnsupportedSubFormat.to_string(),
            "ccitt: unsupported sub-format"
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::goread::BytesReader;
    use crate::testsupport::{b64, fixture, sha};

    /// `io.ReadAll` over the reader: the bytes produced before the error, and the error.
    fn read_all(r: &mut Reader<BytesReader<'_>>) -> (Vec<u8>, Option<Error>) {
        let mut out = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let (n, e) = r.read(&mut chunk);
            out.extend_from_slice(&chunk[..n]);
            match e {
                None => {}
                Some(e) if e.is_eof() => return (out, None),
                Some(e) => return (out, Some(e)),
            }
        }
    }

    /// Every case of the oracle's direct `ccitt.NewReader` corpus: the raw CCITT streams
    /// x/image ships, each read at both bit orders, with and without byte alignment, inverted
    /// and not, at the right geometry and at several wrong ones, truncated and corrupted.
    ///
    /// `tiff.Decode` only ever asks for `Align: false` and an explicit height, so without this
    /// corpus the alignment and auto-detect-height branches would have no oracle at all.
    #[test]
    fn the_reader_matches_go_on_every_ccitt_case() {
        let cases = fixture("tiff")["ccitt"].as_array().unwrap();
        let mut n = 0;
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            let order = if c["order"] == "lsb" {
                Order::Lsb
            } else {
                Order::Msb
            };
            let sub = if c["sub"] == "group4" {
                SubFormat::Group4
            } else {
                SubFormat::Group3
            };
            let opts = Options {
                align: c["align"].as_bool().unwrap(),
                invert: c["invert"].as_bool().unwrap(),
            };
            let mut r = Reader::new(
                BytesReader::new(&data),
                order,
                sub,
                c["width"].as_i64().unwrap(),
                c["height"].as_i64().unwrap(),
                opts,
            );
            let (out, err) = read_all(&mut r);
            assert_eq!(Some(out.len() as u64), c["out_len"].as_u64(), "{name}: len");
            assert_eq!(sha(&out), c["out_sha256"], "{name}: bytes");
            match err {
                Some(e) => assert_eq!(c["err"], e.to_string(), "{name}: error"),
                None => assert!(c["err"].is_null(), "{name}: expected {}", c["err"]),
            }
            n += 1;
        }
        assert!(n > 140, "{n}");
    }
}
