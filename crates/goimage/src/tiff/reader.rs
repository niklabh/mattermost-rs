//! Port of `golang.org/x/image/tiff`'s decoder (tiff/reader.go, buffer.go, compress.go,
//! consts.go at v0.44.0): `tiff.Decode` and `tiff.DecodeConfig` as `image.Decode` calls them.
//! `tiff/writer.go` is deliberately not ported — Mattermost never encodes TIFF.
//!
//! # The reader chain is Go's
//!
//! `image.Decode` hands the decoder a `bufio.Reader` over the caller's `bytes.Reader`, and a
//! `bufio.Reader` is not an `io.ReaderAt`, so `newReaderAt` always wraps it in the `buffer` of
//! buffer.go. That choice is observable twice over: uncompressed strips are taken as a **window
//! on the buffer** rather than a copy (so the horizontal predictor writes through it, and a later
//! strip reading the same bytes sees the difference), and a short file surfaces as `io.EOF` or
//! `io.ErrUnexpectedEOF` depending on whether `io.ReadFull` got *no* bytes or *some*. [`Buffer`]
//! is therefore a port of that type and not a slice cursor. The `io.ReaderAt` fast path in
//! `Decode`'s `cNone` arm is unreachable from `image.Decode` and is not ported.
//!
//! # Allocation
//!
//! Every size that comes out of the file is guarded exactly where Go guards it: `safemath.Mul3`
//! on the image and the tile, `blockMaxDataSize` on each decompressed strip, `maxChunkSize` on
//! IFD reads, `maxCount` on the entries that scale with the image. A caller facing untrusted
//! input still runs [`decode_config`] against a resolution limit first, as Mattermost's
//! `imaging.Decoder` does.

use std::collections::BTreeMap;
use std::ops::Range;

use super::ccitt;
use super::lzw;
use crate::goread::{BufReader, Error as IoError, Read};
use crate::image::{Color, Image, Paletted, Pixels, Rect};
use crate::zlib::reader::Reader as ZlibReader;

// --- consts.go ------------------------------------------------------------------------------

const LE_HEADER: &[u8] = b"II\x2a\x00";
const BE_HEADER: &[u8] = b"MM\x00\x2a";
/// Length of an IFD entry in bytes.
const IFD_LEN: usize = 12;

// Data types (p. 14-16 of the spec).
const DT_BYTE: u16 = 1;
const DT_SHORT: u16 = 3;
const DT_LONG: u16 = 4;

/// The length of one instance of each data type in bytes.
const LENGTHS: [u32; 6] = [0, 1, 1, 2, 4, 8];

// Tags (see p. 28-41 of the spec).
const T_IMAGE_WIDTH: u16 = 256;
const T_IMAGE_LENGTH: u16 = 257;
const T_BITS_PER_SAMPLE: u16 = 258;
const T_COMPRESSION: u16 = 259;
const T_PHOTOMETRIC_INTERPRETATION: u16 = 262;
const T_FILL_ORDER: u16 = 266;
const T_STRIP_OFFSETS: u16 = 273;
const T_ROWS_PER_STRIP: u16 = 278;
const T_STRIP_BYTE_COUNTS: u16 = 279;
const T_T4_OPTIONS: u16 = 292;
const T_T6_OPTIONS: u16 = 293;
const T_TILE_WIDTH: u16 = 322;
const T_TILE_LENGTH: u16 = 323;
const T_TILE_OFFSETS: u16 = 324;
const T_TILE_BYTE_COUNTS: u16 = 325;
const T_PREDICTOR: u16 = 317;
const T_COLOR_MAP: u16 = 320;
const T_EXTRA_SAMPLES: u16 = 338;
const T_SAMPLE_FORMAT: u16 = 339;

// Compression types.
const C_NONE: u64 = 1;
const C_G3: u64 = 3;
const C_G4: u64 = 4;
const C_LZW: u64 = 5;
const C_DEFLATE: u64 = 8;
const C_PACK_BITS: u64 = 32773;
const C_DEFLATE_OLD: u64 = 32946;

// Photometric interpretation values (see p. 37 of the spec).
const P_WHITE_IS_ZERO: u64 = 0;
const P_BLACK_IS_ZERO: u64 = 1;
const P_RGB: u64 = 2;
const P_PALETTED: u64 = 3;

/// `prHorizontal`, the only predictor this decoder acts on.
const PR_HORIZONTAL: u64 = 2;

/// `imageMode` (consts.go:106). `mBilevel` is declared in Go and never assigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageMode {
    Paletted,
    Gray,
    GrayInvert,
    Rgb,
    Rgba,
    Nrgba,
}

// --- errors ---------------------------------------------------------------------------------

/// Every error `tiff.Decode`/`tiff.DecodeConfig` can return, rendered with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `tiff.FormatError` (reader.go:26). Every one of Go's messages is a literal.
    #[error("tiff: invalid format: {0}")]
    Format(&'static str),
    /// `tiff.UnsupportedError` (reader.go:34).
    #[error("tiff: unsupported feature: {0}")]
    Unsupported(String),
    /// An `io` or `compress/zlib` error travelling up unchanged.
    #[error("{0}")]
    Io(#[from] IoError),
    /// An error from `tiff/lzw`.
    #[error("{0}")]
    Lzw(#[from] lzw::Error),
    /// An error from `golang.org/x/image/ccitt`, which decodes Group 3 and Group 4 fax data.
    #[error("{0}")]
    Ccitt(#[from] ccitt::Error),
}

/// `errNoPixels`.
const ERR_NO_PIXELS: Error = Error::Format("not enough pixel data");
/// `errInvalidColorIndex`.
const ERR_INVALID_COLOR_INDEX: Error = Error::Format("invalid color index");

/// One `Read` call's outcome with `io.EOF` — which `bytes.Buffer.ReadFrom` treats as success —
/// separated from a real failure.
enum Stop {
    Eof,
    Err(Error),
}

impl From<IoError> for Stop {
    fn from(e: IoError) -> Stop {
        if e == IoError::Eof {
            Stop::Eof
        } else {
            Stop::Err(Error::Io(e))
        }
    }
}

impl From<lzw::Error> for Stop {
    fn from(e: lzw::Error) -> Stop {
        if e.is_eof() {
            Stop::Eof
        } else {
            Stop::Err(Error::Lzw(e))
        }
    }
}

impl From<ccitt::Error> for Stop {
    fn from(e: ccitt::Error) -> Stop {
        if e.is_eof() {
            Stop::Eof
        } else {
            Stop::Err(Error::Ccitt(e))
        }
    }
}

// --- buffer.go ------------------------------------------------------------------------------

const FILL_CHUNK_SIZE: usize = 10 << 20;
/// `maxChunkSize` (reader.go:45).
const MAX_CHUNK_SIZE: usize = 10 << 20;

/// Port of `buffer` (buffer.go:15): an `io.Reader` presented as an `io.ReaderAt` by keeping
/// everything read so far. Here the source is a slice, so "reading" is copying a prefix of it —
/// but the copy is real, because the predictor writes into it.
struct Buffer<'a> {
    src: &'a [u8],
    buf: Vec<u8>,
}

impl<'a> Buffer<'a> {
    fn new(src: &'a [u8]) -> Buffer<'a> {
        Buffer {
            src,
            buf: Vec::with_capacity(1024),
        }
    }

    /// Port of `buffer.fill` (buffer.go:23). Go allocates `next` bytes before reading them; this
    /// appends only what arrives, which no caller can observe and no corrupt length can exploit.
    fn fill(&mut self, end: usize) -> Option<IoError> {
        while self.buf.len() < end {
            let m = self.buf.len();
            let next = (end - m).min(FILL_CHUNK_SIZE);
            let n = next.min(self.src.len() - m.min(self.src.len()));
            self.buf.extend_from_slice(&self.src[m..m + n]);
            if n < next {
                // `io.ReadFull`: no bytes is `io.EOF`, some bytes is `io.ErrUnexpectedEOF`.
                return Some(if n == 0 {
                    IoError::Eof
                } else {
                    IoError::UnexpectedEof
                });
            }
        }
        None
    }

    /// Port of `buffer.ReadAt` (buffer.go:41). The error travels with the bytes, as Go's does.
    fn read_at(&mut self, p: &mut [u8], off: u64) -> (usize, Option<IoError>) {
        let Some(end64) = off
            .checked_add(p.len() as u64)
            .filter(|e| *e <= i64::MAX as u64)
        else {
            return (0, Some(IoError::UnexpectedEof));
        };
        let err = self.fill(end64 as usize);
        let end = (end64 as usize).min(self.buf.len());
        let start = (off as usize).min(end);
        let n = p.len().min(end - start);
        p[..n].copy_from_slice(&self.buf[start..start + n]);
        (n, err)
    }

    /// Port of `buffer.Slice` (buffer.go:59), as the range Go's returned slice covers.
    fn slice(&mut self, off: u64, n: u64) -> Result<Range<usize>, IoError> {
        let Some(end) = off.checked_add(n).filter(|e| *e <= i64::MAX as u64) else {
            return Err(IoError::UnexpectedEof);
        };
        if let Some(e) = self.fill(end as usize) {
            return Err(e);
        }
        Ok(off as usize..end as usize)
    }

    /// Port of `safeReadAt` (reader.go:53) — `internal/saferio.ReadDataAt`: never allocate a
    /// length read out of the file before proving the file has that much data behind it.
    fn safe_read_at(&mut self, n: u64, off: u64) -> Result<Vec<u8>, IoError> {
        if n > i64::MAX as u64 {
            return Err(IoError::UnexpectedEof);
        }
        if n < MAX_CHUNK_SIZE as u64 {
            let mut buf = vec![0u8; n as usize];
            if let (_, Some(e)) = self.read_at(&mut buf, off) {
                // `io.SectionReader` can return EOF for n == 0, which is a success here.
                if e != IoError::Eof || n > 0 {
                    return Err(e);
                }
            }
            return Ok(buf);
        }
        let mut buf = Vec::new();
        let mut buf1 = vec![0u8; MAX_CHUNK_SIZE];
        let (mut n, mut off) = (n, off);
        while n > 0 {
            let next = n.min(MAX_CHUNK_SIZE as u64) as usize;
            if let (_, Some(e)) = self.read_at(&mut buf1[..next], off) {
                return Err(e);
            }
            buf.extend_from_slice(&buf1[..next]);
            n -= next as u64;
            off += next as u64;
        }
        Ok(buf)
    }
}

/// Port of `io.SectionReader` over the [`Buffer`], which is what every compressed branch of
/// `Decode` reads its strip through.
struct Section<'a, 'b> {
    r: &'b mut Buffer<'a>,
    off: u64,
    limit: u64,
}

impl Read for Section<'_, '_> {
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<IoError>) {
        if self.off >= self.limit {
            return (0, Some(IoError::Eof));
        }
        let max = self.limit - self.off;
        let p = if p.len() as u64 > max {
            &mut p[..max as usize]
        } else {
            p
        };
        let (n, err) = self.r.read_at(p, self.off);
        self.off += n as u64;
        (n, err)
    }
}

// --- compress.go ----------------------------------------------------------------------------

/// Port of `unpackBits` (compress.go:21): PackBits, with Go's decompression-bomb limit.
///
/// Go wraps the `io.SectionReader` in a `bufio.Reader` because it is not an `io.ByteReader`, so
/// the caller passes one here too and the short-read behaviour below matches.
fn unpack_bits<R: crate::goread::ByteRead>(br: &mut R, lim: i64) -> Result<Vec<u8>, Error> {
    let mut buf = [0u8; 128];
    let mut dst: Vec<u8> = Vec::with_capacity(1024);
    loop {
        let b = match br.read_byte() {
            Ok(b) => b,
            Err(IoError::Eof) => return Ok(dst),
            Err(e) => return Err(Error::Io(e)),
        };
        let code = b as i8 as i32;
        if code >= 0 {
            let want = code as usize + 1;
            let (n, err) = crate::goread::read_full(br, &mut buf[..want]);
            if let Some(e) = err {
                return Err(Error::Io(e));
            }
            dst.extend_from_slice(&buf[..n]);
        } else if code == -128 {
            // No-op.
        } else {
            let b = match br.read_byte() {
                Ok(b) => b,
                Err(e) => return Err(Error::Io(e)),
            };
            let run = (1 - code) as usize;
            for slot in buf.iter_mut().take(run) {
                *slot = b;
            }
            dst.extend_from_slice(&buf[..run]);
        }
        if dst.len() as i64 > lim {
            return Err(Error::Format("PackBits: decompressed data too large"));
        }
    }
}

/// Port of `readBuf` (reader.go:901): `bytes.Buffer.ReadFrom(io.LimitReader(r, lim))`. The limit
/// is a hard stop — once `lim` bytes are out, the reader is not called again, so a strip that
/// would decompress to more than the block can hold is silently truncated rather than rejected.
fn read_buf<F>(mut read: F, lim: i64) -> (Vec<u8>, Option<Error>)
where
    F: FnMut(&mut [u8]) -> (usize, Option<Stop>),
{
    let lim = lim.max(0) as usize;
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    while out.len() < lim {
        let want = chunk.len().min(lim - out.len());
        let (n, stop) = read(&mut chunk[..want]);
        out.extend_from_slice(&chunk[..n]);
        match stop {
            None => {}
            Some(Stop::Eof) => return (out, None),
            Some(Stop::Err(e)) => return (out, Some(e)),
        }
    }
    (out, None)
}

// --- the decoder ----------------------------------------------------------------------------

/// Byte order, as `binary.LittleEndian` / `binary.BigEndian`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn u16(self, p: &[u8]) -> u16 {
        let b = [p[0], p[1]];
        match self {
            ByteOrder::Little => u16::from_le_bytes(b),
            ByteOrder::Big => u16::from_be_bytes(b),
        }
    }

    fn u32(self, p: &[u8]) -> u32 {
        let b = [p[0], p[1], p[2], p[3]];
        match self {
            ByteOrder::Little => u32::from_le_bytes(b),
            ByteOrder::Big => u32::from_be_bytes(b),
        }
    }

    fn put_u16(self, p: &mut [u8], v: u16) {
        let b = match self {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        };
        p[0] = b[0];
        p[1] = b[1];
    }
}

/// The colour model `tiff.DecodeConfig` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigModel {
    Gray,
    Gray16,
    Rgba,
    Rgba64,
    Nrgba,
    Nrgba64,
    /// `color.Palette(d.palette)` — the ColorMap entries, as `color.RGBA64`. Empty when the file
    /// says Paletted and carries no ColorMap.
    Palette(Vec<Color>),
}

/// `image.Config` for a TIFF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i64,
    pub height: i64,
    pub model: ConfigModel,
}

/// Where the current strip or tile's bytes live.
enum Block {
    /// `readBuf`/`unpackBits` allocated them.
    Owned(Vec<u8>),
    /// `buffer.Slice` handed back a window on the file buffer. Go's `d.buf` aliases that buffer,
    /// so the horizontal predictor's `d.buf[off] += d.buf[off-n]` writes into the file's own
    /// bytes and a later block covering the same range reads the accumulated values.
    Window(Range<usize>),
}

/// `readBits`' state: `d.off`, `d.v` and `d.nbits`, kept together so the bit reader can borrow
/// them while the block's bytes are borrowed from elsewhere in the decoder.
#[derive(Default)]
struct Bits {
    off: usize,
    v: u32,
    nbits: u32,
}

impl Bits {
    /// Port of `decoder.readBits` (reader.go:271).
    fn read_bits(&mut self, buf: &[u8], n: u32) -> Option<u32> {
        while self.nbits < n {
            self.v <<= 8;
            if self.off >= buf.len() {
                return None;
            }
            self.v |= u32::from(buf[self.off]);
            self.off += 1;
            self.nbits += 8;
        }
        self.nbits -= n;
        let rv = self.v >> self.nbits;
        self.v &= !(rv << self.nbits);
        Some(rv)
    }

    /// Port of `decoder.flushBits` (reader.go:289).
    fn flush_bits(&mut self) {
        self.v = 0;
        self.nbits = 0;
    }
}

/// Port of `decoder` (reader.go:91).
struct Decoder<'a> {
    r: Buffer<'a>,
    byte_order: ByteOrder,
    width: i64,
    height: i64,
    model: ConfigModel,
    mode: ImageMode,
    bpp: u64,
    features: BTreeMap<u16, Vec<u64>>,
    ifd: BTreeMap<u16, [u8; IFD_LEN]>,
    palette: Vec<Color>,
    block: Block,
    bits: Bits,
}

/// Port of `safemath.Mul3`: `x*y*z` unless an argument is negative or the product overflows an
/// `int`.
fn mul3(x: i64, y: i64, z: i64) -> Option<i64> {
    if x < 0 || y < 0 || z < 0 {
        return None;
    }
    x.checked_mul(y)?.checked_mul(z)
}

impl<'a> Decoder<'a> {
    /// `firstVal` (reader.go:109).
    fn first_val(&self, tag: u16) -> u64 {
        self.features
            .get(&tag)
            .and_then(|f| f.first())
            .copied()
            .unwrap_or(0)
    }

    /// `firstIntVal` (reader.go:120). Go's `int` is 64-bit on every platform this runs on.
    fn first_int_val(&self, tag: u16) -> Result<i64, Error> {
        let v = self.first_val(tag);
        if v > i64::MAX as u64 {
            return Err(Error::Format("IFD value too large"));
        }
        Ok(v as i64)
    }

    /// `samples per pixel`: `len(d.features[tBitsPerSample])`.
    fn samples(&self) -> usize {
        self.features.get(&T_BITS_PER_SAMPLE).map_or(0, |v| v.len())
    }

    /// Port of `ifdUint` (reader.go:133).
    fn ifd_uint(&mut self, p: &[u8], max_count: i64) -> Result<Vec<u64>, Error> {
        if p.len() < IFD_LEN {
            return Err(Error::Format("bad IFD entry"));
        }
        let datatype = self.byte_order.u16(&p[2..4]);
        if datatype == 0 || usize::from(datatype) >= LENGTHS.len() {
            return Err(Error::Unsupported("IFD entry datatype".to_owned()));
        }
        let unit = LENGTHS[usize::from(datatype)];
        let count = self.byte_order.u32(&p[4..8]);
        if count > i32::MAX as u32 / unit {
            return Err(Error::Format("IFD data too large"));
        }
        let truncated_count = i64::from(count).min(max_count).max(0) as usize;
        // Go's `lengths[datatype] * count` is `uint32` arithmetic. The check above keeps the
        // product inside `MaxInt32`, so the wrap is unreachable — but a debug-mode panic here
        // would be a divergence from Go, where it would silently wrap.
        let datalen = unit.wrapping_mul(count);
        let raw: Vec<u8> = if datalen > 4 {
            let truncated_len = u64::from(unit) * truncated_count as u64;
            self.r
                .safe_read_at(truncated_len, u64::from(self.byte_order.u32(&p[8..12])))?
        } else {
            p[8..8 + datalen as usize].to_vec()
        };
        let mut u = vec![0u64; truncated_count];
        match datatype {
            DT_BYTE => {
                for (i, slot) in u.iter_mut().enumerate() {
                    *slot = u64::from(raw[i]);
                }
            }
            DT_SHORT => {
                for (i, slot) in u.iter_mut().enumerate() {
                    *slot = u64::from(self.byte_order.u16(&raw[2 * i..2 * (i + 1)]));
                }
            }
            DT_LONG => {
                for (i, slot) in u.iter_mut().enumerate() {
                    *slot = u64::from(self.byte_order.u32(&raw[4 * i..4 * (i + 1)]));
                }
            }
            // dtASCII and dtRational reach here: Go's `default` arm.
            _ => return Err(Error::Unsupported("data type".to_owned())),
        }
        Ok(u)
    }

    /// Port of `parseIFDOffsets` (reader.go:183).
    fn parse_ifd_offsets(&mut self, tag: u16, max_count: i64) -> Result<Vec<u64>, Error> {
        let Some(p) = self.ifd.get(&tag).copied() else {
            return Ok(Vec::new());
        };
        self.ifd_uint(&p, max_count)
    }

    /// Port of `parseIFD` (reader.go:194): stow away the entries the decoder cares about and
    /// return the tag, which the caller checks is ascending.
    fn parse_ifd(&mut self, p: &[u8]) -> Result<i64, Error> {
        /// The limit for parsed IFD entries that do not scale with the image size.
        const SMALL_ENTRY_MAX_COUNT: i64 = 16;

        let tag = self.byte_order.u16(&p[0..2]);
        match tag {
            T_BITS_PER_SAMPLE
            | T_EXTRA_SAMPLES
            | T_PHOTOMETRIC_INTERPRETATION
            | T_COMPRESSION
            | T_PREDICTOR
            | T_ROWS_PER_STRIP
            | T_TILE_WIDTH
            | T_TILE_LENGTH
            | T_IMAGE_LENGTH
            | T_IMAGE_WIDTH
            | T_FILL_ORDER
            | T_T4_OPTIONS
            | T_T6_OPTIONS => {
                let val = self.ifd_uint(p, SMALL_ENTRY_MAX_COUNT)?;
                self.features.insert(tag, val);
            }
            T_STRIP_OFFSETS | T_STRIP_BYTE_COUNTS | T_TILE_OFFSETS | T_TILE_BYTE_COUNTS => {
                // These keys may contain many values; parse them once the image size is known.
                let mut v = [0u8; IFD_LEN];
                let n = p.len().min(IFD_LEN);
                v[..n].copy_from_slice(&p[..n]);
                self.ifd.insert(tag, v);
            }
            T_COLOR_MAP => {
                const MAX_COLORS: i64 = 256;
                let val = self.ifd_uint(p, (3 * MAX_COLORS) + 1)?;
                let numcolors = val.len() / 3;
                if val.len() % 3 != 0 || numcolors == 0 || numcolors > MAX_COLORS as usize {
                    return Err(Error::Format("bad ColorMap length"));
                }
                self.palette = (0..numcolors)
                    .map(|i| {
                        Color::Rgba64([
                            val[i] as u16,
                            val[i + numcolors] as u16,
                            val[i + 2 * numcolors] as u16,
                            0xffff,
                        ])
                    })
                    .collect();
            }
            T_SAMPLE_FORMAT => {
                // Page 27 of the spec: a Baseline reader that cannot handle a SampleFormat other
                // than 1 (unsigned integer data) must terminate the import gracefully.
                let val = self.ifd_uint(p, SMALL_ENTRY_MAX_COUNT)?;
                if val.iter().any(|v| *v != 1) {
                    return Err(Error::Unsupported("sample format".to_owned()));
                }
            }
            _ => {}
        }
        Ok(i64::from(tag))
    }

    /// Port of `decoder.decode` (reader.go:304): the current block's bytes into `dst`.
    fn decode_block(
        &mut self,
        dst: &mut Image,
        xmin: i64,
        ymin: i64,
        xmax: i64,
        ymax: i64,
    ) -> Result<(), Error> {
        let Decoder {
            r,
            byte_order,
            mode,
            bpp,
            block,
            bits,
            palette,
            ..
        } = self;
        let byte_order = *byte_order;
        let (mode, bpp) = (*mode, *bpp);
        let samples = self.features.get(&T_BITS_PER_SAMPLE).map_or(0, |v| v.len()) as i64;
        let predictor = self
            .features
            .get(&T_PREDICTOR)
            .and_then(|f| f.first())
            .copied()
            .unwrap_or(0);
        let buf: &mut [u8] = match block {
            Block::Owned(v) => v.as_mut_slice(),
            Block::Window(w) => &mut r.buf[w.clone()],
        };
        bits.off = 0;

        // Apply the horizontal predictor: each sample holds the difference to the preceding
        // pixel's (page 64-65 of the spec).
        if predictor == PR_HORIZONTAL {
            match bpp {
                16 => {
                    let mut off: i64 = 0;
                    let n = 2 * samples; // bytes per sample times samples per pixel
                    for _y in ymin..ymax {
                        off += n;
                        let mut x = 0;
                        while x < (xmax - xmin - 1) * n {
                            if off + 2 > buf.len() as i64 {
                                return Err(ERR_NO_PIXELS);
                            }
                            let o = off as usize;
                            let k = (off - n) as usize;
                            let v0 = byte_order.u16(&buf[k..k + 2]);
                            let v1 = byte_order.u16(&buf[o..o + 2]);
                            byte_order.put_u16(&mut buf[o..o + 2], v1.wrapping_add(v0));
                            off += 2;
                            x += 2;
                        }
                    }
                }
                8 => {
                    let mut off: i64 = 0;
                    let n = samples; // one byte per sample times samples per pixel
                    for _y in ymin..ymax {
                        off += n;
                        let mut x = 0;
                        while x < (xmax - xmin - 1) * n {
                            if off >= buf.len() as i64 {
                                return Err(ERR_NO_PIXELS);
                            }
                            let o = off as usize;
                            buf[o] = buf[o].wrapping_add(buf[o - n as usize]);
                            off += 1;
                            x += 1;
                        }
                    }
                }
                1 => {
                    return Err(Error::Unsupported(
                        "horizontal predictor with 1 BitsPerSample".to_owned(),
                    ));
                }
                _ => {}
            }
        }

        // `calcRowBytes`: the bytes in a row of `num_samples` samples of `bits_per_sample` bits.
        let calc_row_bytes = |num_samples: i64, bits_per_sample: u64| -> i64 {
            ((xmax - xmin) * num_samples * bits_per_sample as i64 + 7) / 8
        };
        // `calcRowOff`: the offset of row `y`.
        let calc_row_off = |y: i64, row_bytes: i64| -> usize { ((y - ymin) * row_bytes) as usize };

        let bounds = dst.bounds();
        let r_max_x = xmax.min(bounds.max_x);
        let r_max_y = ymax.min(bounds.max_y);
        match (mode, dst) {
            (ImageMode::Gray | ImageMode::GrayInvert, Image::Gray16(img)) => {
                let row_bytes = calc_row_bytes(1, bpp);
                for y in ymin..r_max_y {
                    bits.off = calc_row_off(y, row_bytes);
                    for x in xmin..r_max_x {
                        if bits.off + 2 > buf.len() {
                            return Err(ERR_NO_PIXELS);
                        }
                        let mut v = byte_order.u16(&buf[bits.off..bits.off + 2]);
                        bits.off += 2;
                        if mode == ImageMode::GrayInvert {
                            v = 0xffff - v;
                        }
                        // `SetGray16`.
                        if bounds.contains(x, y) {
                            let i = img.offset(x, y, 2);
                            img.pix[i] = (v >> 8) as u8;
                            img.pix[i + 1] = v as u8;
                        }
                    }
                }
            }
            (ImageMode::Gray | ImageMode::GrayInvert, Image::Gray(img)) => {
                let row_bytes = calc_row_bytes(1, bpp);
                let max = (1u32 << bpp) - 1;
                for y in ymin..r_max_y {
                    bits.off = calc_row_off(y, row_bytes);
                    bits.flush_bits();
                    for x in xmin..r_max_x {
                        let Some(v) = bits.read_bits(buf, bpp as u32) else {
                            return Err(ERR_NO_PIXELS);
                        };
                        let mut v = v * 0xff / max;
                        if mode == ImageMode::GrayInvert {
                            v = 0xff - v;
                        }
                        if bounds.contains(x, y) {
                            let i = img.offset(x, y, 1);
                            img.pix[i] = v as u8;
                        }
                    }
                }
            }
            (ImageMode::Paletted, Image::Paletted(img)) => {
                let p_len = palette.len();
                let row_bytes = calc_row_bytes(1, bpp);
                for y in ymin..r_max_y {
                    bits.off = calc_row_off(y, row_bytes);
                    bits.flush_bits();
                    for x in xmin..r_max_x {
                        let Some(v) = bits.read_bits(buf, bpp as u32) else {
                            return Err(ERR_NO_PIXELS);
                        };
                        let idx = v as u8;
                        if usize::from(idx) >= p_len {
                            return Err(ERR_INVALID_COLOR_INDEX);
                        }
                        if bounds.contains(x, y) {
                            let i = img.pix.offset(x, y, 1);
                            img.pix.pix[i] = idx;
                        }
                    }
                }
            }
            (ImageMode::Rgb, Image::Rgba64(img)) => {
                let row_bytes = calc_row_bytes(3, bpp);
                for y in ymin..r_max_y {
                    bits.off = calc_row_off(y, row_bytes);
                    for x in xmin..r_max_x {
                        if bits.off + 6 > buf.len() {
                            return Err(ERR_NO_PIXELS);
                        }
                        let o = bits.off;
                        let (r0, g0, b0) = (
                            byte_order.u16(&buf[o..o + 2]),
                            byte_order.u16(&buf[o + 2..o + 4]),
                            byte_order.u16(&buf[o + 4..o + 6]),
                        );
                        bits.off += 6;
                        set_rgba64(img, &bounds, x, y, [r0, g0, b0, 0xffff]);
                    }
                }
            }
            (ImageMode::Rgb, Image::Rgba(img)) => {
                let row_bytes = calc_row_bytes(3, bpp);
                for y in ymin..r_max_y {
                    bits.off = calc_row_off(y, row_bytes);
                    let min = img.offset(xmin, y, 4);
                    let max = img.offset(r_max_x, y, 4);
                    let mut off = ((y - ymin) * (xmax - xmin) * 3) as usize;
                    let mut i = min;
                    while i < max {
                        if off + 3 > buf.len() {
                            return Err(ERR_NO_PIXELS);
                        }
                        img.pix[i] = buf[off];
                        img.pix[i + 1] = buf[off + 1];
                        img.pix[i + 2] = buf[off + 2];
                        img.pix[i + 3] = 0xff;
                        off += 3;
                        i += 4;
                    }
                }
            }
            (ImageMode::Nrgba, Image::Nrgba64(img)) => {
                let row_bytes = calc_row_bytes(4, bpp);
                for y in ymin..r_max_y {
                    bits.off = calc_row_off(y, row_bytes);
                    for x in xmin..r_max_x {
                        if bits.off + 8 > buf.len() {
                            return Err(ERR_NO_PIXELS);
                        }
                        let o = bits.off;
                        let px = [
                            byte_order.u16(&buf[o..o + 2]),
                            byte_order.u16(&buf[o + 2..o + 4]),
                            byte_order.u16(&buf[o + 4..o + 6]),
                            byte_order.u16(&buf[o + 6..o + 8]),
                        ];
                        bits.off += 8;
                        set_rgba64(img, &bounds, x, y, px);
                    }
                }
            }
            (ImageMode::Nrgba, Image::Nrgba(img)) | (ImageMode::Rgba, Image::Rgba(img)) => {
                let _row_bytes = calc_row_bytes(4, bpp);
                for y in ymin..r_max_y {
                    let min = img.offset(xmin, y, 4);
                    let max = img.offset(r_max_x, y, 4);
                    let i0 = ((y - ymin) * (xmax - xmin) * 4) as usize;
                    let i1 = ((y - ymin + 1) * (xmax - xmin) * 4) as usize;
                    if i1 > buf.len() {
                        return Err(ERR_NO_PIXELS);
                    }
                    // Go's `copy` takes the shorter of the two.
                    let n = (max - min).min(i1 - i0);
                    img.pix[min..min + n].copy_from_slice(&buf[i0..i0 + n]);
                }
            }
            (ImageMode::Rgba, Image::Rgba64(img)) => {
                let row_bytes = calc_row_bytes(4, bpp);
                for y in ymin..r_max_y {
                    bits.off = calc_row_off(y, row_bytes);
                    for x in xmin..r_max_x {
                        if bits.off + 8 > buf.len() {
                            return Err(ERR_NO_PIXELS);
                        }
                        let o = bits.off;
                        let px = [
                            byte_order.u16(&buf[o..o + 2]),
                            byte_order.u16(&buf[o + 2..o + 4]),
                            byte_order.u16(&buf[o + 4..o + 6]),
                            byte_order.u16(&buf[o + 6..o + 8]),
                        ];
                        bits.off += 8;
                        set_rgba64(img, &bounds, x, y, px);
                    }
                }
            }
            // The mode decides which image `Decode` allocated, so no other pairing exists.
            _ => {}
        }
        Ok(())
    }
}

/// `SetRGBA64`/`SetNRGBA64`: the same eight bytes, big-endian, bounds checked.
fn set_rgba64(img: &mut Pixels, bounds: &Rect, x: i64, y: i64, c: [u16; 4]) {
    if !bounds.contains(x, y) {
        return;
    }
    let i = img.offset(x, y, 8);
    for (k, v) in c.iter().enumerate() {
        img.pix[i + 2 * k] = (*v >> 8) as u8;
        img.pix[i + 2 * k + 1] = *v as u8;
    }
}

/// `maxBytesPerPixel` (reader.go:517).
const MAX_BYTES_PER_PIXEL: i64 = 8;

/// Port of `newDecoder` (reader.go:519): header, IFD, dimensions and colour model.
fn new_decoder(data: &[u8]) -> Result<Decoder<'_>, Error> {
    let mut d = Decoder {
        r: Buffer::new(data),
        byte_order: ByteOrder::Little,
        width: 0,
        height: 0,
        model: ConfigModel::Gray,
        mode: ImageMode::GrayInvert,
        bpp: 0,
        features: BTreeMap::new(),
        ifd: BTreeMap::new(),
        palette: Vec::new(),
        block: Block::Owned(Vec::new()),
        bits: Bits::default(),
    };

    let mut p = [0u8; 8];
    if let (_, Some(e)) = d.r.read_at(&mut p, 0) {
        return Err(Error::Io(if e == IoError::Eof {
            IoError::UnexpectedEof
        } else {
            e
        }));
    }
    d.byte_order = if &p[0..4] == LE_HEADER {
        ByteOrder::Little
    } else if &p[0..4] == BE_HEADER {
        ByteOrder::Big
    } else {
        return Err(Error::Format("malformed header"));
    };

    let ifd_offset = u64::from(d.byte_order.u32(&p[4..8]));

    // The first two bytes contain the number of entries (12 bytes each).
    if let (_, Some(e)) = d.r.read_at(&mut p[0..2], ifd_offset) {
        return Err(Error::Io(e));
    }
    let num_items = u64::from(d.byte_order.u16(&p[0..2]));

    // All IFD entries are read in one chunk.
    let entries =
        d.r.safe_read_at(IFD_LEN as u64 * num_items, ifd_offset + 2)?;

    let mut prev_tag: i64 = -1;
    for chunk in entries.chunks_exact(IFD_LEN) {
        let tag = d.parse_ifd(chunk)?;
        if tag <= prev_tag {
            return Err(Error::Format("tags are not sorted in ascending order"));
        }
        prev_tag = tag;
    }

    d.width = d.first_int_val(T_IMAGE_WIDTH)?;
    d.height = d.first_int_val(T_IMAGE_LENGTH)?;
    if d.width == 0 || d.height == 0 {
        return Err(Error::Format("zero-size image"));
    }
    // Check that the image fits in memory, conservatively assuming 8 bytes per pixel.
    if mul3(d.width, d.height, MAX_BYTES_PER_PIXEL).is_none() {
        return Err(Error::Format("image too large"));
    }

    // Default is 1 per specification.
    d.features
        .entry(T_BITS_PER_SAMPLE)
        .or_insert_with(|| vec![1]);
    d.bpp = d.first_val(T_BITS_PER_SAMPLE);
    match d.bpp {
        0 => return Err(Error::Format("BitsPerSample must not be 0")),
        1 | 8 | 16 => {}
        other => return Err(Error::Unsupported(format!("BitsPerSample of {other}"))),
    }

    // Determine the image mode.
    let photometric = d.first_val(T_PHOTOMETRIC_INTERPRETATION);
    match photometric {
        P_RGB => {
            let want = if d.bpp == 16 { 16 } else { 8 };
            for b in d.features.get(&T_BITS_PER_SAMPLE).into_iter().flatten() {
                if *b != want {
                    return Err(Error::Format(if d.bpp == 16 {
                        "wrong number of samples for 16bit RGB"
                    } else {
                        "wrong number of samples for 8bit RGB"
                    }));
                }
            }
            // RGB images normally have three samples per pixel; a fourth is described by
            // ExtraSamples (p. 31-32), and an unspecified extra sample is not supported.
            match d.samples() {
                3 => {
                    d.mode = ImageMode::Rgb;
                    d.model = if d.bpp == 16 {
                        ConfigModel::Rgba64
                    } else {
                        ConfigModel::Rgba
                    };
                }
                4 => match d.first_val(T_EXTRA_SAMPLES) {
                    1 => {
                        d.mode = ImageMode::Rgba;
                        d.model = if d.bpp == 16 {
                            ConfigModel::Rgba64
                        } else {
                            ConfigModel::Rgba
                        };
                    }
                    2 => {
                        d.mode = ImageMode::Nrgba;
                        d.model = if d.bpp == 16 {
                            ConfigModel::Nrgba64
                        } else {
                            ConfigModel::Nrgba
                        };
                    }
                    _ => return Err(Error::Format("wrong number of samples for RGB")),
                },
                _ => return Err(Error::Format("wrong number of samples for RGB")),
            }
        }
        P_PALETTED => {
            d.mode = ImageMode::Paletted;
            d.model = ConfigModel::Palette(d.palette.clone());
        }
        P_WHITE_IS_ZERO => {
            d.mode = ImageMode::GrayInvert;
            d.model = if d.bpp == 16 {
                ConfigModel::Gray16
            } else {
                ConfigModel::Gray
            };
        }
        P_BLACK_IS_ZERO => {
            d.mode = ImageMode::Gray;
            d.model = if d.bpp == 16 {
                ConfigModel::Gray16
            } else {
                ConfigModel::Gray
            };
        }
        _ => return Err(Error::Unsupported("color model".to_owned())),
    }
    if photometric != P_RGB && d.samples() != 1 {
        return Err(Error::Unsupported("extra samples".to_owned()));
    }

    Ok(d)
}

/// Port of `tiff.DecodeConfig` (reader.go:682).
pub fn decode_config(data: &[u8]) -> Result<Config, Error> {
    let d = new_decoder(data)?;
    Ok(Config {
        width: d.width,
        height: d.height,
        model: d.model,
    })
}

/// Port of `tiff.Decode` (reader.go:699).
pub fn decode(data: &[u8]) -> Result<Image, Error> {
    let mut d = new_decoder(data)?;

    let mut block_padding = false;
    let mut block_width = d.width;
    let mut block_height = d.height;
    let mut blocks_across: i64 = 1;
    let mut blocks_down: i64 = 1;

    // `newDecoder` rejects a zero dimension, so these two never fire; they are Go's.
    if d.width == 0 {
        blocks_across = 0;
    }
    if d.height == 0 {
        blocks_down = 0;
    }

    let (block_offsets, block_counts);

    if d.first_val(T_TILE_WIDTH) != 0 {
        block_padding = true;

        block_width = d.first_int_val(T_TILE_WIDTH)?;
        block_height = d.first_int_val(T_TILE_LENGTH)?;

        // The spec says tile sizes must be a multiple of 16. Invalid sizes are permitted, but
        // anything too small would let a malicious input force unbounded work.
        if block_width < 8 || block_height < 8 {
            return Err(Error::Format("tile size is too small"));
        }
        if mul3(block_width, block_height, MAX_BYTES_PER_PIXEL).is_none() {
            return Err(Error::Format("tile size is too large"));
        }
        if block_width - d.width > 16 || block_height - d.height > 16 {
            // Tiles may be padded to the nearest multiple of 16, but a tile both larger than the
            // image and over 1024 pixels in one dimension is probably malicious.
            if block_width > 1024 || block_height > 1024 {
                return Err(Error::Format("tile size exceeds image size"));
            }
        }
        if block_width != 0 {
            blocks_across = (d.width + block_width - 1) / block_width;
        }
        if block_height != 0 {
            blocks_down = (d.height + block_height - 1) / block_height;
        }

        let n = blocks_across.saturating_mul(blocks_down);
        block_offsets = d.parse_ifd_offsets(T_TILE_OFFSETS, n)?;
        block_counts = d.parse_ifd_offsets(T_TILE_BYTE_COUNTS, n)?;
    } else {
        let v = d.first_val(T_ROWS_PER_STRIP);
        if v > 0 && v < block_height as u64 {
            block_height = v as i64;
        }

        if block_height != 0 {
            // This cannot overflow: w*h*8 does not, and blockHeight is at most the height.
            blocks_down = (d.height + block_height - 1) / block_height;
        }

        block_offsets = d.parse_ifd_offsets(T_STRIP_OFFSETS, blocks_down)?;
        block_counts = d.parse_ifd_offsets(T_STRIP_BYTE_COUNTS, blocks_down)?;
    }

    // Check we have the right number of strips/tiles, offsets and counts.
    let n = blocks_across.saturating_mul(blocks_down);
    if (block_offsets.len() as i64) < n || (block_counts.len() as i64) < n {
        return Err(Error::Format("inconsistent header"));
    }

    let img_rect = Rect::new(0, 0, d.width, d.height);
    let mut img = match d.mode {
        ImageMode::Gray | ImageMode::GrayInvert => {
            if d.bpp == 16 {
                Image::Gray16(Pixels::new(img_rect, 2))
            } else {
                Image::Gray(Pixels::new(img_rect, 1))
            }
        }
        ImageMode::Paletted => Image::Paletted(Paletted {
            pix: Pixels::new(img_rect, 1),
            palette: d.palette.clone(),
        }),
        ImageMode::Nrgba => {
            if d.bpp == 16 {
                Image::Nrgba64(Pixels::new(img_rect, 8))
            } else {
                Image::Nrgba(Pixels::new(img_rect, 4))
            }
        }
        ImageMode::Rgb | ImageMode::Rgba => {
            if d.bpp == 16 {
                Image::Rgba64(Pixels::new(img_rect, 8))
            } else {
                Image::Rgba(Pixels::new(img_rect, 4))
            }
        }
    };

    if blocks_across == 0 || blocks_down == 0 {
        return Ok(img);
    }
    // Maximum data per pixel is 8 bytes (RGBA64).
    let block_max_data_size = block_width * block_height * 8;
    let params = BlockParams {
        compression: d.first_val(T_COMPRESSION),
        photometric: d.first_val(T_PHOTOMETRIC_INTERPRETATION),
        fill_order: d.first_val(T_FILL_ORDER),
        max_data_size: block_max_data_size,
    };
    for i in 0..blocks_across {
        let mut blk_w = block_width;
        if !block_padding && i == blocks_across - 1 && d.width % block_width != 0 {
            blk_w = d.width % block_width;
        }
        for j in 0..blocks_down {
            let mut blk_h = block_height;
            if !block_padding && j == blocks_down - 1 && d.height % block_height != 0 {
                blk_h = d.height % block_height;
            }
            let k = (j * blocks_across + i) as usize;
            let offset = block_offsets[k];
            let n = block_counts[k];
            d.block = read_block(&mut d.r, &params, offset, n, blk_w, blk_h)?;

            let xmin = i * block_width;
            let ymin = j * block_height;
            let xmax = xmin + blk_w;
            let ymax = ymin + blk_h;
            d.decode_block(&mut img, xmin, ymin, xmax, ymax)?;
        }
    }
    Ok(img)
}

/// What the compression switch needs from the IFD besides the block's own bytes.
struct BlockParams {
    compression: u64,
    photometric: u64,
    fill_order: u64,
    /// `blockMaxDataSize`: `blockWidth * blockHeight * 8`, eight bytes being the most any pixel
    /// format needs.
    max_data_size: i64,
}

/// Port of `ccittFillOrder` (reader.go:690).
fn ccitt_fill_order(tiff_fill_order: u64) -> ccitt::Order {
    if tiff_fill_order == 2 {
        ccitt::Order::Lsb
    } else {
        ccitt::Order::Msb
    }
}

/// The compression switch of `Decode` (reader.go:843): one strip or tile's bytes.
fn read_block(
    r: &mut Buffer<'_>,
    params: &BlockParams,
    offset: u64,
    n: u64,
    blk_w: i64,
    blk_h: i64,
) -> Result<Block, Error> {
    let limit = offset.saturating_add(n);
    let lim = params.max_data_size;
    match params.compression {
        // The spec gives Compression no default, but some tools write none at all and mean 1.
        C_NONE | 0 => {
            if n > lim as u64 {
                return Err(Error::Format("block data size too large"));
            }
            Ok(Block::Window(r.slice(offset, n)?))
        }
        C_G3 | C_G4 => {
            let sub_format = if params.compression == C_G3 {
                ccitt::SubFormat::Group3
            } else {
                ccitt::SubFormat::Group4
            };
            let opts = ccitt::Options {
                invert: params.photometric == P_WHITE_IS_ZERO,
                align: false,
            };
            let mut z = ccitt::Reader::new(
                Section {
                    r,
                    off: offset,
                    limit,
                },
                ccitt_fill_order(params.fill_order),
                sub_format,
                blk_w,
                blk_h,
                opts,
            );
            let (buf, err) = read_buf(
                |p| {
                    let (n, e) = z.read(p);
                    (n, e.map(Stop::from))
                },
                lim,
            );
            match err {
                Some(e) => Err(e),
                None => Ok(Block::Owned(buf)),
            }
        }
        C_LZW => {
            let mut z = lzw::Reader::new(
                BufReader::new(Section {
                    r,
                    off: offset,
                    limit,
                }),
                lzw::Order::Msb,
                8,
            );
            let (buf, err) = read_buf(
                |p| {
                    let (n, e) = z.read(p);
                    (n, e.map(Stop::from))
                },
                lim,
            );
            match err {
                Some(e) => Err(e),
                None => Ok(Block::Owned(buf)),
            }
        }
        C_DEFLATE | C_DEFLATE_OLD => {
            let mut z = ZlibReader::new(BufReader::new(Section {
                r,
                off: offset,
                limit,
            }))?;
            let (buf, err) = read_buf(
                |p| {
                    let (n, e) = z.read(p);
                    (n, e.map(Stop::from))
                },
                lim,
            );
            match err {
                Some(e) => Err(e),
                None => Ok(Block::Owned(buf)),
            }
        }
        C_PACK_BITS => {
            let mut br = BufReader::new(Section {
                r,
                off: offset,
                limit,
            });
            Ok(Block::Owned(unpack_bits(&mut br, lim)?))
        }
        other => Err(Error::Unsupported(format!("compression value {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_or_wrong_header_is_go_s_error() {
        assert_eq!(decode(b"").err(), Some(Error::Io(IoError::UnexpectedEof)));
        assert_eq!(
            decode(b"II\x2a\x00\x08").err(),
            Some(Error::Io(IoError::UnexpectedEof))
        );
        assert_eq!(
            decode(b"XX\x2a\x00\x08\x00\x00\x00").err(),
            Some(Error::Format("malformed header"))
        );
        // An eight-byte file: the header reads, the IFD count does not, and `newDecoder` passes
        // that `io.EOF` through without turning it into `io.ErrUnexpectedEOF`.
        assert_eq!(
            decode(b"II\x2a\x00\x08\x00\x00\x00").err(),
            Some(Error::Io(IoError::Eof))
        );
    }

    #[test]
    fn mul3_rejects_negatives_and_overflow() {
        assert_eq!(mul3(2, 3, 4), Some(24));
        assert_eq!(mul3(-1, 3, 4), None);
        assert_eq!(mul3(i64::MAX, 2, 1), None);
        assert_eq!(mul3(1 << 40, 1 << 40, 8), None);
    }

    /// `readBits` packs big-endian and `flushBits` drops the partial byte at a row boundary.
    #[test]
    fn read_bits_takes_the_high_bits_first() {
        let mut b = Bits::default();
        let buf = [0b1011_0010u8, 0xff];
        assert_eq!(b.read_bits(&buf, 1), Some(1));
        assert_eq!(b.read_bits(&buf, 2), Some(0b01));
        assert_eq!(b.read_bits(&buf, 4), Some(0b1001));
        b.flush_bits();
        assert_eq!(b.read_bits(&buf, 8), Some(0xff));
        assert_eq!(b.read_bits(&buf, 1), None);
    }

    /// The PackBits run-length sign: a negative code repeats, a positive one copies, -128 is a
    /// no-op, and the limit is a hard error rather than a truncation.
    #[test]
    fn pack_bits_follows_the_sign_of_the_code() {
        use crate::goread::BytesReader;
        let src = [0x02u8, b'a', b'b', b'c', 0xfe, b'z', 0x80, 0x00, b'!'];
        let mut r = BytesReader::new(&src);
        assert_eq!(unpack_bits(&mut r, 1000).unwrap(), b"abczzz!");
        let mut r = BytesReader::new(&src);
        assert_eq!(
            unpack_bits(&mut r, 3).err(),
            Some(Error::Format("PackBits: decompressed data too large"))
        );
        // A literal run that runs off the end is `unexpected EOF`; a repeat code with no byte
        // after it is a plain `EOF`.
        let mut r = BytesReader::new(&[0x03u8, b'a']);
        assert_eq!(
            unpack_bits(&mut r, 1000).err(),
            Some(Error::Io(IoError::UnexpectedEof))
        );
        let mut r = BytesReader::new(&[0xfeu8]);
        assert_eq!(
            unpack_bits(&mut r, 1000).err(),
            Some(Error::Io(IoError::Eof))
        );
    }

    /// `readBuf`'s limit is a hard stop, not a bound on one read: a stream that never ends is
    /// truncated at `lim` and the reader is not called again. No geometry can observe this —
    /// `blockMaxDataSize` is the most bytes any block can need — so only a direct test can.
    #[test]
    fn read_buf_stops_dead_at_the_limit() {
        let mut calls = 0;
        let (out, err) = read_buf(
            |p| {
                calls += 1;
                p.fill(0xab);
                (p.len(), None)
            },
            700,
        );
        assert_eq!((out.len(), err.is_none()), (700, true));
        assert_eq!(calls, 2, "512 bytes then 188, and then no further read");
    }

    /// `buffer.fill`'s two EOFs, which decide half the decoder's error texts.
    #[test]
    fn the_buffer_reports_no_bytes_as_eof_and_some_bytes_as_unexpected() {
        let data = [1u8, 2, 3, 4];
        let mut b = Buffer::new(&data);
        assert_eq!(b.fill(4), None);
        assert_eq!(b.fill(5), Some(IoError::Eof));
        let mut b = Buffer::new(&data);
        assert_eq!(b.fill(6), Some(IoError::UnexpectedEof));
        // A zero-length `safeReadAt` whose `io.EOF` came from reading *nothing* is a success —
        // `io.SectionReader` returns EOF for n == 0 — while `unexpected EOF` still fails.
        let mut b = Buffer::new(&data);
        assert_eq!(b.safe_read_at(0, 4), Ok(Vec::new()));
        assert_eq!(b.safe_read_at(0, 5), Ok(Vec::new()));
        assert_eq!(b.safe_read_at(1, 5), Err(IoError::Eof));
        let mut b = Buffer::new(&data);
        assert_eq!(b.safe_read_at(0, 99), Err(IoError::UnexpectedEof));
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::{b64, describe, fixture, sha};
    use serde_json::{Value as Json, json};

    /// A ColorMap entry as the oracle's `paletteEntry` spells a `color.RGBA64`: its Go type, then
    /// the four `RGBA()` channels, which for an `RGBA64` are the stored values.
    fn palette_json(p: &[Color]) -> Vec<Json> {
        p.iter()
            .map(|c| match *c {
                Color::Rgba64([r, g, b, a]) => json!(["color.RGBA64", r, g, b, a]),
                other => panic!("a TIFF ColorMap entry is always color.RGBA64, not {other:?}"),
            })
            .collect()
    }

    fn model_json(m: &ConfigModel) -> Json {
        match m {
            ConfigModel::Gray => json!("gray"),
            ConfigModel::Gray16 => json!("gray16"),
            ConfigModel::Rgba => json!("rgba"),
            ConfigModel::Rgba64 => json!("rgba64"),
            ConfigModel::Nrgba => json!("nrgba"),
            ConfigModel::Nrgba64 => json!("nrgba64"),
            ConfigModel::Palette(p) => json!({ "palette": palette_json(p) }),
        }
    }

    /// `testsupport::describe` spells a palette entry only as `color.RGBA` or `color.NRGBA`; a
    /// TIFF ColorMap is `color.RGBA64`, which the oracle falls back to `["%T", r, g, b, a]` for.
    fn describe_tiff(m: &Image) -> Json {
        let Image::Paletted(p) = m else {
            return describe(m);
        };
        let b = m.bounds();
        json!({
            "rect": [b.min_x, b.min_y, b.max_x, b.max_y],
            "type": "paletted",
            "stride": p.pix.stride,
            "pix_sha256": sha(&p.pix.pix),
            "palette": palette_json(&p.palette),
        })
    }

    /// Every file of the oracle's TIFF corpus through `image.DecodeConfig` and `image.Decode`:
    /// the dimensions, the colour model with its palette, and the decoded image's Go type,
    /// rectangle, stride and pixel hash — or Go's error text, byte for byte.
    #[test]
    fn decode_matches_go_on_every_corpus_file() {
        let cases = fixture("tiff")["decode"].as_array().unwrap();
        let (mut checked, mut skipped) = (0, 0);
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            if !(data.starts_with(LE_HEADER) || data.starts_with(BE_HEADER)) {
                // `image.Decode`'s sniffing, not this decoder.
                assert_eq!(c["image"]["err"], "image: unknown format", "{name}");
                assert_eq!(c["config"]["err"], "image: unknown format", "{name}");
                skipped += 1;
                continue;
            }
            // `DecodeConfig` never looks at Compression, so the CCITT files are checked here too.
            let config = match decode_config(&data) {
                Ok(cfg) => json!({
                    "w": cfg.width, "h": cfg.height, "format": "tiff", "model": model_json(&cfg.model)
                }),
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(config, c["config"], "{name}: config");

            let image = match decode(&data) {
                Ok(img) => {
                    let mut d = describe_tiff(&img);
                    d["format"] = json!("tiff");
                    d
                }
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(image, c["image"], "{name}: image");
            checked += 1;
        }
        assert_eq!(skipped, 3, "files the TIFF magic does not match");
        assert!(checked > 220, "{checked}");
    }
}
