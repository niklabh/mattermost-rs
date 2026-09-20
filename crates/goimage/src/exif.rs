//! Port of the EXIF orientation walk of `github.com/bep/imagemeta` v0.17.2, for the four formats
//! `imaging.GetImageOrientation` accepts — JPEG, PNG, TIFF and WebP — as Mattermost drives it:
//! `Sources: EXIF` only, a `ShouldHandleTag` that accepts the tag named `Orientation` and nothing
//! else, and a `HandleTag` that stops the walk on the first `Orientation` whose value decoded as a
//! Go `uint16`.
//!
//! # What the walk answers, and why it is ported rather than re-derived
//!
//! The result Mattermost reads is one number: the first uint16 `Orientation` met before anything
//! makes `imagemeta.Decode` return an error. Which one is "first", and whether an error comes
//! first, is decided by details a reader of the EXIF spec would not reproduce:
//!
//! - the walk visits IFD1 and every IFD-pointer sub-IFD too (Mattermost's `ShouldHandleTag`
//!   replaces imagemeta's IFD0-only default), and a SHORT *or* SSHORT orientation counts while a
//!   LONG does not;
//! - an unknown tag type anywhere before the orientation is an error, after it is not;
//! - an already-seen IFD pointer returns *without consuming its four value bytes*
//!   (metadecoder_exif.go:376), so the rest of that IFD is read misaligned;
//! - the reader's "one silent EOF" (io.go:329): the first read past the end does not stop the
//!   walk but hands back the *previous* read's bytes, so a truncated structure can still yield a
//!   value — the port keeps a persistent buffer exactly as Go does;
//! - Go's panics are control flow: `errStop` is swallowed by the JPEG EXIF handler
//!   (imagedecoder_jpg.go:126) and by `Decode` (imagemeta.go:82), `ErrStopWalking` and a returned
//!   `io.EOF` become success. Here they are [`Flow`] values, never Rust panics;
//! - an XMP APP1 segment shares the EXIF marker, so it consumes the EXIF source
//!   (imagedecoder_jpg.go:52): an orientation in a later real EXIF segment is never read;
//! - `imagemeta.Decode` wraps its input in a 4 KiB `bufio.Reader` whose `Seek` discards the
//!   buffer (io.go:43), so a *failing* relative seek moves the logical position forward by
//!   whatever was buffered. [`Bufio`] models that, because Mattermost's non-seekable wrapper
//!   fails seeks the plain `bytes.Reader` accepts.
//!
//! # Where the four formats part company
//!
//! JPEG, PNG and WebP each copy their EXIF payload into an in-memory segment ([`Stream`] over a
//! [`BytesReader`]) before walking it, so a bad offset inside the payload cannot reach the file.
//! **TIFF does not**: `imagedecoder_tif.go:82` hands the EXIF decoder `e.streamReader` itself, so
//! the walk runs over the caller's reader through the 4 KiB [`Bufio`], inheriting its position,
//! its buffering and its one-silent-EOF state. Every value offset and sub-IFD pointer in a TIFF is
//! therefore a seek on the *file*, which is why [`Exif`] borrows its stream rather than owning a
//! `BytesReader`, and why the two reader shapes Mattermost hands in can disagree about a TIFF far
//! more readily than about a JPEG.
//!
//! Two further per-format traps:
//!
//! - TIFF never reads the next-IFD pointer: `decode()` is not called, `decodeTags("IFD0")` is, so
//!   an orientation that lives only in IFD1 is invisible even though a JPEG's would be found.
//!   `readerOffset` stays 0 for the same reason, which is correct only because a TIFF's offsets
//!   really are from the start of the file.
//! - WebP does not skip the RIFF pad byte after an odd-length chunk (`imagedecoder_webp.go:163`
//!   skips exactly `chunkLen`), so a spec-conforming odd chunk misaligns every chunk id after it.
//!   That is a bug in imagemeta and it is reproduced here.

use std::collections::HashSet;

/// `io.Seek*` whence values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whence {
    Start,
    Current,
    End,
}

/// A Go reader error, with `io.EOF` and `io.ErrUnexpectedEOF` distinguishable because the walk
/// treats them differently.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    /// `io.EOF`.
    #[error("EOF")]
    Eof,
    /// `io.ErrUnexpectedEOF`.
    #[error("unexpected EOF")]
    UnexpectedEof,
    /// Any other error, by its Go text.
    #[error("{0}")]
    Other(String),
}

/// `io.ReadSeeker`, with Go's `(n, err)` narrowed to what the readers here produce: none of them
/// returns data and an error from the same call.
pub trait ReadSeek {
    /// `Read(p)`.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ReadError>;
    /// `Seek(offset, whence)`.
    fn seek(&mut self, offset: i64, whence: Whence) -> Result<i64, ReadError>;
}

/// Port of `bytes.Reader` (bytes/reader.go): seeking past the end is allowed, reading there is
/// `io.EOF`, a negative position is an error that leaves the position where it was.
#[derive(Clone, Debug)]
pub struct BytesReader<'a> {
    s: &'a [u8],
    i: i64,
}

impl<'a> BytesReader<'a> {
    /// `bytes.NewReader(s)`.
    pub fn new(s: &'a [u8]) -> BytesReader<'a> {
        BytesReader { s, i: 0 }
    }
}

impl ReadSeek for BytesReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        let len = self.s.len() as i64;
        if self.i >= len {
            return Err(ReadError::Eof);
        }
        let start = self.i as usize;
        let n = buf.len().min(self.s.len() - start);
        buf[..n].copy_from_slice(&self.s[start..start + n]);
        self.i += n as i64;
        Ok(n)
    }

    fn seek(&mut self, offset: i64, whence: Whence) -> Result<i64, ReadError> {
        let abs = match whence {
            Whence::Start => offset,
            Whence::Current => self.i.saturating_add(offset),
            Whence::End => (self.s.len() as i64).saturating_add(offset),
        };
        if abs < 0 {
            return Err(ReadError::Other(
                "bytes.Reader.Seek: negative position".to_owned(),
            ));
        }
        self.i = abs;
        Ok(abs)
    }
}

/// Port of imagemeta's `bufferedReadSeeker` (io.go:17-53): a 4096-byte `bufio.Reader` over the
/// caller's `ReadSeeker`. `Seek` corrects a relative offset for the buffered bytes and then
/// **resets the buffer whether or not the seek succeeded**, so a failed relative seek leaves the
/// logical position at the underlying reader's, `Buffered()` bytes further on.
pub struct Bufio<'a> {
    rd: &'a mut dyn ReadSeek,
    buf: Vec<u8>,
    r: usize,
    w: usize,
    err: Option<ReadError>,
}

impl<'a> Bufio<'a> {
    /// `bufio.NewReaderSize(rs, 4096)`.
    pub fn new(rd: &'a mut dyn ReadSeek) -> Bufio<'a> {
        Bufio {
            rd,
            buf: vec![0; 4096],
            r: 0,
            w: 0,
            err: None,
        }
    }

    fn buffered(&self) -> usize {
        self.w - self.r
    }
}

impl ReadSeek for Bufio<'_> {
    /// Port of `bufio.Reader.Read` (bufio/bufio.go).
    fn read(&mut self, p: &mut [u8]) -> Result<usize, ReadError> {
        if p.is_empty() {
            if self.buffered() > 0 {
                return Ok(0);
            }
            return self.err.take().map_or(Ok(0), Err);
        }
        if self.r == self.w {
            if let Some(e) = self.err.take() {
                return Err(e);
            }
            if p.len() >= self.buf.len() {
                // Large read, empty buffer: straight from the underlying reader.
                return self.rd.read(p);
            }
            self.r = 0;
            self.w = 0;
            match self.rd.read(&mut self.buf) {
                Ok(0) => return Ok(0),
                Ok(n) => self.w = n,
                Err(e) => return Err(e),
            }
        }
        let n = p.len().min(self.buffered());
        p[..n].copy_from_slice(&self.buf[self.r..self.r + n]);
        self.r += n;
        Ok(n)
    }

    fn seek(&mut self, mut offset: i64, whence: Whence) -> Result<i64, ReadError> {
        if whence == Whence::Current {
            offset -= self.buffered() as i64;
        }
        let res = self.rd.seek(offset, whence);
        self.r = 0;
        self.w = 0;
        self.err = None;
        res
    }
}

/// Port of `io.ReadFull` (`ReadAtLeast` with `min == len(buf)`).
pub fn read_full(r: &mut dyn ReadSeek, buf: &mut [u8]) -> Result<(), ReadError> {
    let mut n = 0;
    let mut empty = 0;
    let mut err = None;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => {
                // Go loops on (0, nil); none of these readers produces it for a non-empty
                // buffer, but a reader that did would spin — stop as bufio does.
                empty += 1;
                if empty >= 100 {
                    err = Some(ReadError::Other(
                        "multiple Read calls return no data or error".to_owned(),
                    ));
                    break;
                }
            }
            Ok(k) => n += k,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    if n >= buf.len() {
        return Ok(());
    }
    match err {
        Some(ReadError::Eof) if n > 0 => Err(ReadError::UnexpectedEof),
        Some(e) => Err(e),
        None => Err(ReadError::UnexpectedEof),
    }
}

/// An error `imagemeta.Decode` returns, after its own `errFinal` filtering.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExifError {
    /// `*InvalidFormatError` (helpers.go:22): "invalid format: " + the cause.
    #[error("invalid format: {0}")]
    InvalidFormat(String),
    /// A reader error that `errFinal` did not filter.
    #[error("{0}")]
    Read(ReadError),
}

/// The image formats this walk is ported for — the four `GetImageOrientation` maps to an
/// `imagemeta.ImageFormat` (orientation.go:145-156).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
    Tiff,
    Webp,
}

/// How control leaves a Go function in the walk: a returned error, or one of the two panics.
#[derive(Debug)]
enum Flow {
    /// `panic(errStop)` from `streamReader.stop` (io.go:329).
    Stop,
    /// `ErrStopWalking`, returned by `HandleTag` or panicked by the tag-count limit.
    StopWalking,
    /// A returned error.
    Err(Failure),
}

/// A returned error, kept apart from [`ExifError`] because `errFinal` turns `io.EOF` into success
/// and wraps anything saying "unexpected EOF" as an `InvalidFormatError`.
#[derive(Debug)]
enum Failure {
    Read(ReadError),
    Invalid(String),
}

impl From<ReadError> for Flow {
    fn from(e: ReadError) -> Flow {
        Flow::Err(Failure::Read(e))
    }
}

fn invalid(msg: impl Into<String>) -> Flow {
    Flow::Err(Failure::Invalid(msg.into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    Big,
    Little,
}

impl Order {
    fn u16(self, b: &[u8]) -> u16 {
        match self {
            Order::Big => u16::from_be_bytes([b[0], b[1]]),
            Order::Little => u16::from_le_bytes([b[0], b[1]]),
        }
    }

    fn u32(self, b: &[u8]) -> u32 {
        match self {
            Order::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            Order::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    }
}

/// `maxBufSize` (io.go:134): the largest segment `bufferedReader` will read.
const MAX_BUF_SIZE: i64 = 10 * 1024 * 1024;
/// `defaultLimitNumTags` (imagemeta.go:139).
const LIMIT_NUM_TAGS: u32 = 5000;
/// `defaultLimitTagSize` (imagemeta.go:140).
const LIMIT_TAG_SIZE: u32 = 10000;

/// Port of imagemeta's `streamReader` (io.go:106): a reader plus the byte order, a *persistent*
/// scratch buffer (whose stale contents a silent EOF returns), and the EOF/error state.
struct Stream<R: ReadSeek> {
    r: R,
    order: Order,
    buf: Vec<u8>,
    is_eof: bool,
    reader_offset: i64,
}

/// A value reader for `readNFromRIntoBuf(n, r)` when `r` is not the stream's own reader.
type Alt<'a, 'b> = Option<&'a mut BytesReader<'b>>;

impl<R: ReadSeek> Stream<R> {
    fn new(r: R, order: Order) -> Stream<R> {
        Stream {
            r,
            order,
            buf: Vec::new(),
            is_eof: false,
            reader_offset: 0,
        }
    }

    /// `allocateBuf` (io.go:178): a new zeroed buffer only when it must grow.
    fn allocate(&mut self, n: usize) {
        if n > self.buf.len() {
            self.buf = vec![0; n];
        }
    }

    /// `readNFromRIntoBufE` (io.go:291).
    fn read_e(&mut self, n: usize, alt: Alt) -> Result<(), ReadError> {
        self.allocate(n);
        match alt {
            Some(a) => read_full(a, &mut self.buf[..n]),
            None => read_full(&mut self.r, &mut self.buf[..n]),
        }
    }

    /// `readNFromRIntoBuf` (io.go:285) with `stop`'s semantics (io.go:329): the first `io.EOF` is
    /// swallowed and the buffer keeps its previous bytes; anything else stops the walk.
    fn read(&mut self, n: usize, alt: Alt) -> Result<(), Flow> {
        match self.read_e(n, alt) {
            Ok(()) => Ok(()),
            Err(e) => self.stop(e),
        }
    }

    fn stop(&mut self, err: ReadError) -> Result<(), Flow> {
        if err == ReadError::Eof && !self.is_eof {
            self.is_eof = true;
            return Ok(());
        }
        // `readErr` is recorded here in Go, but only `baseStreamingDecoder.streamErr` reads it,
        // and only when no panic unwound — which every path that sets it does.
        Err(Flow::Stop)
    }

    /// `readBytes` (io.go:243): `io.ReadFull` into the *caller's* slice, not the scratch buffer,
    /// with the same one-silent-EOF stop. A short read leaves the bytes that did arrive and the
    /// rest of the caller's slice untouched — which is how WebP's chunk id can still hold the
    /// previous chunk's four bytes after a read that returned nothing.
    fn read_bytes(&mut self, b: &mut [u8]) -> Result<(), Flow> {
        match read_full(&mut self.r, b) {
            Ok(()) => Ok(()),
            Err(e) => self.stop(e),
        }
    }

    fn read1(&mut self, alt: Alt) -> Result<u8, Flow> {
        self.read(1, alt)?;
        Ok(self.buf[0])
    }

    fn read2(&mut self, alt: Alt) -> Result<u16, Flow> {
        self.read(2, alt)?;
        Ok(self.order.u16(&self.buf))
    }

    fn read4(&mut self, alt: Alt) -> Result<u32, Flow> {
        self.read(4, alt)?;
        Ok(self.order.u32(&self.buf))
    }

    /// `pos` (io.go:184): `Seek(0, SeekCurrent)`, error ignored.
    fn pos(&mut self) -> i64 {
        self.r.seek(0, Whence::Current).unwrap_or(0)
    }

    /// `seek` (io.go:318): an absolute seek whose failure goes through `stop`.
    fn seek(&mut self, pos: i64) -> Result<(), Flow> {
        match self.r.seek(pos, Whence::Start) {
            Ok(_) => Ok(()),
            Err(e) => self.stop(e),
        }
    }

    /// `skip` (io.go:325): a relative seek whose result is ignored.
    fn skip(&mut self, n: i64) {
        let _ = self.r.seek(n, Whence::Current);
    }

    /// `bufferedReader` (io.go:137): the next `length` bytes as their own in-memory reader.
    fn buffered_reader(&mut self, length: i64) -> Result<Vec<u8>, Flow> {
        if length > MAX_BUF_SIZE {
            return Err(invalid(format!(
                "length {length} exceeds max {MAX_BUF_SIZE}"
            )));
        }
        if length == 0 {
            return Ok(Vec::new());
        }
        if length < 0 {
            return Err(invalid("negative length"));
        }
        let mut b = vec![0; length as usize];
        read_full(&mut self.r, &mut b)?;
        Ok(b)
    }
}

/// The state `Decode` shares with the handlers: the tag counter of the wrapped `ShouldHandleTag`
/// and the orientation `HandleTag` captured.
struct Walk {
    tag_count: u32,
    found: Option<u16>,
}

/// `exifTypeSize` (metadecoder_exif.go:79).
fn type_size(typ: u16) -> Option<u32> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

/// The IFD pointers of `exifIFDPointers` (metadecoder_exif.go:95), by the name `seenIFDs` keys on.
fn ifd_pointer(tag: u16) -> Option<&'static str> {
    Some(match tag {
        0x014a => "SubIFD",
        0x8769 => "ExifIFDP",
        0x8825 => "GPSInfoIFD",
        0xa005 => "InteroperabilityIFD",
        _ => return None,
    })
}

/// A decoded tag value, as far as the walk distinguishes values: `HandleTag` accepts only a
/// `uint16`, and an IFD pointer follows a `uint32` or the `uint32`s of a `[]any`.
enum Value {
    U16(u16),
    U32(u32),
    List(Vec<Option<u32>>),
    Other,
}

/// Port of `metaDecoderEXIF` (metadecoder_exif.go:196). The stream is **borrowed**, not owned:
/// JPEG, PNG and WebP build one over their in-memory segment and throw it away, but TIFF passes
/// the file's own `streamReader` (`newMetaDecoderEXIFFromStreamReader`), so the walk must be able
/// to drive either.
struct Exif<'a, R: ReadSeek> {
    s: &'a mut Stream<R>,
    seen: HashSet<&'static str>,
}

impl<R: ReadSeek> Exif<'_, R> {
    /// `convertValue` for one element (metadecoder_exif.go:205-259): the reads it performs, and
    /// what kind of value comes out.
    fn convert_one(&mut self, typ: u16, alt: &mut Option<BytesReader>) -> Result<Value, Flow> {
        Ok(match typ {
            1 | 7 | 2 | 6 => {
                self.s.read1(alt.as_mut())?;
                Value::Other
            }
            3 | 8 => Value::U16(self.s.read2(alt.as_mut())?),
            4 => Value::U32(self.s.read4(alt.as_mut())?),
            5 | 10 => {
                self.s.read4(alt.as_mut())?;
                self.s.read4(alt.as_mut())?;
                Value::Other
            }
            9 | 11 => {
                self.s.read4(alt.as_mut())?;
                Value::Other
            }
            // 12: read8r.
            _ => {
                self.s.read(8, alt.as_mut())?;
                Value::Other
            }
        })
    }

    /// `convertValues` (metadecoder_exif.go:261).
    fn convert_values(
        &mut self,
        typ: u16,
        count: u32,
        len: u32,
        alt: &mut Option<BytesReader>,
    ) -> Result<Value, Flow> {
        if count == 0 {
            return Ok(Value::Other);
        }
        if typ == 2 {
            self.s.read(len as usize, alt.as_mut())?;
            return Ok(Value::Other);
        }
        if count == 1 {
            return self.convert_one(typ, alt);
        }
        let mut items = Vec::with_capacity(count as usize);
        for _ in 0..count {
            items.push(match self.convert_one(typ, alt)? {
                Value::U32(v) => Some(v),
                _ => None,
            });
        }
        // All-byte arrays become `[]byte`, which is not a `[]any`.
        if matches!(typ, 1 | 6 | 7) {
            return Ok(Value::Other);
        }
        Ok(Value::List(items))
    }

    /// Port of `decode` (metadecoder_exif.go:300).
    fn decode(&mut self, walk: &mut Walk) -> Result<(), Flow> {
        self.s.reader_offset = self.s.pos();
        match self.s.read2(None)? {
            0x4d4d => self.s.order = Order::Big,
            0x4949 => self.s.order = Order::Little,
            _ => return Ok(()),
        }
        self.s.skip(2);
        let ifd0 = self.s.read4(None)?;
        if ifd0 < 8 {
            return Ok(());
        }
        self.s.skip(i64::from(ifd0 - 8));
        self.decode_tags(walk)?;
        let ifd1 = self.s.read4(None)?;
        if ifd1 == 0 {
            return Ok(());
        }
        let to = i64::from(ifd1) + self.s.reader_offset;
        self.s.seek(to)?;
        self.decode_tags(walk)
    }

    /// Port of `decodeTags` (metadecoder_exif.go:528).
    fn decode_tags(&mut self, walk: &mut Walk) -> Result<(), Flow> {
        let n = self.s.read2(None)?;
        for _ in 0..n {
            self.decode_tag(walk)?;
        }
        Ok(())
    }

    /// Port of `decodeTagsAt` (metadecoder_exif.go:540) through `preservePos` (io.go:311): the
    /// position is restored after a returned error too, but not after a panic.
    fn decode_tags_at(&mut self, walk: &mut Walk, offset: i64) -> Result<(), Flow> {
        let pos = self.s.pos();
        let res = self
            .s
            .seek(offset + self.s.reader_offset)
            .and_then(|()| self.decode_tags(walk));
        match res {
            Err(Flow::Stop) | Err(Flow::StopWalking) => res,
            _ => {
                self.s.seek(pos)?;
                res
            }
        }
    }

    /// Port of `decodeTag` (metadecoder_exif.go:349).
    fn decode_tag(&mut self, walk: &mut Walk) -> Result<(), Flow> {
        let tag = self.s.read2(None)?;
        let typ = self.s.read2(None)?;
        let count = self.s.read4(None)?;
        if count > 0x10000 {
            self.s.skip(4);
            return Ok(());
        }
        let pointer = ifd_pointer(tag);
        if let Some(name) = pointer {
            // Returns without consuming the value's four bytes.
            if !self.seen.insert(name) {
                return Ok(());
            }
        }
        let Some(size) = type_size(typ) else {
            return Err(invalid(format!("unknown EXIF type {typ}")));
        };
        let val_len = size * count;
        // XMP (0x02bc) and IPTC (0x83bb) are not requested sources; nor is anything too large.
        if tag == 0x02bc || tag == 0x83bb || val_len > LIMIT_TAG_SIZE {
            self.s.skip(4);
            return Ok(());
        }
        if pointer.is_none() {
            // The wrapped ShouldHandleTag (imagemeta.go:145): count, then panic past the limit.
            walk.tag_count += 1;
            if walk.tag_count > LIMIT_NUM_TAGS {
                return Err(Flow::StopWalking);
            }
            if tag != 0x0112 {
                self.s.skip(4);
                return Ok(());
            }
        }

        let val = if val_len > 4 {
            let value_offset = self.s.read4(None)?;
            let offset = value_offset.wrapping_add(self.s.reader_offset as u32);
            let old = self.s.pos();
            let res = self
                .s
                .seek(i64::from(offset))
                .and_then(|()| self.s.buffered_reader(i64::from(val_len)))
                .and_then(|seg| {
                    let mut alt = Some(BytesReader::new(&seg));
                    self.convert_values(typ, count, val_len, &mut alt)
                });
            // `defer e.seek(oldPos)`: runs on every exit, including a panic, and a failure of
            // *this* seek replaces whatever `res` holds. Over a segment it cannot fail; over a
            // TIFF's own file reader (or Mattermost's forward-only wrapper) it can.
            self.s.seek(old)?;
            res?
        } else {
            let v = self.convert_values(typ, count, val_len, &mut None)?;
            let padding = 4 - val_len;
            if padding > 0 {
                self.s.skip(i64::from(padding));
            }
            v
        };

        if pointer.is_some() {
            return match val {
                Value::U32(v) => self.decode_tags_at(walk, i64::from(v)),
                Value::List(items) => {
                    for off in items.into_iter().flatten() {
                        self.decode_tags_at(walk, i64::from(off))?;
                    }
                    Ok(())
                }
                _ => Err(invalid("invalid IFD pointer value")),
            };
        }
        // HandleTag: only a uint16 orientation stops the walk.
        if let Value::U16(v) = val {
            walk.found = Some(v);
            return Err(Flow::StopWalking);
        }
        Ok(())
    }
}

/// Runs the EXIF decoder over one in-memory segment, as `newMetaDecoderEXIF` does: a *fresh*
/// `streamReader` with its own `isEOF`, started in the calling decoder's byte order (which only
/// the "Exif" header read can observe, since `decode` resets the order from the TIFF marker).
///
/// `header` is JPEG's alone: PNG's `eXIf` chunk and WebP's `EXIF` chunk hold a bare TIFF
/// structure, so an "Exif\0\0" prefix there is read as a byte-order marker and rejected.
///
/// `order` is carried for fidelity and is observable from **one** call site only. The single read
/// that happens before `decode` resets the order from the TIFF marker is JPEG's `read4` of
/// "Exif", and JPEG passes big-endian; the marker itself reads the same either way, and WebP's
/// little-endian never reaches anything else. So mutating this to `Order::Big` is an equivalent
/// mutant — no input can tell — while mutating it to `Order::Little` is caught by every JPEG case.
fn decode_segment(seg: &[u8], walk: &mut Walk, header: bool, order: Order) -> Result<(), Flow> {
    let mut s = Stream::new(BytesReader::new(seg), order);
    let mut e = Exif {
        s: &mut s,
        seen: HashSet::new(),
    };
    if header {
        // handleEXIF (imagedecoder_jpg.go:139): the "Exif" header, then two bytes skipped.
        if e.s.read4(None)? != 0x4578_6966 {
            return Ok(());
        }
        e.s.skip(2);
    }
    e.decode(walk)
}

/// Port of `imageDecoderJPEG.decode` (imagedecoder_jpg.go:15) with only the EXIF source.
fn decode_jpeg(e: &mut Stream<Bufio>, walk: &mut Walk) -> Result<(), Flow> {
    // read2E: an error here is a clean nil.
    if e.read_e(2, None).is_err() {
        return Ok(());
    }
    if e.order.u16(&e.buf) != 0xffd8 {
        return Ok(());
    }
    loop {
        let marker = e.read2(None)?;
        if e.is_eof {
            return Ok(());
        }
        if marker == 0 {
            continue;
        }
        if marker == 0xffda {
            return Ok(());
        }
        let length = e.read2(None)?;
        if length < 2 {
            return Err(invalid("invalid format"));
        }
        let length = length - 2;
        if marker == 0xffe1 {
            // handleEXIF (imagedecoder_jpg.go:126). The EXIF source is spent either way, and the
            // only requested source, so the loop ends after it.
            let _thumbnail_offset = e.pos();
            let seg = e.buffered_reader(i64::from(length))?;
            return match decode_segment(&seg, walk, true, e.order) {
                // recover(): errStop is swallowed.
                Err(Flow::Stop) => Ok(()),
                other => other,
            };
        }
        e.skip(i64::from(length));
    }
}

/// Port of `imageDecoderPNG.decode` (imagedecoder_png.go:27) with only the EXIF source.
fn decode_png(e: &mut Stream<Bufio>, walk: &mut Walk) -> Result<(), Flow> {
    e.skip(8);
    loop {
        let chunk_length = e.read4(None)?;
        e.read(4, None)?;
        let tag = [e.buf[0], e.buf[1], e.buf[2], e.buf[3]];
        if &tag == b"eXIf" {
            let seg = e.buffered_reader(i64::from(chunk_length))?;
            decode_segment(&seg, walk, false, e.order)?;
            // e.skip(4) for the CRC; EXIF was the only source, so the walk is done.
            return Ok(());
        } else if &tag == b"zTXt" {
            // readNullTerminatedBytes(79 + 1): the profile name is read either way; none of its
            // branches reads more with IPTC not requested.
            let mut n: i64 = 0;
            for _ in 0..80 {
                let b = e.read1(None)?;
                n += 1;
                if b == 0 {
                    break;
                }
            }
            e.skip(i64::from(chunk_length) - n);
            e.skip(4);
        } else {
            e.skip(i64::from(chunk_length));
            e.skip(4);
        }
    }
}

/// Port of `imageDecoderTIF.decode` (imagedecoder_tif.go:14) with only the EXIF source.
///
/// Three differences from every other format, each of which changes an answer:
///
/// 1. a header this decoder dislikes is `errInvalidFormat`, where `metaDecoderEXIF.decode`
///    returns success — so a TIFF with a bad byte-order marker, a magic that is not 42 or an IFD
///    offset below 8 is an *error*, not "no orientation";
/// 2. the whole `CONFIG` block (imagedecoder_tif.go:41-80) is dead under `Sources: EXIF`. It is
///    skipped in Go too, and its `e.seek(ifdPos)` undoes the scan, so omitting it moves nothing;
/// 3. `decodeTags` is called directly, on the file's own stream. No next-IFD pointer is read
///    (IFD1 is unreachable) and `readerOffset` stays 0.
fn decode_tiff(e: &mut Stream<Bufio>, walk: &mut Walk) -> Result<(), Flow> {
    match e.read2(None)? {
        0x4d4d => e.order = Order::Big,
        0x4949 => e.order = Order::Little,
        _ => return Err(invalid("invalid format")),
    }
    // `meaningOfLife`.
    if e.read2(None)? != 42 {
        return Err(invalid("invalid format"));
    }
    let ifd_offset = e.read4(None)?;
    if ifd_offset < 8 {
        return Err(invalid("invalid format"));
    }
    e.skip(i64::from(ifd_offset - 8));
    let mut dec = Exif {
        s: e,
        seen: HashSet::new(),
    };
    dec.decode_tags(walk)
}

/// `RIFF` container fourCCs (imagedecoder_webp.go:6).
const FCC_RIFF: &[u8; 4] = b"RIFF";
const FCC_WEBP: &[u8; 4] = b"WEBP";
const FCC_VP8X: &[u8; 4] = b"VP8X";
const FCC_EXIF: &[u8; 4] = b"EXIF";
/// `exifMetadataBit` in a `VP8X` chunk's first flags byte (imagedecoder_webp.go:70).
const VP8X_EXIF_BIT: u8 = 1 << 3;

/// Port of `decoderWebP.decode` (imagedecoder_webp.go:21) with only the EXIF source.
///
/// `sourceSet` starts as `(EXIF|XMP|CONFIG) & EXIF` = EXIF, so the `XMP `, `VP8 ` and `VP8L` arms
/// are all dead and their chunks fall to `default`. That is position-equivalent — each of those
/// arms consumes exactly `chunkLen` bytes as well — so they are not reproduced. `VP8X` is *not*
/// dead: it is matched on the chunk id alone, its length is checked against 10, its ten bytes are
/// read, and its EXIF flag can clear the last source and end the walk before any `EXIF` chunk is
/// reached.
///
/// The byte order is little-endian throughout (`base.byteOrder` is set for WebP in
/// imagemeta.go:216), which is what makes `chunkLen` read correctly.
fn decode_webp(e: &mut Stream<Bufio>, walk: &mut Walk) -> Result<(), Flow> {
    // Go declares these once, outside the loop, so a short read leaves the previous chunk's bytes.
    let mut buf = [0u8; 10];
    let mut chunk_id = [0u8; 4];
    let mut want_exif = true;

    e.read_bytes(&mut chunk_id)?;
    if &chunk_id != FCC_RIFF {
        return Err(invalid("invalid format"));
    }
    // The RIFF file size is skipped, never checked: a size that lies about the file changes
    // nothing.
    e.skip(4);
    e.read_bytes(&mut chunk_id)?;
    if &chunk_id != FCC_WEBP {
        return Err(invalid("invalid format"));
    }
    loop {
        if !want_exif {
            return Ok(());
        }
        e.read_bytes(&mut chunk_id)?;
        if e.is_eof {
            return Ok(());
        }
        let chunk_len = e.read4(None)?;
        if &chunk_id == FCC_VP8X {
            if chunk_len != 10 {
                return Err(invalid("invalid format"));
            }
            e.read_bytes(&mut buf)?;
            if buf[0] & VP8X_EXIF_BIT == 0 {
                // `sourceSet.Remove(EXIF)` empties the only source, and the arm's closing
                // `if sourceSet.IsZero()` returns: an EXIF chunk after this is never read.
                return Ok(());
            }
        } else if &chunk_id == FCC_EXIF && want_exif {
            want_exif = false;
            let _thumbnail_offset = e.pos();
            // No RIFF pad byte is consumed after this, so an odd chunkLen misaligns the rest.
            let seg = e.buffered_reader(i64::from(chunk_len))?;
            decode_segment(&seg, walk, false, e.order)?;
        } else {
            e.skip(i64::from(chunk_len));
        }
    }
}

/// Port of `imagemeta.Decode` (imagemeta.go:82) with `Sources: EXIF`, Mattermost's
/// `ShouldHandleTag` (the tag named `Orientation`) and its `HandleTag` (stop on the first
/// `uint16`): the first uint16 `Orientation` the walk meets, `None` when it meets none, or the
/// error `Decode` returns.
///
/// `errFinal`'s filtering is applied: `ErrStopWalking`, `errStop` and a returned `io.EOF` are
/// success.
pub fn decode_orientation(r: &mut dyn ReadSeek, format: Format) -> Result<Option<u16>, ExifError> {
    // `base.byteOrder` is big-endian for every format but WebP (imagemeta.go:216).
    let order = match format {
        Format::Webp => Order::Little,
        _ => Order::Big,
    };
    let mut stream = Stream::new(Bufio::new(r), order);
    let mut walk = Walk {
        tag_count: 0,
        found: None,
    };
    let res = match format {
        Format::Jpeg => decode_jpeg(&mut stream, &mut walk),
        Format::Png => decode_png(&mut stream, &mut walk),
        Format::Tiff => decode_tiff(&mut stream, &mut walk),
        Format::Webp => decode_webp(&mut stream, &mut walk),
    };
    match res {
        Ok(()) | Err(Flow::Stop) | Err(Flow::StopWalking) => Ok(walk.found),
        Err(Flow::Err(Failure::Read(ReadError::Eof))) => Ok(walk.found),
        Err(Flow::Err(Failure::Read(e))) => {
            if e.to_string().contains("unexpected EOF") {
                Err(ExifError::InvalidFormat(e.to_string()))
            } else {
                Err(ExifError::Read(e))
            }
        }
        Err(Flow::Err(Failure::Invalid(m))) => Err(ExifError::InvalidFormat(m)),
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::fixture;

    fn format_of(f: &str) -> Option<Format> {
        match f.strip_prefix("image/").unwrap_or(f) {
            "jpeg" => Some(Format::Jpeg),
            "png" => Some(Format::Png),
            "tiff" => Some(Format::Tiff),
            "webp" => Some(Format::Webp),
            _ => None,
        }
    }

    /// The case's bytes. `tiff_pad` is the recipe the oracle carries in place of a payload too
    /// large to base64: that many zero bytes between the 8-byte header and IFD0, with the
    /// header's little-endian IFD0 offset raised to match. (`pad`, the PNG recipe, is applied by
    /// [`crate::testsupport::exif_case_bytes`].)
    fn case_bytes(c: &serde_json::Value) -> Vec<u8> {
        let data = crate::testsupport::exif_case_bytes(c);
        let Some(pad) = c["tiff_pad"].as_u64() else {
            return data;
        };
        let mut out = data[..4].to_vec();
        out.extend_from_slice(&(8 + pad as u32).to_le_bytes());
        out.resize(8 + pad as usize, 0);
        out.extend_from_slice(&data[8..]);
        out
    }

    /// Every case of the oracle whose format this walk is ported for, through a `bytes.Reader`
    /// (Mattermost's seekable mode): Go answers orientation `o` with no error exactly when this
    /// returns `Ok(Some(o))` (or `Ok(None)` for 1), and 1 with an error when this returns `Err`.
    #[test]
    fn every_case_matches_imagemeta() {
        let mut n = 0;
        for c in fixture("exif")["cases"].as_array().unwrap() {
            let Some(format) = format_of(c["format"].as_str().unwrap()) else {
                continue;
            };
            let data = case_bytes(c);
            let got = decode_orientation(&mut BytesReader::new(&data), format);
            let (o, err) = match got {
                Ok(v) => (i64::from(v.unwrap_or(1)), false),
                Err(_) => (1, true),
            };
            let want = &c["seeker"];
            assert_eq!(
                (o, err),
                (
                    want["orientation"].as_i64().unwrap(),
                    want["err"].as_bool().unwrap()
                ),
                "{} ({got:?})",
                c["name"]
            );
            n += 1;
        }
        assert!(n > 200, "{n}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_reader_seeks_past_the_end_but_not_before_the_start() {
        let mut r = BytesReader::new(b"abc");
        assert_eq!(r.seek(10, Whence::Start), Ok(10));
        assert_eq!(r.read(&mut [0; 2]), Err(ReadError::Eof));
        assert!(r.seek(-11, Whence::Current).is_err());
        assert_eq!(r.seek(0, Whence::Current), Ok(10));
        assert_eq!(r.seek(-1, Whence::End), Ok(2));
    }

    #[test]
    fn read_full_distinguishes_eof_from_a_short_read() {
        let mut buf = [0; 4];
        assert_eq!(
            read_full(&mut BytesReader::new(b""), &mut buf),
            Err(ReadError::Eof)
        );
        assert_eq!(
            read_full(&mut BytesReader::new(b"ab"), &mut buf),
            Err(ReadError::UnexpectedEof)
        );
        assert_eq!(read_full(&mut BytesReader::new(b"abcd"), &mut buf), Ok(()));
    }

    /// A failing relative seek through the 4 KiB bufio layer discards what was buffered: the
    /// logical position jumps to the underlying reader's.
    #[test]
    fn a_failed_relative_seek_drops_the_bufio_buffer() {
        let data: Vec<u8> = (0..=255u8).cycle().take(10000).collect();
        let mut inner = BytesReader::new(&data);
        let mut b = Bufio::new(&mut inner);
        let mut one = [0u8; 1];
        b.read(&mut one).unwrap();
        assert_eq!(b.seek(0, Whence::Current), Ok(1));
        b.read(&mut one).unwrap();
        // Buffered 4095 bytes; -5000 relative is negative underneath: fails, buffer dropped.
        assert!(b.seek(-5000, Whence::Current).is_err());
        b.read(&mut one).unwrap();
        assert_eq!(one[0], data[4097]);
    }
}
