//! Port of Go's GIF decoder (image/gif/reader.go, go1.26.4): `gif.Decode`, `gif.DecodeConfig` and
//! `gif.DecodeAll` as `image.Decode`/`image.DecodeConfig` and Mattermost's emoji resize path call
//! them.
//!
//! # Only the reader
//!
//! `image/gif`'s encoder and the quantiser it uses are not here. `image.Decode` yields the first
//! frame; the animated path walks every frame, so [`decode_all`] exists even though nothing
//! reachable through `image.Decode` returns more than one.
//!
//! # The reader chain is Go's, layer by layer
//!
//! A `bytes.Reader` already has `ReadByte`, so unlike the PNG decoder this one puts no
//! `bufio.Reader` in front of its input — Go's `decode` only adds one when the source lacks
//! `ReadByte`. Below that sits [`BlockReader`], Go's `blockReader`: it unpicks the (length, bytes)
//! sub-block framing so the LZW reader never sees it, and how much of the last sub-block the LZW
//! reader happened to consume is what decides whether trailing bytes are tolerated or are "gif:
//! too much image data".
//!
//! # Allocation
//!
//! Like Go, a frame is allocated from its image descriptor before a single pixel is read, and the
//! only bound on that is the descriptor's own 16-bit fields and the logical screen it must fit
//! inside — at most 65535×65535 bytes, exactly as in Go. The caller enforces a resolution limit
//! first, as Mattermost's `imaging.Decoder` does via `DecodeConfig`.

use super::lzw;
use crate::goread::{ByteRead, BytesReader, Error as IoError, Read, read_full, read_full_err};
use crate::image::{Color, Image, Paletted, Pixels, Rect};

// Fields (reader.go:33).
const F_COLOR_TABLE: u8 = 1 << 7;
const F_INTERLACE: u8 = 1 << 6;
const F_COLOR_TABLE_BITS_MASK: u8 = 7;

// Graphic control flags.
const GC_TRANSPARENT_COLOR_SET: u8 = 1 << 0;
const GC_DISPOSAL_METHOD_MASK: u8 = 7 << 2;

/// `DisposalNone` (reader.go:46).
pub const DISPOSAL_NONE: u8 = 0x01;
/// `DisposalBackground`.
pub const DISPOSAL_BACKGROUND: u8 = 0x02;
/// `DisposalPrevious`.
pub const DISPOSAL_PREVIOUS: u8 = 0x03;

// Section indicators (reader.go:52).
const S_EXTENSION: u8 = 0x21;
const S_IMAGE_DESCRIPTOR: u8 = 0x2C;
const S_TRAILER: u8 = 0x3B;

// Extensions (reader.go:59).
const E_TEXT: u8 = 0x01;
const E_GRAPHIC_CONTROL: u8 = 0xF9;
const E_COMMENT: u8 = 0xFE;
const E_APPLICATION: u8 = 0xFF;

/// `interlacing` (reader.go:538): (skip, start) per pass.
const INTERLACING: [(usize, usize); 4] = [(8, 0), (8, 4), (4, 2), (2, 1)];

/// Every error `image/gif`'s decoder can produce, rendered with Go's text byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `fmt.Errorf("gif: reading header: %v", err)` (reader.go:272).
    #[error("gif: reading header: {0}")]
    ReadingHeader(IoError),
    /// `fmt.Errorf("gif: can't recognize format %q", d.vers)` (reader.go:276). The payload is
    /// already quoted — see [`go_quote`].
    #[error("gif: can't recognize format {0}")]
    UnrecognizedFormat(String),
    /// `fmt.Errorf("gif: reading color table: %s", err)` (reader.go:295).
    #[error("gif: reading color table: {0}")]
    ReadingColorTable(IoError),
    /// `fmt.Errorf("gif: reading frames: %v", err)` (reader.go:240).
    #[error("gif: reading frames: {0}")]
    ReadingFrames(IoError),
    /// `fmt.Errorf("gif: reading extension: %v", err)` (reader.go:308, :321, :339, :351).
    #[error("gif: reading extension: {0}")]
    ReadingExtension(IoError),
    /// `fmt.Errorf("gif: unknown extension 0x%.2x", extension)` (reader.go:326).
    #[error("gif: unknown extension 0x{0:02x}")]
    UnknownExtension(u8),
    /// `fmt.Errorf("gif: can't read graphic control: %s", err)` (reader.go:361).
    #[error("gif: can't read graphic control: {0}")]
    ReadingGraphicControl(IoError),
    /// `fmt.Errorf("gif: invalid graphic control extension block size: %d", …)` (reader.go:364).
    #[error("gif: invalid graphic control extension block size: {0}")]
    BadGraphicControlSize(u8),
    /// `fmt.Errorf("gif: invalid graphic control extension block terminator: %d", …)`
    /// (reader.go:374).
    #[error("gif: invalid graphic control extension block terminator: {0}")]
    BadGraphicControlTerminator(u8),
    /// `errors.New("gif: no color table")` (reader.go:392).
    #[error("gif: no color table")]
    NoColorTable,
    /// `fmt.Errorf("gif: can't read image descriptor: %s", err)` (reader.go:488).
    #[error("gif: can't read image descriptor: {0}")]
    ReadingImageDescriptor(IoError),
    /// `errors.New("gif: frame bounds larger than image bounds")` (reader.go:513). Older Go
    /// releases spelled this "gif: image block is out of bounds"; go1.26.4 does not.
    #[error("gif: frame bounds larger than image bounds")]
    FrameBounds,
    /// `fmt.Errorf("gif: pixel size in decode out of range: %d", litWidth)` (reader.go:421).
    #[error("gif: pixel size in decode out of range: {0}")]
    LitWidthOutOfRange(u8),
    /// `fmt.Errorf("gif: reading image data: %v", err)` (reader.go:418, :429, :446, :456).
    #[error("gif: reading image data: {0}")]
    ReadingImageData(lzw::Error),
    /// `errNotEnough` (reader.go:21).
    #[error("gif: not enough image data")]
    NotEnough,
    /// `errTooMuch` (reader.go:22).
    #[error("gif: too much image data")]
    TooMuch,
    /// `errBadPixel` (reader.go:23).
    #[error("gif: invalid pixel value")]
    BadPixel,
    /// `fmt.Errorf("gif: missing image data")` (reader.go:259).
    #[error("gif: missing image data")]
    MissingImageData,
    /// `fmt.Errorf("gif: unknown block type: 0x%.2x", c)` (reader.go:264).
    #[error("gif: unknown block type: 0x{0:02x}")]
    UnknownBlockType(u8),
}

/// `image.Config` for a GIF: the global colour table is the colour model, and an absent global
/// table is Go's nil `color.Palette` — which is an empty one, since `readColorTable` never returns
/// fewer than two entries.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Config {
    pub width: i64,
    pub height: i64,
    pub model: Vec<Color>,
}

/// Port of `gif.GIF` (reader.go:574).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Gif {
    /// The successive frames.
    pub image: Vec<Paletted>,
    /// The successive delay times, one per frame, in 100ths of a second.
    pub delay: Vec<i64>,
    /// 0 loops forever, -1 shows each frame once, otherwise the animation runs `LoopCount+1`
    /// times. A file with no NETSCAPE loop extension gets -1.
    pub loop_count: i64,
    /// The successive disposal methods, one per frame.
    pub disposal: Vec<u8>,
    /// The global colour table, width and height.
    pub config: Config,
    /// The background index in the global colour table.
    pub background_index: u8,
}

/// Port of `readByte` (reader.go:74): `io.EOF` becomes `io.ErrUnexpectedEOF`.
fn read_byte<R: ByteRead>(r: &mut R) -> Result<u8, IoError> {
    r.read_byte().map_err(|e| {
        if e == IoError::Eof {
            IoError::UnexpectedEof
        } else {
            e
        }
    })
}

/// Port of `readFull` (reader.go:66).
fn read_full_gif<R: Read>(r: &mut R, b: &mut [u8]) -> Result<(), IoError> {
    read_full_err(r, b).map_err(|e| {
        if e == IoError::Eof {
            IoError::UnexpectedEof
        } else {
            e
        }
    })
}

/// `strconv.Quote` over the six version bytes, which is what `%q` reaches for.
///
/// Go quotes a rune it cannot decode as `\xNN`, and a decodable non-ASCII rune by
/// `unicode.IsPrint`. Every input that reaches this function through `image.Decode` has the shape
/// `GIF8?a` — the registry's magic — so the one free byte is either ASCII or a lone byte that is
/// never valid UTF-8, and `\xNN` is then exactly Go's answer. A caller that reaches
/// [`decode_all`] directly with a *valid* multi-byte rune in those six bytes would get `\xNN` per
/// byte where Go prints the rune or `\uNNNN`; porting `unicode.IsPrint` for that case would mean
/// depending on the generated tables in `mm-model`, which this crate must not do.
fn go_quote(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() + 2);
    out.push('"');
    for &c in b {
        match c {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(char::from(c)),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            _ => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push('"');
    out
}

/// Why [`BlockReader::close`] refused. Go returns one `error`; the caller distinguishes
/// `errTooMuch` from everything else, so the two are separate here.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CloseError {
    TooMuch,
    Io(IoError),
}

/// Port of `blockReader` (reader.go:120): the (n, n bytes) sub-block framing, undone.
///
/// Go buffers into the decoder's shared `tmp`; a 256-byte buffer of its own is indistinguishable,
/// because nothing the decoder wrote into `tmp` before the frame's data is read again after it.
struct BlockReader<'r, 'a> {
    r: &'r mut BytesReader<'a>,
    tmp: [u8; 256],
    i: u8,
    j: u8,
    err: Option<IoError>,
}

impl<'r, 'a> BlockReader<'r, 'a> {
    fn new(r: &'r mut BytesReader<'a>) -> BlockReader<'r, 'a> {
        BlockReader {
            r,
            tmp: [0; 256],
            i: 0,
            j: 0,
            err: None,
        }
    }

    /// Port of `blockReader.fill` (reader.go:126).
    fn fill(&mut self) {
        if self.err.is_some() {
            return;
        }
        match read_byte(self.r) {
            Ok(j) => {
                self.j = j;
                if j == 0 {
                    // A zero-length sub-block is the terminator: a clean end, not a failure.
                    self.err = Some(IoError::Eof);
                    return;
                }
            }
            Err(e) => {
                self.j = 0;
                self.err = Some(e);
                return;
            }
        }
        self.i = 0;
        let n = usize::from(self.j);
        if let (_, Some(e)) = read_full(self.r, &mut self.tmp[..n]) {
            // `readFull` here is gif's, so io.EOF is already io.ErrUnexpectedEOF.
            self.err = Some(if e == IoError::Eof {
                IoError::UnexpectedEof
            } else {
                e
            });
            self.j = 0;
        }
    }

    /// Port of `blockReader.close` (reader.go:184): at most one trailing sub-block of one byte is
    /// tolerated after the LZW data (golang.org/issue/16146).
    fn close(&mut self) -> Result<(), CloseError> {
        match &self.err {
            Some(IoError::Eof) => return Ok(()),
            Some(e) => return Err(CloseError::Io(e.clone())),
            None => {}
        }
        if self.i == self.j {
            // The LZW data ended on a sub-block boundary: allow one more sub-block of one byte.
            self.fill();
            match &self.err {
                Some(IoError::Eof) => return Ok(()),
                Some(e) => return Err(CloseError::Io(e.clone())),
                None if self.j > 1 => return Err(CloseError::TooMuch),
                None => {}
            }
        }
        // Part of a sub-block remains buffered; the next one must be the terminator.
        self.fill();
        match &self.err {
            Some(IoError::Eof) => Ok(()),
            Some(e) => Err(CloseError::Io(e.clone())),
            None => Err(CloseError::TooMuch),
        }
    }
}

impl Read for BlockReader<'_, '_> {
    /// Port of `blockReader.Read` (reader.go:160). `compress/lzw` only ever calls `ReadByte`, so
    /// in practice this is unreachable; it exists because Go's type needs `io.Reader`.
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<IoError>) {
        if p.is_empty() || self.err.is_some() {
            return (0, self.err.clone());
        }
        if self.i == self.j {
            self.fill();
            if let Some(e) = self.err.clone() {
                return (0, Some(e));
            }
        }
        let (i, j) = (usize::from(self.i), usize::from(self.j));
        let n = p.len().min(j - i);
        p[..n].copy_from_slice(&self.tmp[i..i + n]);
        self.i += n as u8;
        (n, None)
    }
}

impl ByteRead for BlockReader<'_, '_> {
    /// Port of `blockReader.ReadByte` (reader.go:145).
    fn read_byte(&mut self) -> Result<u8, IoError> {
        if self.i == self.j {
            self.fill();
            if let Some(e) = self.err.clone() {
                return Err(e);
            }
        }
        let c = self.tmp[usize::from(self.i)];
        self.i += 1;
        Ok(c)
    }
}

/// Port of `decoder` (reader.go:83).
struct Decoder<'a> {
    r: BytesReader<'a>,

    width: i64,
    height: i64,
    loop_count: i64,
    delay_time: i64,
    background_index: u8,
    disposal_method: u8,

    image_fields: u8,

    transparent_index: u8,
    has_transparent_index: bool,

    /// `d.globalColorTable`; `None` is Go's nil slice, which `readImageDescriptor` tests for.
    global_color_table: Option<Vec<Color>>,

    delay: Vec<i64>,
    disposal: Vec<u8>,
    image: Vec<Paletted>,
    tmp: [u8; 1024],
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Decoder<'a> {
        Decoder {
            r: BytesReader::new(data),
            width: 0,
            height: 0,
            loop_count: 0,
            delay_time: 0,
            background_index: 0,
            disposal_method: 0,
            image_fields: 0,
            transparent_index: 0,
            has_transparent_index: false,
            global_color_table: None,
            delay: Vec::new(),
            disposal: Vec::new(),
            image: Vec::new(),
            tmp: [0; 1024],
        }
    }

    /// Port of `decoder.decode` (reader.go:219).
    fn decode(&mut self, config_only: bool, keep_all_frames: bool) -> Result<(), Error> {
        self.loop_count = -1;
        self.read_header_and_screen_descriptor()?;
        if config_only {
            return Ok(());
        }
        loop {
            let c = read_byte(&mut self.r).map_err(Error::ReadingFrames)?;
            match c {
                S_EXTENSION => self.read_extension()?,
                S_IMAGE_DESCRIPTOR => {
                    self.read_image_descriptor(keep_all_frames)?;
                    if !keep_all_frames && self.image.len() == 1 {
                        return Ok(());
                    }
                }
                S_TRAILER => {
                    if self.image.is_empty() {
                        return Err(Error::MissingImageData);
                    }
                    return Ok(());
                }
                other => return Err(Error::UnknownBlockType(other)),
            }
        }
    }

    /// Port of `readHeaderAndScreenDescriptor` (reader.go:269).
    fn read_header_and_screen_descriptor(&mut self) -> Result<(), Error> {
        read_full_gif(&mut self.r, &mut self.tmp[..13]).map_err(Error::ReadingHeader)?;
        let vers = &self.tmp[..6];
        if vers != b"GIF87a" && vers != b"GIF89a" {
            return Err(Error::UnrecognizedFormat(go_quote(vers)));
        }
        self.width = i64::from(self.tmp[6]) + (i64::from(self.tmp[7]) << 8);
        self.height = i64::from(self.tmp[8]) + (i64::from(self.tmp[9]) << 8);
        let fields = self.tmp[10];
        if fields & F_COLOR_TABLE != 0 {
            self.background_index = self.tmp[11];
            // readColorTable overwrites tmp, but the background index is already out.
            self.global_color_table = Some(self.read_color_table(fields)?);
        }
        // tmp[12] is the pixel aspect ratio, which Go ignores.
        Ok(())
    }

    /// Port of `readColorTable` (reader.go:291).
    fn read_color_table(&mut self, fields: u8) -> Result<Vec<Color>, Error> {
        let n = 1usize << (1 + u32::from(fields & F_COLOR_TABLE_BITS_MASK));
        read_full_gif(&mut self.r, &mut self.tmp[..3 * n]).map_err(Error::ReadingColorTable)?;
        Ok((0..n)
            .map(|i| {
                Color::Rgba([
                    self.tmp[3 * i],
                    self.tmp[3 * i + 1],
                    self.tmp[3 * i + 2],
                    0xFF,
                ])
            })
            .collect())
    }

    /// Port of `readExtension` (reader.go:305).
    fn read_extension(&mut self) -> Result<(), Error> {
        let extension = read_byte(&mut self.r).map_err(Error::ReadingExtension)?;
        let mut size = 0usize;
        match extension {
            E_TEXT => size = 13,
            E_GRAPHIC_CONTROL => return self.read_graphic_control(),
            E_COMMENT => {} // nothing to do but read the data
            E_APPLICATION => {
                let b = read_byte(&mut self.r).map_err(Error::ReadingExtension)?;
                // The spec requires 11, but Adobe sometimes writes 10.
                size = usize::from(b);
            }
            other => return Err(Error::UnknownExtension(other)),
        }
        if size > 0 {
            read_full_gif(&mut self.r, &mut self.tmp[..size]).map_err(Error::ReadingExtension)?;
        }

        // An application extension whose whole identifier is "NETSCAPE2.0" carries a loop count.
        if extension == E_APPLICATION && &self.tmp[..size] == b"NETSCAPE2.0" {
            let n = self.read_block().map_err(Error::ReadingExtension)?;
            if n == 0 {
                return Ok(());
            }
            if n == 3 && self.tmp[0] == 1 {
                self.loop_count = i64::from(self.tmp[1]) | (i64::from(self.tmp[2]) << 8);
            }
        }
        loop {
            let n = self.read_block().map_err(Error::ReadingExtension)?;
            if n == 0 {
                return Ok(());
            }
        }
    }

    /// Port of `readGraphicControl` (reader.go:359).
    fn read_graphic_control(&mut self) -> Result<(), Error> {
        read_full_gif(&mut self.r, &mut self.tmp[..6]).map_err(Error::ReadingGraphicControl)?;
        if self.tmp[0] != 4 {
            return Err(Error::BadGraphicControlSize(self.tmp[0]));
        }
        let flags = self.tmp[1];
        self.disposal_method = (flags & GC_DISPOSAL_METHOD_MASK) >> 2;
        self.delay_time = i64::from(self.tmp[2]) | (i64::from(self.tmp[3]) << 8);
        if flags & GC_TRANSPARENT_COLOR_SET != 0 {
            self.transparent_index = self.tmp[4];
            self.has_transparent_index = true;
        }
        if self.tmp[5] != 0 {
            return Err(Error::BadGraphicControlTerminator(self.tmp[5]));
        }
        Ok(())
    }

    /// Port of `readImageDescriptor` (reader.go:379).
    fn read_image_descriptor(&mut self, keep_all_frames: bool) -> Result<(), Error> {
        let mut m = self.new_image_from_descriptor()?;
        let use_local_color_table = self.image_fields & F_COLOR_TABLE != 0;
        let mut palette = if use_local_color_table {
            self.read_color_table(self.image_fields)?
        } else {
            match self.global_color_table.as_ref() {
                None => return Err(Error::NoColorTable),
                // Go assigns the global slice here and clones it below only when the frame is
                // transparent. A `Vec` cannot alias, so this is that clone, and the transparency
                // branch mutates *this* copy — which is what keeps `self.global_color_table`
                // unchanged for the frames that follow.
                Some(g) => g.clone(),
            }
        };
        if self.has_transparent_index {
            let ti = usize::from(self.transparent_index);
            if ti < palette.len() {
                palette[ti] = Color::Rgba([0, 0, 0, 0]);
            } else {
                // Out of range is an error by the spec, but Firefox and Chrome accept it, so Go
                // enlarges the palette with transparent entries (golang.org/issue/15059).
                let mut p = vec![Color::Rgba([0, 0, 0, 0]); ti + 1];
                p[..palette.len()].copy_from_slice(&palette);
                palette = p;
            }
        }
        m.palette = palette;

        let lit_width = read_byte(&mut self.r).map_err(|e| Error::ReadingImageData(e.into()))?;
        if !(2..=8).contains(&lit_width) {
            return Err(Error::LitWidthOutOfRange(lit_width));
        }
        {
            let br = BlockReader::new(&mut self.r);
            let mut lzwr = lzw::Reader::new(br, i64::from(lit_width));
            if let Some(e) = lzw_read_full(&mut lzwr, &mut m.pix.pix) {
                if e != lzw::UNEXPECTED_EOF {
                    return Err(Error::ReadingImageData(e));
                }
                return Err(Error::NotEnough);
            }
            // Both lzwr and br should now be exhausted. giflib does not enforce the spec's
            // "an End of Information code must be the last code output", so a stream that simply
            // ran out after the last pixel is accepted too (golang.org/issue/9856).
            let mut scratch = [0u8; 1];
            let (n, err) = lzwr.read(&mut scratch);
            if n != 0 || !matches!(err, Some(lzw::EOF) | Some(lzw::UNEXPECTED_EOF)) {
                return match err {
                    Some(e) => Err(Error::ReadingImageData(e)),
                    None => Err(Error::TooMuch),
                };
            }
            // Some GIFs carry an extra byte in the sub-block stream, which Go ignores
            // (golang.org/issue/16146).
            match lzwr.get_mut().close() {
                Ok(()) => {}
                Err(CloseError::TooMuch) => return Err(Error::TooMuch),
                Err(CloseError::Io(e)) => return Err(Error::ReadingImageData(e.into())),
            }
        }

        // Check that the colour indexes are inside the palette.
        if m.palette.len() < 256 {
            for &pixel in &m.pix.pix {
                if usize::from(pixel) >= m.palette.len() {
                    return Err(Error::BadPixel);
                }
            }
        }

        if self.image_fields & F_INTERLACE != 0 {
            uninterlace(&mut m);
        }

        if keep_all_frames || self.image.is_empty() {
            self.image.push(m);
            self.delay.push(self.delay_time);
            self.disposal.push(self.disposal_method);
        }
        // "The scope of this extension is the first graphic rendering block to follow" (GIF89a
        // §23), so the graphic control fields reset — the delay and the transparency, but *not*
        // the disposal method, which Go leaves standing for the next frame.
        self.delay_time = 0;
        self.has_transparent_index = false;
        Ok(())
    }

    /// Port of `newImageFromDescriptor` (reader.go:486).
    fn new_image_from_descriptor(&mut self) -> Result<Paletted, Error> {
        read_full_gif(&mut self.r, &mut self.tmp[..9]).map_err(Error::ReadingImageDescriptor)?;
        let left = i64::from(self.tmp[0]) + (i64::from(self.tmp[1]) << 8);
        let top = i64::from(self.tmp[2]) + (i64::from(self.tmp[3]) << 8);
        let width = i64::from(self.tmp[4]) + (i64::from(self.tmp[5]) << 8);
        let height = i64::from(self.tmp[6]) + (i64::from(self.tmp[7]) << 8);
        self.image_fields = self.tmp[8];

        // "Each image must fit within the boundaries of the Logical Screen" (GIF89a §20). Go
        // spells this out rather than using `Rectangle.In`, whose answer is true for *any* empty
        // rectangle; `left` and `top` are non-negative by construction, so only the maxima need
        // comparing.
        if left + width > self.width || top + height > self.height {
            return Err(Error::FrameBounds);
        }
        Ok(Paletted {
            pix: Pixels::new(
                Rect {
                    min_x: left,
                    min_y: top,
                    max_x: left + width,
                    max_y: top + height,
                },
                1,
            ),
            palette: Vec::new(),
        })
    }

    /// Port of `readBlock` (reader.go:521).
    fn read_block(&mut self) -> Result<usize, IoError> {
        let n = read_byte(&mut self.r)?;
        if n == 0 {
            return Ok(0);
        }
        read_full_gif(&mut self.r, &mut self.tmp[..usize::from(n)])?;
        Ok(usize::from(n))
    }
}

/// `readFull(lzwr, m.Pix)`: `io.ReadFull` over the LZW reader, then gif's `io.EOF` →
/// `io.ErrUnexpectedEOF`.
fn lzw_read_full<R: ByteRead>(r: &mut lzw::Reader<R>, buf: &mut [u8]) -> Option<lzw::Error> {
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
    } else if n > 0 && err == Some(lzw::EOF) {
        err = Some(lzw::UNEXPECTED_EOF);
    }
    if err == Some(lzw::EOF) {
        err = Some(lzw::UNEXPECTED_EOF);
    }
    err
}

/// Port of `uninterlace` (reader.go:546).
fn uninterlace(m: &mut Paletted) {
    let dx = m.pix.rect.dx().max(0) as usize;
    let dy = m.pix.rect.dy().max(0) as usize;
    let mut n_pix = vec![0u8; dx * dy];
    // Steps through the input by sequential scan lines.
    let mut offset = 0usize;
    for (skip, start) in INTERLACING {
        // Steps through the output as the pass defines.
        let mut n_offset = start * dx;
        let mut y = start;
        while y < dy {
            if let (Some(dst), Some(src)) = (
                n_pix.get_mut(n_offset..n_offset + dx),
                m.pix.pix.get(offset..offset + dx),
            ) {
                dst.copy_from_slice(src);
            }
            offset += dx;
            n_offset += dx * skip;
            y += skip;
        }
    }
    m.pix.pix = n_pix;
}

/// Port of `gif.Decode` (reader.go:565): the **first** frame only, as an [`Image::Paletted`].
pub fn decode(data: &[u8]) -> Result<Image, Error> {
    let mut d = Decoder::new(data);
    d.decode(false, false)?;
    // `decode` returns only after a frame was appended or at a trailer it refuses when empty, so
    // there is always a frame here.
    d.image
        .into_iter()
        .next()
        .map(Image::Paletted)
        .ok_or(Error::MissingImageData)
}

/// Port of `gif.DecodeConfig` (reader.go:627): the header and the global colour table, without
/// decoding a frame.
pub fn decode_config(data: &[u8]) -> Result<Config, Error> {
    let mut d = Decoder::new(data);
    d.decode(true, false)?;
    Ok(Config {
        width: d.width,
        height: d.height,
        model: d.global_color_table.unwrap_or_default(),
    })
}

/// Port of `gif.DecodeAll` (reader.go:605): every frame, with its delay and disposal method.
pub fn decode_all(data: &[u8]) -> Result<Gif, Error> {
    let mut d = Decoder::new(data);
    d.decode(false, true)?;
    Ok(Gif {
        image: d.image,
        delay: d.delay,
        loop_count: d.loop_count,
        disposal: d.disposal,
        config: Config {
            width: d.width,
            height: d.height,
            model: d.global_color_table.unwrap_or_default(),
        },
        background_index: d.background_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_quote_escapes_the_way_strconv_does() {
        assert_eq!(go_quote(b"GIF87b"), r#""GIF87b""#);
        assert_eq!(go_quote(b"GIF8\x00a"), r#""GIF8\x00a""#);
        assert_eq!(go_quote(b"GIF8\x7fa"), r#""GIF8\x7fa""#);
        assert_eq!(go_quote(b"GIF8\x80a"), r#""GIF8\x80a""#);
        assert_eq!(go_quote(b"GIF8\"a"), r#""GIF8\"a""#);
        assert_eq!(go_quote(b"GIF8\\a"), r#""GIF8\\a""#);
        assert_eq!(go_quote(b"GIF8 a"), r#""GIF8 a""#);
        assert_eq!(go_quote(b"\x07\x08\x0c\n\r\t\x0b"), r#""\a\b\f\n\r\t\v""#);
    }

    #[test]
    fn a_short_header_is_unexpected_eof_and_a_wrong_one_names_the_version() {
        assert_eq!(
            decode(b"").err().map(|e| e.to_string()).as_deref(),
            Some("gif: reading header: unexpected EOF")
        );
        assert_eq!(
            decode(b"GIF89a").err().map(|e| e.to_string()).as_deref(),
            Some("gif: reading header: unexpected EOF")
        );
        assert_eq!(
            decode(b"GIF88a\0\0\0\0\0\0\0")
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some(r#"gif: can't recognize format "GIF88a""#)
        );
    }

    /// Every pass of the interlace table, on a height that reaches all four.
    #[test]
    fn uninterlace_reverses_the_four_passes() {
        // dy = 9: pass 1 takes rows 0 and 8, pass 2 row 4, pass 3 rows 2 and 6, pass 4 rows
        // 1, 3, 5 and 7 — in that stored order.
        let stored: Vec<u8> = vec![0, 8, 4, 2, 6, 1, 3, 5, 7];
        let mut m = Paletted {
            pix: Pixels {
                pix: stored,
                stride: 1,
                rect: Rect::new(0, 0, 1, 9),
            },
            palette: Vec::new(),
        };
        uninterlace(&mut m);
        assert_eq!(m.pix.pix, vec![0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::{b64, describe, fixture, palette_entry};
    use serde_json::{Value as Json, json};

    /// Mirror of the oracle's `modelName` for a `color.Palette`.
    fn model_json(p: &[Color]) -> Json {
        json!({ "palette": p.iter().map(palette_entry).collect::<Vec<_>>() })
    }

    fn frames_json(g: &Gif) -> Json {
        let frames: Vec<Json> = g
            .image
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut d = describe(&Image::Paletted(m.clone()));
                d["delay"] = json!(g.delay.get(i).copied().unwrap_or_default());
                d["disposal"] = json!(g.disposal.get(i).copied().unwrap_or_default());
                d
            })
            .collect();
        json!({
            "loop_count": g.loop_count,
            "background_index": g.background_index,
            "w": g.config.width,
            "h": g.config.height,
            "model": model_json(&g.config.model),
            "frames": frames,
        })
    }

    /// `image.sniff` for the one magic that routes a file here.
    fn sniffs_gif(data: &[u8]) -> bool {
        data.len() >= 6
            && b"GIF8?a"
                .iter()
                .zip(data)
                .all(|(m, b)| *m == b'?' || m == b)
    }

    /// Every truncation and every single-byte corruption of a real animation, through all three
    /// entry points.
    ///
    /// Nothing is asserted about the answers — Go's are already asserted case by case above. What
    /// this asserts is that there is an answer: a decoder that allocates a frame from a size it
    /// read out of its own input can be talked into a runaway, and the crate's 6 GiB capped
    /// allocator turns one into a failure here rather than an OOM kill of whatever else is
    /// running. The logical screen is 16 bits square, so the worst a corrupt descriptor can ask
    /// for is ~4 GiB, and that is exactly what Go would ask for too.
    #[test]
    fn every_corruption_of_one_file_is_answered_rather_than_crashed() {
        let dec = fixture("gif")["decode"].as_array().unwrap();
        let base = dec
            .iter()
            .find(|c| c["name"] == "go_anim_disposals")
            .map(|c| b64(c["b64"].as_str().unwrap()))
            .unwrap();
        let mut n = 0;
        let try_all = |data: &[u8]| {
            let _ = decode_config(data);
            let _ = decode(data);
            let _ = decode_all(data);
        };
        for cut in 0..=base.len() {
            try_all(&base[..cut]);
            n += 1;
        }
        for pos in 0..base.len() {
            for mask in [0x01u8, 0x55, 0x80, 0xff] {
                let mut bad = base.clone();
                bad[pos] ^= mask;
                try_all(&bad);
                n += 1;
            }
        }
        assert!(n > 1000, "{n}");
    }

    /// Every file of the oracle's GIF corpus through `gif.DecodeConfig`, `gif.Decode` and
    /// `gif.DecodeAll` — the dimensions and the global colour table, the first frame's type,
    /// geometry, palette and pixels, and per frame the delay and the disposal method — or Go's
    /// error text, byte for byte.
    #[test]
    fn decode_matches_go_on_every_corpus_file() {
        let f = fixture("gif");
        let dec = f["decode"].as_array().unwrap();
        let all = f["decode_all"].as_array().unwrap();
        assert_eq!(dec.len(), all.len());
        let (mut checked, mut unsniffed, mut multi_frame) = (0, 0, 0);
        for (c, a) in dec.iter().zip(all) {
            let name = c["name"].as_str().unwrap();
            assert_eq!(c["name"], a["name"]);
            let data = b64(c["b64"].as_str().unwrap());

            let config = match decode_config(&data) {
                Ok(cfg) => {
                    json!({ "w": cfg.width, "h": cfg.height, "model": model_json(&cfg.model) })
                }
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(config, a["config"], "{name}: DecodeConfig");

            let first = match decode(&data) {
                Ok(m) => describe(&m),
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(first, a["decode"], "{name}: Decode");

            let every = match decode_all(&data) {
                Ok(g) => {
                    if g.image.len() > 1 {
                        multi_frame += 1;
                    }
                    frames_json(&g)
                }
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(every, a["all"], "{name}: DecodeAll");

            // What the registry answers, for the files whose first six bytes route them here.
            if sniffs_gif(&data) {
                let mut want_config = config.clone();
                if want_config.get("err").is_none() {
                    want_config["format"] = json!("gif");
                }
                assert_eq!(want_config, c["config"], "{name}: image.DecodeConfig");
                let mut want_image = first.clone();
                if want_image.get("err").is_none() {
                    want_image["format"] = json!("gif");
                }
                assert_eq!(want_image, c["image"], "{name}: image.Decode");
            } else {
                assert_eq!(c["image"]["err"], "image: unknown format", "{name}");
                unsniffed += 1;
            }
            checked += 1;
        }
        assert!(checked > 200, "{checked}");
        assert!(unsniffed >= 8, "{unsniffed}");
        assert!(multi_frame >= 10, "{multi_frame}");
    }
}
