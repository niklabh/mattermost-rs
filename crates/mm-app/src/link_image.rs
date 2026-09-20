//! Port of `parseImages` (app/post_metadata.go:1165) — what a link preview learns about an image
//! it fetched — and of `peekContentType` / `http.DetectContentType`, which decide that a response
//! with no `Content-Type` *is* an image in the first place.
//!
//! # Why this is not a call into an image crate
//!
//! `parseImages` returns three distinguishable outcomes, and each one lands on the wire
//! differently one step up in `getEmbedForPost`:
//!
//! | Go returns | this returns | the post gets |
//! |---|---|---|
//! | `(&PostImage{…}, nil)` | [`ImageProbe::Image`] | an `image` embed plus a `metadata.images` entry |
//! | `(nil, nil)` — only for TIFF | [`ImageProbe::Nil`] | a bare `link` embed |
//! | `(nil, err)` | [`ImageProbe::Error`] | **no embed at all**, and a `none` row |
//!
//! So *which* malformed images Go rejects is wire behaviour, not an implementation detail. A
//! general-purpose decoder is stricter in some places and more lenient in others, so each of the
//! six formats the server registers is reproduced here down to the branch that accepts or rejects
//! the header — `image.DecodeConfig` (std `image/png`, `image/jpeg`, `image/gif`, and
//! `golang.org/x/image@v0.44.0`'s `bmp`, `tiff` and `webp`, registered by `channels/app/imaging`;
//! `go list -deps ./cmd/mattermost` finds no other `image.RegisterFormat` call) — and so are the two
//! follow-ups: the JPEG EXIF orientation that swaps the dimensions
//! (`imaging.GetImageOrientation`, built on `github.com/bep/imagemeta@v0.17.2`), and the GIF frame
//! count (`imgutils.CountGIFFrames`), which **decompresses every frame** and fails the whole
//! probe on a corrupt one.
//!
//! Pinned by `fixtures/behaviour_link_image.json` (`reference/dump/behaviour_link_image.go`), whose
//! results come from Go's own functions over the same bytes.
//!
//! The Go error strings are reproduced where Go builds them, but nothing downstream reads one:
//! `getLinkMetadata` only logs it. They are carried for diagnosis and pinned by the oracle.

use mm_model::post_metadata::PostImage;

/// `MaxMetadataImageSize` (post_metadata.go:40), which is `MaxOpenGraphResponseSize`
/// (opengraph.go:23): the caller hands [`parse_images`] at most this many bytes.
pub const MAX_METADATA_IMAGE_SIZE: usize = 50 * 1024 * 1024;

/// What `parseImages` answered. See the module docs for what each becomes on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageProbe {
    /// `(&PostImage{…}, nil)`.
    Image(PostImage),
    /// `(nil, nil)`: a TIFF that decoded. "Make image information nil when the format is tiff."
    Nil,
    /// `(nil, err)`, with Go's error text.
    Error(String),
    /// A shape whose Go answer this port cannot compute with certainty; the caller forwards.
    Unreproducible(&'static str),
}

const UNEXPECTED_EOF: &str = "unexpected EOF";

/// Port of `parseImages` (post_metadata.go:1165). `body` is the response body, already limited
/// to [`MAX_METADATA_IMAGE_SIZE`].
///
/// Go reads the config through a `TeeReader` and then hands `io.MultiReader(buf, body)` to the
/// orientation and frame-count passes — i.e. both see the **whole** body from its first byte,
/// which is why both take `body` here.
pub fn parse_images(body: &[u8]) -> ImageProbe {
    let config = match decode_config(body) {
        Ok(config) => config,
        Err(err) => return ImageProbe::Error(err),
    };
    let mut image = PostImage {
        width: config.width,
        height: config.height,
        format: config.format.to_owned(),
        frame_count: 0,
    };

    if config.format == "jpeg" {
        // An orientation error is logged ("Failed to get image orientation") and ignored.
        match jpeg_orientation(body) {
            Err(reason) => return ImageProbe::Unreproducible(reason),
            Ok(Some(orientation)) if (5..=8).contains(&orientation) => {
                // RotatedCWMirrored, RotatedCCW, RotatedCCWMirrored, RotatedCW.
                std::mem::swap(&mut image.width, &mut image.height);
            }
            Ok(_) => {}
        }
    }

    if config.format == "gif" {
        match count_gif_frames(body) {
            Ok(frames) => image.frame_count = frames,
            Err(err) => return ImageProbe::Error(err),
        }
    }

    if config.format == "tiff" {
        return ImageProbe::Nil;
    }
    ImageProbe::Image(image)
}

/// The fields of `image.Config` a caller of [`decode_config`] reads, plus the format name
/// `image.DecodeConfig` returns beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedConfig {
    pub format: &'static str,
    pub width: i64,
    pub height: i64,
}

/// Port of `image.DecodeConfig` (image/format.go:101) over the six registered formats: the
/// first magic that matches picks the decoder, and no match is `image.ErrFormat`.
///
/// `?` in a magic is a wildcard byte, and `Peek` fails on input shorter than the magic, so a
/// truncated header is "unknown format" rather than a decoder's own error.
pub fn decode_config(data: &[u8]) -> Result<DecodedConfig, String> {
    type Decoder = fn(&[u8]) -> Result<(i64, i64), String>;
    const FORMATS: &[(&str, &[u8], Decoder)] = &[
        ("png", b"\x89PNG\r\n\x1a\n", png_config),
        ("jpeg", b"\xff\xd8", jpeg_config),
        ("gif", b"GIF8?a", gif_config),
        ("bmp", b"BM????\x00\x00\x00\x00", bmp_config),
        ("tiff", b"II\x2a\x00", tiff_config),
        ("tiff", b"MM\x00\x2a", tiff_config),
        ("webp", b"RIFF????WEBPVP8", webp_config),
    ];
    for (name, magic, decode) in FORMATS {
        let Some(prefix) = data.get(..magic.len()) else {
            continue;
        };
        if prefix
            .iter()
            .zip(magic.iter())
            .all(|(b, m)| *m == b'?' || b == m)
        {
            let (width, height) = decode(data)?;
            return Ok(DecodedConfig {
                format: name,
                width,
                height,
            });
        }
    }
    Err("image: unknown format".to_owned())
}

// ---------------------------------------------------------------------------------------------
// A forward-only byte stream: what every decoder here sees through `bufio`.
// ---------------------------------------------------------------------------------------------

struct Stream<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Stream<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// `io.ReadFull`: all `n` bytes, or nothing and the stream exhausted. `Err(true)` is
    /// `io.EOF` (no byte was available), `Err(false)` is `io.ErrUnexpectedEOF`.
    fn read_full(&mut self, n: usize) -> Result<&'a [u8], bool> {
        if n == 0 {
            return Ok(&[]);
        }
        let available = self.remaining();
        if available < n {
            self.pos = self.data.len();
            return Err(available == 0);
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn byte(&mut self) -> Option<u8> {
        let b = self.data.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }
}

fn eof_text(eof: bool) -> &'static str {
    if eof { "EOF" } else { UNEXPECTED_EOF }
}

// ---------------------------------------------------------------------------------------------
// PNG — image/png/reader.go
// ---------------------------------------------------------------------------------------------

const CRC_TABLE: [u32; 256] = {
    let mut table = [0_u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// `crc32.NewIEEE()`, incrementally.
struct Crc(u32);

impl Crc {
    fn new() -> Self {
        Self(0xffff_ffff)
    }
    fn write(&mut self, data: &[u8]) {
        for b in data {
            self.0 = CRC_TABLE[((self.0 ^ u32::from(*b)) & 0xff) as usize] ^ (self.0 >> 8);
        }
    }
    fn sum(&self) -> u32 {
        !self.0
    }
}

const PNG_STAGE_START: u8 = 0;
const PNG_STAGE_IHDR: u8 = 1;
const PNG_STAGE_PLTE: u8 = 2;
const PNG_STAGE_TRNS: u8 = 3;
const PNG_STAGE_IDAT: u8 = 4;
const PNG_STAGE_IEND: u8 = 5;

/// The colour-type/bit-depth combinations `parseIHDR` accepts, as Go's `cb` families.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PngCb {
    Invalid,
    Gray,
    Paletted,
    TrueColor,
    Other,
}

fn png_format(s: &str) -> String {
    format!("png: invalid format: {s}")
}

fn png_unsupported(s: &str) -> String {
    format!("png: unsupported feature: {s}")
}

/// Port of png `DecodeConfig` (reader.go:993): chunks until IHDR, or — for a paletted image —
/// until the stage reaches tRNS (a PLTE followed by tRNS or IDAT), each chunk checksummed.
///
/// Every read failure surfaces as `unexpected EOF`: the partial ones already are, and
/// `DecodeConfig` maps a bare `io.EOF` to it.
fn png_config(data: &[u8]) -> Result<(i64, i64), String> {
    let mut s = Stream::new(data);
    let eof = |_| UNEXPECTED_EOF.to_owned();
    // checkHeader — the magic already matched.
    s.read_full(8).map_err(eof)?;

    let mut stage = PNG_STAGE_START;
    let mut cb = PngCb::Invalid;
    let mut depth = 0_u32;
    let (mut width, mut height) = (0_i64, 0_i64);

    loop {
        let header = s.read_full(8).map_err(eof)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let mut crc = Crc::new();
        crc.write(&header[4..8]);

        let mut ignore = false;
        match &header[4..8] {
            b"IHDR" => {
                if stage != PNG_STAGE_START {
                    return Err(png_format("chunk out of order"));
                }
                stage = PNG_STAGE_IHDR;
                if length != 13 {
                    return Err(png_format("bad IHDR length"));
                }
                let ihdr = s.read_full(13).map_err(eof)?;
                crc.write(ihdr);
                if ihdr[10] != 0 {
                    return Err(png_unsupported("compression method"));
                }
                if ihdr[11] != 0 {
                    return Err(png_unsupported("filter method"));
                }
                if ihdr[12] != 0 && ihdr[12] != 1 {
                    return Err(png_format("invalid interlace method"));
                }
                let w = i32::from_be_bytes([ihdr[0], ihdr[1], ihdr[2], ihdr[3]]);
                let h = i32::from_be_bytes([ihdr[4], ihdr[5], ihdr[6], ihdr[7]]);
                if w <= 0 || h <= 0 {
                    return Err(png_format("non-positive dimension"));
                }
                let n_pixels = i64::from(w) * i64::from(h);
                // "There can be up to 8 bytes per pixel": Go's `nPixels != (nPixels*8)/8` on a
                // wrapping 64-bit int.
                if n_pixels != n_pixels.wrapping_mul(8) / 8 {
                    return Err(png_unsupported("dimension overflow"));
                }
                depth = u32::from(ihdr[8]);
                cb = match (ihdr[8], ihdr[9]) {
                    (1 | 2 | 4, 0) | (8, 0) | (16, 0) => PngCb::Gray,
                    (1 | 2 | 4 | 8, 3) => PngCb::Paletted,
                    (8 | 16, 2) => PngCb::TrueColor,
                    (8 | 16, 4) | (8 | 16, 6) => PngCb::Other,
                    _ => PngCb::Invalid,
                };
                if cb == PngCb::Invalid {
                    return Err(png_unsupported(&format!(
                        "bit depth {}, color type {}",
                        ihdr[8], ihdr[9]
                    )));
                }
                width = i64::from(w);
                height = i64::from(h);
            }
            b"PLTE" => {
                if stage != PNG_STAGE_IHDR {
                    return Err(png_format("chunk out of order"));
                }
                stage = PNG_STAGE_PLTE;
                let np = length / 3;
                // Only a paletted image reaches here: any other breaks out after IHDR.
                if length % 3 != 0 || np == 0 || np > 256 || np > (1_u32 << depth) {
                    return Err(png_format("bad PLTE length"));
                }
                let plte = s.read_full((3 * np) as usize).map_err(eof)?;
                crc.write(plte);
                if cb != PngCb::Paletted && cb != PngCb::TrueColor && cb != PngCb::Other {
                    return Err(png_format("PLTE, color type mismatch"));
                }
            }
            b"tRNS" => {
                match cb {
                    PngCb::Paletted => {
                        if stage != PNG_STAGE_PLTE {
                            return Err(png_format("chunk out of order"));
                        }
                    }
                    PngCb::TrueColor => {
                        if stage != PNG_STAGE_IHDR && stage != PNG_STAGE_PLTE {
                            return Err(png_format("chunk out of order"));
                        }
                    }
                    _ => {
                        if stage != PNG_STAGE_IHDR {
                            return Err(png_format("chunk out of order"));
                        }
                    }
                }
                stage = PNG_STAGE_TRNS;
                // The loop breaks after IHDR for every non-paletted image, so a tRNS parsed
                // here belongs to a paletted one.
                if length > 256 {
                    return Err(png_format("bad tRNS length"));
                }
                let trns = s.read_full(length as usize).map_err(eof)?;
                crc.write(trns);
            }
            b"IDAT" => {
                if !(PNG_STAGE_IHDR..=PNG_STAGE_IDAT).contains(&stage)
                    || (stage == PNG_STAGE_IHDR && cb == PngCb::Paletted)
                {
                    return Err(png_format("chunk out of order"));
                } else if stage == PNG_STAGE_IDAT {
                    ignore = true;
                } else {
                    // configOnly: the data is not read.
                    stage = PNG_STAGE_IDAT;
                }
            }
            b"IEND" => {
                if stage != PNG_STAGE_IDAT {
                    return Err(png_format("chunk out of order"));
                }
                stage = PNG_STAGE_IEND;
                if length != 0 {
                    return Err(png_format("bad IEND length"));
                }
            }
            _ => ignore = true,
        }

        let checksum = match &header[4..8] {
            b"IDAT" => ignore,
            _ => true,
        };
        if ignore {
            if length > 0x7fff_ffff {
                return Err(png_format(&format!("Bad chunk length: {length}")));
            }
            let skipped = s.read_full(length as usize).map_err(eof)?;
            crc.write(skipped);
        }
        if checksum {
            let sum = s.read_full(4).map_err(eof)?;
            if u32::from_be_bytes([sum[0], sum[1], sum[2], sum[3]]) != crc.sum() {
                return Err(png_format("invalid checksum"));
            }
        }

        let done = if cb == PngCb::Paletted {
            stage >= PNG_STAGE_TRNS
        } else {
            stage >= PNG_STAGE_IHDR
        };
        if done {
            return Ok((width, height));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// JPEG — image/jpeg/reader.go
// ---------------------------------------------------------------------------------------------

fn jpeg_format(s: &str) -> String {
    format!("invalid JPEG format: {s}")
}

fn jpeg_unsupported(s: &str) -> String {
    format!("unsupported JPEG feature: {s}")
}

#[derive(Clone, Copy, Default)]
struct JpegComponent {
    h: u8,
    v: u8,
    c: u8,
}

/// Port of jpeg `DecodeConfig` (reader.go:778) over `decode(r, configOnly=true)` (:520).
///
/// In config mode no entropy-coded data is read, so the byte-stuffing machinery never engages
/// and the input is a plain stream. The walk ends at the first SOS (answered from the SOF seen
/// before it), at a SOF preceded by a JFIF APP0, or at EOI — which is always an error, because
/// config mode never decodes a scan.
fn jpeg_config(data: &[u8]) -> Result<(i64, i64), String> {
    let mut s = Stream::new(data);
    let eof = || UNEXPECTED_EOF.to_owned();
    let soi = s.read_full(2).map_err(|_| eof())?;
    if soi[0] != 0xff || soi[1] != 0xd8 {
        return Err(jpeg_format("missing SOI marker"));
    }

    let mut n_comp = 0_usize;
    let (mut width, mut height) = (0_i64, 0_i64);
    let mut jfif = false;

    loop {
        let pair = s.read_full(2).map_err(|_| eof())?;
        let (mut t0, mut t1) = (pair[0], pair[1]);
        while t0 != 0xff {
            t0 = t1;
            t1 = s.byte().ok_or_else(eof)?;
        }
        let mut marker = t1;
        if marker == 0 {
            // "\xff\x00" is extraneous data.
            continue;
        }
        while marker == 0xff {
            marker = s.byte().ok_or_else(eof)?;
        }
        if marker == 0xd9 {
            // EOI. Config mode never decodes a scan, so no image exists.
            return Err(jpeg_format("missing SOS marker"));
        }
        if (0xd0..=0xd7).contains(&marker) {
            continue;
        }

        let len = s.read_full(2).map_err(|_| eof())?;
        let n = (i64::from(len[0]) << 8) + i64::from(len[1]) - 2;
        if n < 0 {
            return Err(jpeg_format("short segment length"));
        }
        let n = n as usize;

        match marker {
            0xc0..=0xc2 => {
                let result = jpeg_sof(&mut s, n, &mut n_comp, &mut width, &mut height);
                if jfif {
                    result?;
                    return Ok((width, height));
                }
                result?;
            }
            0xc4 | 0xdb | 0xdd => {
                s.read_full(n).map_err(|_| eof())?;
            }
            0xda => {
                return match n_comp {
                    1 | 3 | 4 => Ok((width, height)),
                    _ => Err(jpeg_format("missing SOF marker")),
                };
            }
            0xe0 => {
                if n < 5 {
                    s.read_full(n).map_err(|_| eof())?;
                } else {
                    let tag = s.read_full(5).map_err(|_| eof())?;
                    jfif = tag == b"JFIF\x00";
                    s.read_full(n - 5).map_err(|_| eof())?;
                }
            }
            0xee => {
                // APP14 only feeds the colour model, which nothing here reads.
                s.read_full(n).map_err(|_| eof())?;
            }
            0xe1..=0xef | 0xfe => {
                s.read_full(n).map_err(|_| eof())?;
            }
            m if m < 0xc0 => return Err(jpeg_format("unknown marker")),
            _ => return Err(jpeg_unsupported("unknown marker")),
        }
    }
}

/// `processSOF` (reader.go:301).
fn jpeg_sof(
    s: &mut Stream<'_>,
    n: usize,
    n_comp: &mut usize,
    width: &mut i64,
    height: &mut i64,
) -> Result<(), String> {
    if *n_comp != 0 {
        return Err(jpeg_format("multiple SOF markers"));
    }
    *n_comp = match n {
        9 => 1,
        15 => 3,
        18 => 4,
        _ => return Err(jpeg_unsupported("number of components")),
    };
    let tmp = s.read_full(n).map_err(|_| UNEXPECTED_EOF.to_owned())?;
    if tmp[0] != 8 {
        return Err(jpeg_unsupported("precision"));
    }
    *height = (i64::from(tmp[1]) << 8) + i64::from(tmp[2]);
    *width = (i64::from(tmp[3]) << 8) + i64::from(tmp[4]);
    if usize::from(tmp[5]) != *n_comp {
        return Err(jpeg_format("SOF has wrong length"));
    }
    let ratio = || jpeg_unsupported("luma/chroma subsampling ratio");
    let mut comp = [JpegComponent::default(); 4];
    for i in 0..*n_comp {
        comp[i].c = tmp[6 + 3 * i];
        for j in 0..i {
            if comp[i].c == comp[j].c {
                return Err(jpeg_format("repeated component identifier"));
            }
        }
        if tmp[8 + 3 * i] > 3 {
            return Err(jpeg_format("bad Tq value"));
        }
        let hv = tmp[7 + 3 * i];
        let (mut h, mut v) = (hv >> 4, hv & 0x0f);
        // Out of range is a *format* error; 3 is merely unsupported.
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
            return Err(jpeg_format("luma/chroma subsampling ratio"));
        }
        if h == 3 || v == 3 {
            return Err(ratio());
        }
        match *n_comp {
            1 => {
                h = 1;
                v = 1;
            }
            3 => match i {
                0 => {
                    if v == 4 {
                        return Err(ratio());
                    }
                }
                1 => {
                    if comp[0].h % h != 0 || comp[0].v % v != 0 {
                        return Err(ratio());
                    }
                }
                _ => {
                    if comp[1].h != h || comp[1].v != v {
                        return Err(ratio());
                    }
                }
            },
            _ => match i {
                0 => {
                    if hv != 0x11 && hv != 0x22 {
                        return Err(ratio());
                    }
                }
                1 | 2 => {
                    if hv != 0x11 {
                        return Err(ratio());
                    }
                }
                _ => {
                    if comp[0].h != h || comp[0].v != v {
                        return Err(ratio());
                    }
                }
            },
        }
        comp[i].h = h;
        comp[i].v = v;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// GIF — image/gif/reader.go (config) and channels/utils/imgutils/gif.go (frames)
// ---------------------------------------------------------------------------------------------

/// Go's `%q` of the six version bytes. Only byte 4 is free (the magic is `GIF8?a`) and byte 5 is
/// `a`, so a byte at or above 0x80 can never begin a valid UTF-8 sequence and prints as `\xNN`.
fn go_quote(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('"');
    out
}

/// `readHeaderAndScreenDescriptor` — identical in `image/gif` and `imgutils`.
fn gif_header(s: &mut Stream<'_>) -> Result<(i64, i64, bool), String> {
    let tmp = s
        .read_full(13)
        .map_err(|_| format!("gif: reading header: {UNEXPECTED_EOF}"))?;
    if &tmp[..6] != b"GIF87a" && &tmp[..6] != b"GIF89a" {
        return Err(format!(
            "gif: can't recognize format {}",
            go_quote(&tmp[..6])
        ));
    }
    let width = i64::from(tmp[6]) + (i64::from(tmp[7]) << 8);
    let height = i64::from(tmp[8]) + (i64::from(tmp[9]) << 8);
    let fields = tmp[10];
    let global = fields & 0x80 != 0;
    if global {
        gif_color_table(s, fields)?;
    }
    Ok((width, height, global))
}

fn gif_color_table(s: &mut Stream<'_>, fields: u8) -> Result<(), String> {
    let n = 1_usize << (1 + usize::from(fields & 7));
    s.read_full(3 * n)
        .map_err(|_| format!("gif: reading color table: {UNEXPECTED_EOF}"))?;
    Ok(())
}

/// Port of gif `DecodeConfig` (reader.go:627): the header and screen descriptor only.
fn gif_config(data: &[u8]) -> Result<(i64, i64), String> {
    let mut s = Stream::new(data);
    let (width, height, _) = gif_header(&mut s)?;
    Ok((width, height))
}

/// Port of `imgutils.CountGIFFrames` (channels/utils/imgutils/gif.go): a GIF decoder with the
/// pixel writes removed. Every frame's LZW stream is still decompressed and must end where the
/// block structure says it does, so a corrupt frame fails the whole count — and with it
/// `parseImages`.
///
/// One deliberate leniency is Go's and is kept: the frame data is read through
/// `io.Copy(io.Discard, io.LimitReader(lzwr, w*h))`, and `io.Discard` treats `io.EOF` as success,
/// so a frame whose LZW end code arrives **before** `w*h` pixels is accepted (the std decoder
/// would reject it with "not enough image data").
pub fn count_gif_frames(data: &[u8]) -> Result<i64, String> {
    let mut s = Stream::new(data);
    let (screen_w, screen_h, has_global) = gif_header(&mut s)?;
    let mut image_count = 0_i64;
    loop {
        let c = s
            .byte()
            .ok_or_else(|| format!("gif: reading frames: {UNEXPECTED_EOF}"))?;
        match c {
            0x21 => gif_extension(&mut s)?,
            0x2c => {
                gif_image(&mut s, screen_w, screen_h, has_global)?;
                image_count += 1;
            }
            0x3b => {
                if image_count == 0 {
                    return Err("gif: missing image data".to_owned());
                }
                return Ok(image_count);
            }
            _ => return Err(format!("gif: unknown block type: 0x{c:02x}")),
        }
    }
}

/// `readExtension` (gif.go).
fn gif_extension(s: &mut Stream<'_>) -> Result<(), String> {
    let err = || format!("gif: reading extension: {UNEXPECTED_EOF}");
    let extension = s.byte().ok_or_else(err)?;
    let size = match extension {
        0x01 => 13,
        0xf9 => return gif_graphic_control(s),
        0xfe => 0,
        0xff => usize::from(s.byte().ok_or_else(err)?),
        _ => return Err(format!("gif: unknown extension 0x{extension:02x}")),
    };
    let mut netscape = false;
    if size > 0 {
        let tmp = s.read_full(size).map_err(|_| err())?;
        netscape = extension == 0xff && tmp == b"NETSCAPE2.0";
    }
    if netscape {
        // The loop-count block; its value is not needed.
        let n = gif_block(s).map_err(|()| err())?;
        if n == 0 {
            return Ok(());
        }
    }
    loop {
        if gif_block(s).map_err(|()| err())? == 0 {
            return Ok(());
        }
    }
}

/// `readBlock`: a length byte and that many bytes; `0` is the terminator.
fn gif_block(s: &mut Stream<'_>) -> Result<usize, ()> {
    let n = s.byte().ok_or(())?;
    if n == 0 {
        return Ok(0);
    }
    s.read_full(usize::from(n)).map_err(|_| ())?;
    Ok(usize::from(n))
}

/// `readGraphicControl` (gif.go).
fn gif_graphic_control(s: &mut Stream<'_>) -> Result<(), String> {
    let tmp = s
        .read_full(6)
        .map_err(|_| format!("gif: can't read graphic control: {UNEXPECTED_EOF}"))?;
    if tmp[0] != 4 {
        return Err(format!(
            "gif: invalid graphic control extension block size: {}",
            tmp[0]
        ));
    }
    if tmp[5] != 0 {
        return Err(format!(
            "gif: invalid graphic control extension block terminator: {}",
            tmp[5]
        ));
    }
    Ok(())
}

/// `readImageDescriptor` (gif.go), frame counting only.
fn gif_image(
    s: &mut Stream<'_>,
    screen_w: i64,
    screen_h: i64,
    has_global: bool,
) -> Result<(), String> {
    let tmp = s
        .read_full(9)
        .map_err(|_| format!("gif: can't read image descriptor: {UNEXPECTED_EOF}"))?;
    let u16le = |i: usize| i64::from(tmp[i]) + (i64::from(tmp[i + 1]) << 8);
    let (left, top, width, height) = (u16le(0), u16le(2), u16le(4), u16le(6));
    let fields = tmp[8];
    if left + width > screen_w || top + height > screen_h {
        return Err("gif: frame bounds larger than image bounds".to_owned());
    }
    if fields & 0x80 != 0 {
        gif_color_table(s, fields)?;
    } else if !has_global {
        return Err("gif: no color table".to_owned());
    }
    let lit_width = s
        .byte()
        .ok_or_else(|| format!("gif: reading image data: {UNEXPECTED_EOF}"))?;
    if !(2..=8).contains(&lit_width) {
        return Err(format!(
            "gif: pixel size in decode out of range: {lit_width}"
        ));
    }

    let mut blocks = BlockReader::default();
    let mut lzw = Lzw::new(lit_width);

    // io.Copy(io.Discard, io.LimitReader(lzwr, int64(w*h))): 8 KiB reads, EOF is success.
    let mut limit = (width * height) as u64;
    while limit > 0 {
        let want = limit.min(8192) as usize;
        match lzw.read(s, &mut blocks, want) {
            Ok(n) => limit -= n as u64,
            Err(LzwError::Eof) => break,
            Err(LzwError::UnexpectedEof) => return Err("gif: not enough image data".to_owned()),
            Err(LzwError::InvalidCode) => {
                return Err("gif: reading image data: lzw: invalid code".to_owned());
            }
        }
    }

    // "In theory, both lzwr and br should be exhausted."
    match lzw.read(s, &mut blocks, 1) {
        Ok(_) => return Err("gif: too much image data".to_owned()),
        Err(LzwError::Eof | LzwError::UnexpectedEof) => {}
        Err(LzwError::InvalidCode) => {
            return Err("gif: reading image data: lzw: invalid code".to_owned());
        }
    }

    blocks.close(s)
}

/// The sticky error of a `blockReader`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockErr {
    /// A clean zero-length terminator block.
    Eof,
    /// The stream ran out.
    UnexpectedEof,
}

/// Port of imgutils' `blockReader`: GIF data sub-blocks presented as one byte stream.
#[derive(Default)]
struct BlockReader {
    buf: Vec<u8>,
    i: usize,
    err: Option<BlockErr>,
}

impl BlockReader {
    fn fill(&mut self, s: &mut Stream<'_>) {
        if self.err.is_some() {
            return;
        }
        let Some(j) = s.byte() else {
            self.err = Some(BlockErr::UnexpectedEof);
            self.buf.clear();
            return;
        };
        if j == 0 {
            self.err = Some(BlockErr::Eof);
            self.buf.clear();
            return;
        }
        self.i = 0;
        match s.read_full(usize::from(j)) {
            Ok(block) => {
                self.buf.clear();
                self.buf.extend_from_slice(block);
            }
            Err(_) => {
                self.err = Some(BlockErr::UnexpectedEof);
                self.buf.clear();
            }
        }
    }

    fn read_byte(&mut self, s: &mut Stream<'_>) -> Result<u8, BlockErr> {
        if self.i >= self.buf.len() {
            self.fill(s);
            if let Some(err) = self.err {
                return Err(err);
            }
        }
        // A successful fill leaves `i == 0` over a non-empty block.
        let c = self
            .buf
            .get(self.i)
            .copied()
            .ok_or(BlockErr::UnexpectedEof)?;
        self.i += 1;
        Ok(c)
    }

    /// `close` (gif.go): at most one trailing sub-block of slack before the terminator.
    fn close(&mut self, s: &mut Stream<'_>) -> Result<(), String> {
        let unexpected = || format!("gif: reading image data: {UNEXPECTED_EOF}");
        match self.err {
            Some(BlockErr::Eof) => return Ok(()),
            Some(BlockErr::UnexpectedEof) => return Err(unexpected()),
            None => {}
        }
        if self.i == self.buf.len() {
            self.fill(s);
            match self.err {
                Some(BlockErr::Eof) => return Ok(()),
                Some(BlockErr::UnexpectedEof) => return Err(unexpected()),
                None => {
                    if self.buf.len() > 1 {
                        return Err("gif: too much image data".to_owned());
                    }
                }
            }
        }
        self.fill(s);
        match self.err {
            Some(BlockErr::Eof) => Ok(()),
            Some(BlockErr::UnexpectedEof) => Err(unexpected()),
            None => Err("gif: too much image data".to_owned()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LzwError {
    Eof,
    UnexpectedEof,
    InvalidCode,
}

const LZW_MAX_WIDTH: u32 = 12;
const LZW_INVALID_CODE: u16 = 0xffff;
const LZW_FLUSH_BUFFER: usize = 1 << LZW_MAX_WIDTH;

/// Port of `compress/lzw.Reader` in LSB order (reader.go), which GIF uses. The batching —
/// `decode` stops once 4096 bytes are buffered — is reproduced exactly, because what the frame
/// check reads *after* the pixel quota depends on where a batch ended.
struct Lzw {
    bits: u32,
    n_bits: u32,
    width: u32,
    lit_width: u32,
    err: Option<LzwError>,
    clear: u16,
    eof: u16,
    hi: u16,
    overflow: u16,
    last: u16,
    suffix: Box<[u8; 1 << LZW_MAX_WIDTH]>,
    prefix: Box<[u16; 1 << LZW_MAX_WIDTH]>,
    output: Box<[u8; 2 * (1 << LZW_MAX_WIDTH)]>,
    o: usize,
    /// `toRead` as `output[start..end]`.
    to_read: (usize, usize),
}

impl Lzw {
    fn new(lit_width: u8) -> Self {
        let lit_width = u32::from(lit_width);
        let clear = 1_u16 << lit_width;
        Self {
            bits: 0,
            n_bits: 0,
            width: 1 + lit_width,
            lit_width,
            err: None,
            clear,
            eof: clear + 1,
            hi: clear + 1,
            overflow: 1 << (1 + lit_width),
            last: LZW_INVALID_CODE,
            suffix: Box::new([0; 1 << LZW_MAX_WIDTH]),
            prefix: Box::new([0; 1 << LZW_MAX_WIDTH]),
            output: Box::new([0; 2 * (1 << LZW_MAX_WIDTH)]),
            o: 0,
            to_read: (0, 0),
        }
    }

    fn read_code(&mut self, s: &mut Stream<'_>, b: &mut BlockReader) -> Result<u16, BlockErr> {
        while self.n_bits < self.width {
            let x = b.read_byte(s)?;
            self.bits |= u32::from(x) << self.n_bits;
            self.n_bits += 8;
        }
        let code = (self.bits & ((1 << self.width) - 1)) as u16;
        self.bits >>= self.width;
        self.n_bits -= self.width;
        Ok(code)
    }

    /// `Read(p)` with `len(p) == max`: returns the byte count, never zero.
    fn read(
        &mut self,
        s: &mut Stream<'_>,
        b: &mut BlockReader,
        max: usize,
    ) -> Result<usize, LzwError> {
        loop {
            let (start, end) = self.to_read;
            if start < end {
                let n = (end - start).min(max);
                self.to_read.0 += n;
                return Ok(n);
            }
            if let Some(err) = self.err {
                return Err(err);
            }
            self.decode(s, b);
        }
    }

    fn decode(&mut self, s: &mut Stream<'_>, b: &mut BlockReader) {
        let last_index = self.output.len() - 1;
        loop {
            let code = match self.read_code(s, b) {
                Ok(code) => code,
                // `io.EOF` from the block reader is an unexpected end to the LZW stream.
                Err(_) => {
                    self.err = Some(LzwError::UnexpectedEof);
                    break;
                }
            };
            if code < self.clear {
                self.output[self.o] = code as u8;
                self.o += 1;
                if self.last != LZW_INVALID_CODE {
                    self.suffix[usize::from(self.hi)] = code as u8;
                    self.prefix[usize::from(self.hi)] = self.last;
                }
            } else if code == self.clear {
                self.width = 1 + self.lit_width;
                self.hi = self.eof;
                self.overflow = 1 << self.width;
                self.last = LZW_INVALID_CODE;
                continue;
            } else if code == self.eof {
                self.err = Some(LzwError::Eof);
                break;
            } else if code <= self.hi {
                let mut c = code;
                let mut i = last_index;
                if code == self.hi && self.last != LZW_INVALID_CODE {
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
                let len = self.output.len() - i;
                self.output.copy_within(i.., self.o);
                self.o += len;
                if self.last != LZW_INVALID_CODE {
                    self.suffix[usize::from(self.hi)] = c as u8;
                    self.prefix[usize::from(self.hi)] = self.last;
                }
            } else {
                self.err = Some(LzwError::InvalidCode);
                break;
            }
            self.last = code;
            self.hi += 1;
            if self.hi >= self.overflow {
                if self.width == LZW_MAX_WIDTH {
                    self.last = LZW_INVALID_CODE;
                    self.hi -= 1;
                } else {
                    self.width += 1;
                    self.overflow = 1 << self.width;
                }
            }
            if self.o >= LZW_FLUSH_BUFFER {
                break;
            }
        }
        self.to_read = (0, self.o);
        self.o = 0;
    }
}

// ---------------------------------------------------------------------------------------------
// BMP — golang.org/x/image/bmp/reader.go
// ---------------------------------------------------------------------------------------------

const BMP_UNSUPPORTED: &str = "bmp: unsupported BMP image";

/// Port of bmp `decodeConfig` (reader.go:155).
fn bmp_config(data: &[u8]) -> Result<(i64, i64), String> {
    const FILE_HEADER_LEN: u32 = 14;
    const INFO_HEADER_LEN: u32 = 40;
    let mut s = Stream::new(data);
    let unsupported = || BMP_UNSUPPORTED.to_owned();
    let head = s
        .read_full((FILE_HEADER_LEN + 4) as usize)
        .map_err(|_| UNEXPECTED_EOF.to_owned())?;
    if &head[..2] != b"BM" {
        return Err("bmp: invalid format".to_owned());
    }
    let u32le = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let offset = u32le(head, 10);
    let info_len = u32le(head, 14);
    if info_len != INFO_HEADER_LEN && info_len != 108 && info_len != 124 {
        return Err(unsupported());
    }
    let rest = s
        .read_full((info_len - 4) as usize)
        .map_err(|_| UNEXPECTED_EOF.to_owned())?;
    // `b` in Go is the whole header from byte 0.
    let mut b = Vec::with_capacity((FILE_HEADER_LEN + info_len) as usize);
    b.extend_from_slice(head);
    b.extend_from_slice(rest);
    let u16le = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);

    let width = i64::from(u32le(&b, 18) as i32);
    let mut height = i64::from(u32le(&b, 22) as i32);
    if height < 0 {
        height = -height;
    }
    if width < 0 {
        return Err(unsupported());
    }
    if (width == 0) != (height == 0) {
        return Err(unsupported());
    }
    // safemath.Mul3(width, height, 4)
    if (width as u128) * (height as u128) * 4 > i64::MAX as u128 {
        return Err(unsupported());
    }
    let planes = u16le(26);
    let bpp = u16le(28);
    let mut compression = u32le(&b, 30);
    if compression == 3
        && info_len > INFO_HEADER_LEN
        && u32le(&b, 54) == 0x00ff_0000
        && u32le(&b, 58) == 0xff00
        && u32le(&b, 62) == 0xff
        && u32le(&b, 66) == 0xff00_0000
    {
        compression = 0;
    }
    if planes != 1 || compression != 0 {
        return Err(unsupported());
    }
    match bpp {
        1 | 2 | 4 | 8 => {
            let mut color_used = u32le(&b, 46);
            if color_used == 0 {
                color_used = 1 << bpp;
            } else if color_used > (1 << bpp) {
                return Err(unsupported());
            }
            if offset != FILE_HEADER_LEN + info_len + color_used * 4 {
                return Err(unsupported());
            }
            // Go does not map `io.EOF` here, so an absent palette reads "EOF".
            s.read_full((color_used * 4) as usize)
                .map_err(|eof| eof_text(eof).to_owned())?;
            Ok((width, height))
        }
        24 | 32 => {
            if offset != FILE_HEADER_LEN + info_len {
                return Err(unsupported());
            }
            Ok((width, height))
        }
        _ => Err(unsupported()),
    }
}

// ---------------------------------------------------------------------------------------------
// TIFF — golang.org/x/image/tiff/reader.go
// ---------------------------------------------------------------------------------------------

fn tiff_format(s: &str) -> String {
    format!("tiff: invalid format: {s}")
}

fn tiff_unsupported(s: &str) -> String {
    format!("tiff: unsupported feature: {s}")
}

/// The `ReadAt` error of tiff's `buffer`: `io.EOF` when the fill that ran out read nothing,
/// `io.ErrUnexpectedEOF` when it read something.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReadAtErr {
    Eof,
    UnexpectedEof,
}

impl ReadAtErr {
    fn text(self) -> String {
        eof_text(self == Self::Eof).to_owned()
    }
}

/// Port of tiff's `buffer` (buffer.go): a `ReaderAt` over a stream, filled lazily in 10 MiB
/// chunks. Only how far it has filled is state — the bytes are `data`'s.
struct TiffBuffer<'a> {
    data: &'a [u8],
    filled: usize,
}

impl<'a> TiffBuffer<'a> {
    /// `fill(end)`.
    fn fill(&mut self, end: usize) -> Result<(), ReadAtErr> {
        const FILL_CHUNK_SIZE: usize = 10 << 20;
        let mut m = self.filled;
        while m < end {
            let next = (end - m).min(FILL_CHUNK_SIZE);
            let available = self.data.len() - m;
            if available < next {
                self.filled = self.data.len();
                return Err(if available == 0 {
                    ReadAtErr::Eof
                } else {
                    ReadAtErr::UnexpectedEof
                });
            }
            m += next;
        }
        self.filled = self.filled.max(m);
        Ok(())
    }

    /// `ReadAt(p, off)` with `len(p) == n`: the bytes when every one was available.
    fn read_at(&mut self, n: usize, off: u64) -> Result<&'a [u8], ReadAtErr> {
        let end = off
            .checked_add(n as u64)
            .filter(|end| *end <= i64::MAX as u64)
            .ok_or(ReadAtErr::UnexpectedEof)?;
        let end = usize::try_from(end).map_err(|_| ReadAtErr::UnexpectedEof)?;
        self.fill(end)?;
        let off = off as usize;
        Ok(&self.data[off..end])
    }

    /// `safeReadAt` (reader.go:53) for the sizes this port can meet (always below the 10 MiB
    /// chunking threshold: at most 65,535 IFD entries of 12 bytes, or 769 values of 8).
    fn safe_read_at(&mut self, n: usize, off: u64) -> Result<&'a [u8], ReadAtErr> {
        match self.read_at(n, off) {
            Ok(bytes) => Ok(bytes),
            // "io.SectionReader can return EOF for n == 0, but for our purposes that is a
            // success."
            Err(ReadAtErr::Eof) if n == 0 => Ok(&[]),
            Err(err) => Err(err),
        }
    }
}

const TIFF_LENGTHS: [u32; 6] = [0, 1, 1, 2, 4, 8];

/// Port of tiff `DecodeConfig` → `newDecoder` (reader.go:519): the first IFD's entries, sorted,
/// and the checks on width, height, BitsPerSample and PhotometricInterpretation that decide
/// whether a decoder would be built at all.
fn tiff_config(data: &[u8]) -> Result<(i64, i64), String> {
    let mut r = TiffBuffer { data, filled: 0 };
    let p = r.read_at(8, 0).map_err(|_| UNEXPECTED_EOF.to_owned())?;
    let big_endian = match &p[..4] {
        b"II\x2a\x00" => false,
        b"MM\x00\x2a" => true,
        _ => return Err(tiff_format("malformed header")),
    };
    let u16_at = |b: &[u8], i: usize| {
        if big_endian {
            u16::from_be_bytes([b[i], b[i + 1]])
        } else {
            u16::from_le_bytes([b[i], b[i + 1]])
        }
    };
    let u32_at = |b: &[u8], i: usize| {
        if big_endian {
            u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
        } else {
            u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
        }
    };
    let ifd_offset = u64::from(u32_at(p, 4));
    let count = r.read_at(2, ifd_offset).map_err(ReadAtErr::text)?;
    let num_items = usize::from(u16_at(count, 0));
    let entries = r
        .safe_read_at(12 * num_items, ifd_offset + 2)
        .map_err(ReadAtErr::text)?;

    // features: tag → values (present-but-empty is distinct from absent).
    let mut features: std::collections::HashMap<u16, Vec<u64>> = std::collections::HashMap::new();
    let mut prev_tag = -1_i64;
    for entry in entries.chunks_exact(12) {
        let tag = u16_at(entry, 0);
        match tag {
            258 | 338 | 262 | 259 | 317 | 278 | 322 | 323 | 257 | 256 | 266 | 292 | 293 => {
                let values = tiff_ifd_uint(&mut r, entry, 16, &u16_at, &u32_at)?;
                features.insert(tag, values);
            }
            // StripOffsets, StripByteCounts, TileOffsets, TileByteCounts: stashed, not parsed.
            273 | 279 | 324 | 325 => {}
            320 => {
                let values = tiff_ifd_uint(&mut r, entry, 3 * 256 + 1, &u16_at, &u32_at)?;
                let colors = values.len() / 3;
                if values.len() % 3 != 0 || colors == 0 || colors > 256 {
                    return Err(tiff_format("bad ColorMap length"));
                }
            }
            339 => {
                let values = tiff_ifd_uint(&mut r, entry, 16, &u16_at, &u32_at)?;
                if values.iter().any(|v| *v != 1) {
                    return Err(tiff_unsupported("sample format"));
                }
            }
            _ => {}
        }
        if i64::from(tag) <= prev_tag {
            return Err(tiff_format("tags are not sorted in ascending order"));
        }
        prev_tag = i64::from(tag);
    }

    let first = |features: &std::collections::HashMap<u16, Vec<u64>>, tag: u16| {
        features
            .get(&tag)
            .and_then(|values| values.first().copied())
            .unwrap_or(0)
    };
    let width = first(&features, 256);
    let height = first(&features, 257);
    if width == 0 || height == 0 {
        return Err(tiff_format("zero-size image"));
    }
    // safemath.Mul3(width, height, 8)
    if u128::from(width) * u128::from(height) * 8 > i64::MAX as u128 {
        return Err(tiff_format("image too large"));
    }
    // "Default is 1 per specification" — only when the tag is absent, not when it is empty.
    let default_bits = [1_u64];
    let bits: &[u64] = features.get(&258).map_or(&default_bits, Vec::as_slice);
    let bpp = bits.first().copied().unwrap_or(0);
    match bpp {
        0 => return Err(tiff_format("BitsPerSample must not be 0")),
        1 | 8 | 16 => {}
        _ => return Err(tiff_unsupported(&format!("BitsPerSample of {bpp}"))),
    }
    let photometric = first(&features, 262);
    match photometric {
        // pRGB
        2 => {
            if bpp == 16 {
                if bits.iter().any(|b| *b != 16) {
                    return Err(tiff_format("wrong number of samples for 16bit RGB"));
                }
            } else if bits.iter().any(|b| *b != 8) {
                return Err(tiff_format("wrong number of samples for 8bit RGB"));
            }
            match bits.len() {
                3 => {}
                4 => match first(&features, 338) {
                    1 | 2 => {}
                    _ => return Err(tiff_format("wrong number of samples for RGB")),
                },
                _ => return Err(tiff_format("wrong number of samples for RGB")),
            }
        }
        // pPaletted, pWhiteIsZero, pBlackIsZero
        3 | 0 | 1 => {}
        _ => return Err(tiff_unsupported("color model")),
    }
    if photometric != 2 && bits.len() != 1 {
        return Err(tiff_unsupported("extra samples"));
    }
    Ok((width as i64, height as i64))
}

/// `ifdUint` (reader.go:127).
fn tiff_ifd_uint(
    r: &mut TiffBuffer<'_>,
    p: &[u8],
    max_count: usize,
    u16_at: &dyn Fn(&[u8], usize) -> u16,
    u32_at: &dyn Fn(&[u8], usize) -> u32,
) -> Result<Vec<u64>, String> {
    let datatype = u16_at(p, 2);
    if datatype == 0 || usize::from(datatype) >= TIFF_LENGTHS.len() {
        return Err(tiff_unsupported("IFD entry datatype"));
    }
    let size = TIFF_LENGTHS[usize::from(datatype)];
    let count = u32_at(p, 4);
    if count > (i32::MAX as u32) / size {
        return Err(tiff_format("IFD data too large"));
    }
    let truncated = (count as usize).min(max_count);
    let datalen = size * count;
    let raw: &[u8] = if datalen > 4 {
        r.safe_read_at(size as usize * truncated, u64::from(u32_at(p, 8)))
            .map_err(ReadAtErr::text)?
    } else {
        &p[8..8 + datalen as usize]
    };
    let mut values = Vec::with_capacity(truncated);
    match datatype {
        1 => values.extend(raw[..truncated].iter().map(|b| u64::from(*b))),
        3 => values.extend((0..truncated).map(|i| u64::from(u16_at(raw, 2 * i)))),
        4 => values.extend((0..truncated).map(|i| u64::from(u32_at(raw, 4 * i)))),
        _ => return Err(tiff_unsupported("data type")),
    }
    Ok(values)
}

// ---------------------------------------------------------------------------------------------
// WebP — golang.org/x/image/{webp,riff,vp8,vp8l}
// ---------------------------------------------------------------------------------------------

const WEBP_INVALID: &str = "webp: invalid format";

/// Port of `riff.Reader` (riff.go) over a byte stream, `chunkReader` included: the unread part
/// of the current chunk is `chunk_len`, and every read debits it and `total_len` together.
struct Riff<'a> {
    s: Stream<'a>,
    total_len: u32,
    chunk_len: u32,
    padded: bool,
}

impl<'a> Riff<'a> {
    /// `Next` (riff.go): discard the rest of the current chunk, its padding byte, then the next
    /// header. `Ok(None)` is `io.EOF`.
    fn next(&mut self) -> Result<Option<([u8; 4], u32)>, String> {
        if self.chunk_len != 0 {
            let want = self.chunk_len;
            let got = (want as usize).min(self.s.remaining()) as u32;
            self.s.pos += got as usize;
            self.total_len = self.total_len.wrapping_sub(got);
            self.chunk_len -= got;
            if got != want {
                return Err("riff: short chunk data".to_owned());
            }
        }
        if self.padded {
            if self.total_len == 0 {
                return Err("riff: list subchunk too long".to_owned());
            }
            self.total_len -= 1;
            self.s
                .read_full(1)
                .map_err(|_| "riff: missing padding byte".to_owned())?;
        }
        if self.total_len == 0 {
            return Ok(None);
        }
        if self.total_len < 8 {
            return Err("riff: short chunk header".to_owned());
        }
        self.total_len -= 8;
        let header = self
            .s
            .read_full(8)
            .map_err(|_| "riff: short chunk header".to_owned())?;
        let id = [header[0], header[1], header[2], header[3]];
        self.chunk_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if self.chunk_len > self.total_len {
            return Err("riff: list subchunk too long".to_owned());
        }
        self.padded = self.chunk_len & 1 == 1;
        Ok(Some((id, self.chunk_len)))
    }

    /// `io.ReadFull(chunkData, p)` with `len(p) == n`: bounded by the chunk and by the stream.
    /// `Err(true)` is `io.EOF`, `Err(false)` `io.ErrUnexpectedEOF`.
    fn chunk_read_full(&mut self, n: usize) -> Result<&'a [u8], bool> {
        let available = (self.chunk_len as usize).min(self.s.remaining());
        if available < n {
            self.s.pos += available;
            self.total_len = self.total_len.wrapping_sub(available as u32);
            self.chunk_len -= available as u32;
            return Err(available == 0);
        }
        let out = &self.s.data[self.s.pos..self.s.pos + n];
        self.s.pos += n;
        self.total_len = self.total_len.wrapping_sub(n as u32);
        self.chunk_len -= n as u32;
        Ok(out)
    }
}

/// Port of webp `DecodeConfig` → `decode(r, configOnly=true)` (decode.go): chunks until the
/// first VP8, VP8L or VP8X answers.
fn webp_config(data: &[u8]) -> Result<(i64, i64), String> {
    let mut s = Stream::new(data);
    let header = s
        .read_full(8)
        .map_err(|_| "riff: missing RIFF chunk header".to_owned())?;
    let chunk_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if chunk_len < 4 {
        return Err("riff: short chunk data".to_owned());
    }
    // The form type — "WEBP", already matched by the magic.
    s.read_full(4)
        .map_err(|_| "riff: short chunk data".to_owned())?;
    let mut riff = Riff {
        s,
        total_len: chunk_len - 4,
        chunk_len: 0,
        padded: false,
    };

    loop {
        let Some((id, chunk_len)) = riff.next()? else {
            return Err(WEBP_INVALID.to_owned());
        };
        match &id {
            // ALPH before VP8X: `wantAlpha` is still false.
            b"ALPH" => return Err(WEBP_INVALID.to_owned()),
            b"VP8 " => {
                if (chunk_len as i32) < 0 {
                    return Err(WEBP_INVALID.to_owned());
                }
                // vp8's limitReader refuses a read longer than the chunk before reading.
                if chunk_len < 3 {
                    return Err(UNEXPECTED_EOF.to_owned());
                }
                let b = riff
                    .chunk_read_full(3)
                    .map_err(|eof| eof_text(eof).to_owned())?;
                if b[0] & 1 != 0 {
                    // Not a key frame: the header carries no dimensions.
                    return Ok((0, 0));
                }
                if chunk_len - 3 < 7 {
                    return Err(UNEXPECTED_EOF.to_owned());
                }
                let b = riff
                    .chunk_read_full(7)
                    .map_err(|eof| eof_text(eof).to_owned())?;
                if b[0] != 0x9d || b[1] != 0x01 || b[2] != 0x2a {
                    return Err("vp8: invalid format".to_owned());
                }
                let width = (i64::from(b[4] & 0x3f) << 8) | i64::from(b[3]);
                let height = (i64::from(b[6] & 0x3f) << 8) | i64::from(b[5]);
                return Ok((width, height));
            }
            b"VP8L" => return vp8l_config(&mut riff),
            b"VP8X" => {
                if chunk_len != 10 {
                    return Err(WEBP_INVALID.to_owned());
                }
                let b = riff
                    .chunk_read_full(10)
                    .map_err(|eof| eof_text(eof).to_owned())?;
                let width_minus_one =
                    u32::from(b[4]) | (u32::from(b[5]) << 8) | (u32::from(b[6]) << 16);
                let height_minus_one =
                    u32::from(b[7]) | (u32::from(b[8]) << 8) | (u32::from(b[9]) << 16);
                let w = u64::from(width_minus_one) + 1;
                let h = u64::from(height_minus_one) + 1;
                if w * h > (1 << 31) - 1 {
                    return Err(WEBP_INVALID.to_owned());
                }
                return Ok((w as i64, h as i64));
            }
            _ => {}
        }
    }
}

/// `vp8l.DecodeConfig` → `decodeHeader` (vp8l/decode.go:505), reading the chunk bit by bit.
fn vp8l_config(riff: &mut Riff<'_>) -> Result<(i64, i64), String> {
    let mut bits = 0_u32;
    let mut n_bits = 0_u32;
    let mut read = |n: u32| -> Result<u32, String> {
        while n_bits < n {
            let c = riff
                .chunk_read_full(1)
                .map_err(|_| UNEXPECTED_EOF.to_owned())?;
            bits |= u32::from(c[0]) << n_bits;
            n_bits += 8;
        }
        let u = bits & ((1 << n) - 1);
        bits >>= n;
        n_bits -= n;
        Ok(u)
    };
    if read(8)? != 0x2f {
        return Err("vp8l: invalid header".to_owned());
    }
    let width = read(14)? + 1;
    let height = read(14)? + 1;
    read(1)?;
    if read(3)? != 0 {
        return Err("vp8l: invalid version".to_owned());
    }
    Ok((i64::from(width), i64::from(height)))
}

// ---------------------------------------------------------------------------------------------
// JPEG EXIF orientation — imaging.GetImageOrientation over bep/imagemeta@v0.17.2
// ---------------------------------------------------------------------------------------------

/// `maxExifScanSize` (imaging/orientation.go): `bufReadSeeker` refuses to buffer past 10 MiB.
const MAX_EXIF_SCAN_SIZE: usize = 10 * 1024 * 1024;
/// imagemeta's `defaultLimitNumTags`.
const EXIF_LIMIT_NUM_TAGS: u32 = 5000;
/// imagemeta's `defaultLimitTagSize`.
const EXIF_LIMIT_TAG_SIZE: u32 = 10000;

/// Port of `imaging.GetImageOrientation(r, "jpeg")` as `parseImages` consumes it: `Ok(Some(v))`
/// when the walk handled an `Orientation` tag whose value is a `uint16`, `Ok(None)` for every
/// other outcome (Go's `Upright`, with or without an error, both of which leave the dimensions
/// alone), and `Err` for the one shape this cannot follow.
///
/// # The walk, as imagemeta does it with `Sources: EXIF`
///
/// Only the **first** APP1 (0xFFE1) segment is examined — EXIF is then removed from the source
/// set and the JPEG decoder stops. Its payload must open `Exif`; then a TIFF header, IFD0, and
/// IFD1, following the SubIFD / ExifIFD / GPSInfoIFD / InteroperabilityIFD pointers (each at
/// most once, keyed by name) wherever they appear. Every non-pointer tag of at most 10,000
/// bytes counts towards a 5,000-tag limit, and the tag `0x0112` found **anywhere** — IFD0, IFD1
/// or a sub-IFD — ends the walk.
///
/// The EXIF segment is read through imagemeta's `streamReader`, whose `stop` does not panic on
/// the **first** `io.EOF`: the read returns whatever its scratch buffer last held. That stale
/// read is reproduced ([`ExifReader`]), because a tag count or offset read at the very end of
/// the segment really is the previous value.
///
/// # The one refusal
///
/// A segment skip that lands past 10 MiB makes `bufReadSeeker.Seek` fail *after* `bufio` has
/// prefetched an amount this port cannot know, leaving the stream at an unknowable position.
/// It needs at least ~10.4 MB of input before the first APP1, and is refused rather than
/// guessed.
pub fn jpeg_orientation(data: &[u8]) -> Result<Option<u16>, &'static str> {
    // Reads past the scan limit fail; so does a short stream. Either way the walk ends without
    // an orientation, which is all this function reports.
    let readable = &data[..data.len().min(MAX_EXIF_SCAN_SIZE)];
    let mut s = Stream::new(readable);
    // read2E: an error (or a non-SOI) returns nil — Upright.
    let Ok(soi) = s.read_full(2) else {
        return Ok(None);
    };
    if soi != [0xff, 0xd8] {
        return Ok(None);
    }
    loop {
        // Any failed outer read ends the walk with no orientation: the first `io.EOF` returns
        // `nil` through the `isEOF` test, and every later one panics.
        let Ok(marker) = s.read_full(2) else {
            return Ok(None);
        };
        let marker = u16::from_be_bytes([marker[0], marker[1]]);
        if marker == 0 {
            continue;
        }
        if marker == 0xffda {
            return Ok(None);
        }
        let Ok(length) = s.read_full(2) else {
            return Ok(None);
        };
        let length = u16::from_be_bytes([length[0], length[1]]);
        if length < 2 {
            // errInvalidFormat.
            return Ok(None);
        }
        let length = usize::from(length - 2);
        if marker == 0xffe1 {
            let Ok(segment) = s.read_full(length) else {
                return Ok(None);
            };
            return Ok(exif_orientation(segment));
        }
        // skip: a seek that never fails the walk; past the data it clamps to the end.
        let target = s.pos + length;
        if target > MAX_EXIF_SCAN_SIZE {
            return Err("a JPEG segment skip past the 10 MiB EXIF scan limit");
        }
        s.pos = target.min(readable.len());
    }
}

/// Why the EXIF walk stopped without an orientation — imagemeta's `panic(errStop)` and every
/// error the decoder returns look the same from `parseImages`.
struct ExifStop;

/// The two readers imagemeta's EXIF decoder reads from: the segment itself, and a value
/// buffer (`bufferedReader`) for a tag whose value does not fit in four bytes.
#[derive(Clone, Copy)]
enum Src {
    Segment,
    Value,
}

/// Port of imagemeta's `streamReader` over one EXIF segment (`bytes.Reader` semantics: seeks
/// past the end are allowed, reads there return `io.EOF`).
struct ExifReader<'a> {
    seg: &'a [u8],
    pos: u64,
    value: &'a [u8],
    value_pos: usize,
    big_endian: bool,
    /// `e.buf`: only ever reallocated (zeroed) when a read needs more than its length.
    buf: Vec<u8>,
    is_eof: bool,
    reader_offset: u64,
    tag_count: u32,
    seen_ifds: Vec<u16>,
}

impl<'a> ExifReader<'a> {
    /// `readNFromRIntoBuf`: the stale-first-EOF rule lives here.
    fn read(&mut self, n: usize, src: Src) -> Result<&[u8], ExifStop> {
        if n > self.buf.len() {
            self.buf = vec![0; n];
        }
        let (data, pos) = match src {
            Src::Segment => (self.seg, usize::try_from(self.pos).unwrap_or(usize::MAX)),
            Src::Value => (self.value, self.value_pos),
        };
        let available = data.len().saturating_sub(pos);
        if available == 0 {
            // io.ReadFull → io.EOF → `stop(io.EOF)`: the first time, no panic and a stale buf.
            if !self.is_eof {
                self.is_eof = true;
                return Ok(&self.buf[..n]);
            }
            return Err(ExifStop);
        }
        if available < n {
            return Err(ExifStop);
        }
        self.buf[..n].copy_from_slice(&data[pos..pos + n]);
        match src {
            Src::Segment => self.pos += n as u64,
            Src::Value => self.value_pos += n,
        }
        Ok(&self.buf[..n])
    }

    fn u16(&mut self, src: Src) -> Result<u16, ExifStop> {
        let big = self.big_endian;
        let b = self.read(2, src)?;
        Ok(if big {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        })
    }

    fn u32(&mut self, src: Src) -> Result<u32, ExifStop> {
        let big = self.big_endian;
        let b = self.read(4, src)?;
        Ok(if big {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    /// `skip`: `Seek(n, SeekCurrent)` with the error ignored — a negative target is refused and
    /// the position kept.
    fn skip(&mut self, n: i64) {
        if let Some(target) = self.pos.checked_add_signed(n) {
            self.pos = target;
        }
    }

    /// `seek` from a non-negative offset: never fails on a `bytes.Reader`.
    fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }
}

/// The value of one tag, reduced to the shapes the walk distinguishes.
enum ExifValue {
    U16(u16),
    U32s(Vec<u32>),
    Other,
}

fn exif_type_size(typ: u16) -> Option<u32> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// `handleEXIF` + `metaDecoderEXIF.decode` over one APP1 payload.
fn exif_orientation(segment: &[u8]) -> Option<u16> {
    let mut e = ExifReader {
        seg: segment,
        pos: 0,
        value: &[],
        value_pos: 0,
        // The JPEG decoder's byte order, which is big-endian.
        big_endian: true,
        buf: Vec::new(),
        is_eof: false,
        reader_offset: 0,
        tag_count: 0,
        seen_ifds: Vec::new(),
    };
    exif_walk(&mut e).ok().flatten()
}

fn exif_walk(e: &mut ExifReader<'_>) -> Result<Option<u16>, ExifStop> {
    if e.u32(Src::Segment)? != 0x4578_6966 {
        return Ok(None);
    }
    e.skip(2);
    e.reader_offset = e.pos;
    let byte_order = e.u16(Src::Segment)?;
    match byte_order {
        0x4d4d => e.big_endian = true,
        0x4949 => e.big_endian = false,
        _ => return Ok(None),
    }
    e.skip(2);
    let ifd0 = e.u32(Src::Segment)?;
    if ifd0 < 8 {
        return Ok(None);
    }
    e.skip(i64::from(ifd0 - 8));
    if let Some(found) = exif_tags(e)? {
        return Ok(Some(found));
    }
    let ifd1 = e.u32(Src::Segment)?;
    if ifd1 == 0 {
        return Ok(None);
    }
    e.seek(u64::from(ifd1) + e.reader_offset);
    exif_tags(e)
}

/// `decodeTags`: `Ok(Some)` is the orientation (`ErrStopWalking`).
fn exif_tags(e: &mut ExifReader<'_>) -> Result<Option<u16>, ExifStop> {
    let n = e.u16(Src::Segment)?;
    for _ in 0..n {
        if let Some(found) = exif_tag(e)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// `decodeTag` (metadecoder_exif.go).
fn exif_tag(e: &mut ExifReader<'_>) -> Result<Option<u16>, ExifStop> {
    let tag_id = e.u16(Src::Segment)?;
    let data_type = e.u16(Src::Segment)?;
    let count = e.u32(Src::Segment)?;
    if count > 0x10000 {
        e.skip(4);
        return Ok(None);
    }
    let is_pointer = matches!(tag_id, 0x014a | 0x8769 | 0x8825 | 0xa005);
    if is_pointer {
        if e.seen_ifds.contains(&tag_id) {
            return Ok(None);
        }
        e.seen_ifds.push(tag_id);
    }
    // "unknown EXIF type" is an error that ends the walk.
    let size = exif_type_size(data_type).ok_or(ExifStop)?;
    let val_len = size * count;
    // XMP (0x02bc) and IPTC (0x83bb) are not requested sources.
    if tag_id == 0x02bc || tag_id == 0x83bb || val_len > EXIF_LIMIT_TAG_SIZE {
        e.skip(4);
        return Ok(None);
    }
    if !is_pointer {
        e.tag_count += 1;
        if e.tag_count > EXIF_LIMIT_NUM_TAGS {
            // panic(ErrStopWalking): the walk ends, Upright.
            return Err(ExifStop);
        }
        if tag_id != 0x0112 {
            e.skip(4);
            return Ok(None);
        }
    }

    let value = if val_len > 4 {
        let offset = e.u32(Src::Segment)?;
        let offset = u64::from(offset.wrapping_add(e.reader_offset as u32));
        let old = e.pos;
        e.seek(offset);
        // bufferedReader: io.ReadFull of val_len from the segment, or the tag fails.
        let start = usize::try_from(e.pos).unwrap_or(usize::MAX);
        let end = start.saturating_add(val_len as usize);
        let Some(bytes) = e.seg.get(start..end) else {
            return Err(ExifStop);
        };
        e.value = bytes;
        e.value_pos = 0;
        e.pos = end as u64;
        let value = exif_values(e, data_type, count, val_len, Src::Value);
        e.seek(old);
        value?
    } else {
        let value = exif_values(e, data_type, count, val_len, Src::Segment)?;
        if val_len < 4 {
            e.skip(i64::from(4 - val_len));
        }
        value
    };

    if is_pointer {
        let offsets = match value {
            ExifValue::U32s(offsets) => offsets,
            // "invalid IFD pointer value".
            _ => return Err(ExifStop),
        };
        for offset in offsets {
            let old = e.pos;
            e.seek(u64::from(offset) + e.reader_offset);
            let found = exif_tags(e)?;
            e.seek(old);
            if found.is_some() {
                return Ok(found);
            }
        }
        return Ok(None);
    }

    // The Orientation tag: handled only when its value is a uint16.
    match value {
        ExifValue::U16(v) => Ok(Some(v)),
        _ => Ok(None),
    }
}

/// `convertValues`, reading exactly as Go reads (one read per value, one read for a string),
/// so the stale-EOF rule applies at the same reads.
fn exif_values(
    e: &mut ExifReader<'_>,
    typ: u16,
    count: u32,
    val_len: u32,
    src: Src,
) -> Result<ExifValue, ExifStop> {
    if count == 0 {
        return Ok(ExifValue::Other);
    }
    if typ == 2 {
        e.read(val_len as usize, src)?;
        return Ok(ExifValue::Other);
    }
    let mut u32s = Vec::new();
    let mut single_u16 = None;
    for _ in 0..count {
        match typ {
            1 | 6 | 7 => {
                e.read(1, src)?;
            }
            3 | 8 => single_u16 = Some(e.u16(src)?),
            4 => u32s.push(e.u32(src)?),
            9 | 11 => {
                e.read(4, src)?;
            }
            5 | 10 => {
                e.read(4, src)?;
                e.read(4, src)?;
            }
            _ => {
                e.read(8, src)?;
            }
        }
    }
    Ok(match typ {
        3 | 8 if count == 1 => single_u16.map_or(ExifValue::Other, ExifValue::U16),
        4 => ExifValue::U32s(u32s),
        _ => ExifValue::Other,
    })
}

// ---------------------------------------------------------------------------------------------
// net/http.DetectContentType
// ---------------------------------------------------------------------------------------------

/// Port of `peekContentType` (post_metadata.go:1120) over a byte slice: `Peek(512)` on a
/// `bufio.Reader` can only fail with `ErrBufferFull` (impossible at 512 of a 4096 buffer) or
/// `io.EOF` (a short body), and both are tolerated, so its `""` branch is unreachable here and
/// this is exactly [`detect_content_type`].
pub fn peek_content_type(body: &[u8]) -> &'static str {
    detect_content_type(body)
}

/// Port of `http.DetectContentType` (net/http/sniff.go): the WHATWG MIME sniffing subset Go
/// implements, over the first 512 bytes, falling back to `application/octet-stream`.
pub fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(512)];
    let first_non_ws = data
        .iter()
        .position(|b| !matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' '))
        .unwrap_or(data.len());

    const HTML: [&[u8]; 17] = [
        b"<!DOCTYPE HTML",
        b"<HTML",
        b"<HEAD",
        b"<SCRIPT",
        b"<IFRAME",
        b"<H1",
        b"<DIV",
        b"<FONT",
        b"<TABLE",
        b"<A",
        b"<STYLE",
        b"<TITLE",
        b"<B",
        b"<BODY",
        b"<BR",
        b"<P",
        b"<!--",
    ];
    let ws_skipped = &data[first_non_ws..];
    for sig in HTML {
        if ws_skipped.len() < sig.len() + 1 {
            continue;
        }
        let matches = sig.iter().zip(ws_skipped).all(|(&b, &db)| {
            let db = if b.is_ascii_uppercase() {
                db & 0xdf
            } else {
                db
            };
            b == db
        });
        if matches && matches!(ws_skipped[sig.len()], b' ' | b'>') {
            return "text/html; charset=utf-8";
        }
    }

    let masked = |data: &[u8], mask: &[u8], pat: &[u8]| {
        data.len() >= pat.len()
            && pat
                .iter()
                .zip(mask)
                .zip(data)
                .all(|((p, m), d)| d & m == *p)
    };
    if masked(ws_skipped, b"\xFF\xFF\xFF\xFF\xFF", b"<?xml") {
        return "text/xml; charset=utf-8";
    }

    enum Sig {
        Exact(&'static [u8], &'static str),
        Masked(&'static [u8], &'static [u8], &'static str),
        Mp4,
    }
    const SIGS: &[Sig] = &[
        Sig::Exact(b"%PDF-", "application/pdf"),
        Sig::Exact(b"%!PS-Adobe-", "application/postscript"),
        Sig::Masked(
            b"\xFF\xFF\x00\x00",
            b"\xFE\xFF\x00\x00",
            "text/plain; charset=utf-16be",
        ),
        Sig::Masked(
            b"\xFF\xFF\x00\x00",
            b"\xFF\xFE\x00\x00",
            "text/plain; charset=utf-16le",
        ),
        Sig::Masked(
            b"\xFF\xFF\xFF\x00",
            b"\xEF\xBB\xBF\x00",
            "text/plain; charset=utf-8",
        ),
        Sig::Exact(b"\x00\x00\x01\x00", "image/x-icon"),
        Sig::Exact(b"\x00\x00\x02\x00", "image/x-icon"),
        Sig::Exact(b"BM", "image/bmp"),
        Sig::Exact(b"GIF87a", "image/gif"),
        Sig::Exact(b"GIF89a", "image/gif"),
        Sig::Masked(
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
            b"RIFF\x00\x00\x00\x00WEBPVP",
            "image/webp",
        ),
        Sig::Exact(b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
        Sig::Exact(b"\xFF\xD8\xFF", "image/jpeg"),
        Sig::Masked(
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            b"FORM\x00\x00\x00\x00AIFF",
            "audio/aiff",
        ),
        Sig::Masked(b"\xFF\xFF\xFF", b"ID3", "audio/mpeg"),
        Sig::Masked(b"\xFF\xFF\xFF\xFF\xFF", b"OggS\x00", "application/ogg"),
        Sig::Masked(
            b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
            b"MThd\x00\x00\x00\x06",
            "audio/midi",
        ),
        Sig::Masked(
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            b"RIFF\x00\x00\x00\x00AVI ",
            "video/avi",
        ),
        Sig::Masked(
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            b"RIFF\x00\x00\x00\x00WAVE",
            "audio/wave",
        ),
        Sig::Mp4,
        Sig::Exact(b"\x1A\x45\xDF\xA3", "video/webm"),
        Sig::Masked(
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xFF",
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00LP",
            "application/vnd.ms-fontobject",
        ),
        Sig::Exact(b"\x00\x01\x00\x00", "font/ttf"),
        Sig::Exact(b"OTTO", "font/otf"),
        Sig::Exact(b"ttcf", "font/collection"),
        Sig::Exact(b"wOFF", "font/woff"),
        Sig::Exact(b"wOF2", "font/woff2"),
        Sig::Exact(b"\x1F\x8B\x08", "application/x-gzip"),
        Sig::Exact(b"PK\x03\x04", "application/zip"),
        Sig::Exact(b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
        Sig::Exact(b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
        Sig::Exact(b"\x00\x61\x73\x6D", "application/wasm"),
    ];
    for sig in SIGS {
        match sig {
            Sig::Exact(prefix, ct) => {
                if data.starts_with(prefix) {
                    return ct;
                }
            }
            Sig::Masked(mask, pat, ct) => {
                if masked(data, mask, pat) {
                    return ct;
                }
            }
            Sig::Mp4 => {
                if is_mp4(data) {
                    return "video/mp4";
                }
            }
        }
    }

    // textSig, last: no byte in the control ranges after the leading whitespace.
    if ws_skipped.iter().any(|&b| {
        b <= 0x08 || b == 0x0b || (0x0e..=0x1a).contains(&b) || (0x1c..=0x1f).contains(&b)
    }) {
        return "application/octet-stream";
    }
    "text/plain; charset=utf-8"
}

/// `mp4Sig.match` (sniff.go).
fn is_mp4(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    let box_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < box_size || box_size % 4 != 0 {
        return false;
    }
    if &data[4..8] != b"ftyp" {
        return false;
    }
    let mut st = 8;
    while st < box_size {
        if st != 12 && &data[st..st + 3] == b"mp4" {
            return true;
        }
        st += 4;
    }
    false
}

#[cfg(test)]
mod go_parity {
    //! Every case of `fixtures/behaviour_link_image.json`, generated by
    //! `reference/dump/behaviour_link_image.go` from Go's own decoders.
    use super::*;
    use base64::Engine;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_link_image.json"))
            .expect("the fixture is JSON")
    }

    fn input(case: &serde_json::Value) -> Vec<u8> {
        case["input"].as_str().map_or_else(Vec::new, |b64| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("base64")
        })
    }

    fn cases(section: &str) -> Vec<serde_json::Value> {
        fixture()[section].as_array().expect("an array").clone()
    }

    #[test]
    fn parse_images_matches_go() {
        let cases = cases("images");
        assert!(cases.len() >= 250, "the corpus walks every decoder");
        for case in &cases {
            let name = case["name"].as_str().expect("a name");
            let data = input(case);
            match parse_images(&data) {
                ImageProbe::Image(image) => {
                    assert_eq!(case["kind"], "image", "{name}: we answered {image:?}");
                    assert_eq!(image.width, case["width"].as_i64().unwrap(), "{name}");
                    assert_eq!(image.height, case["height"].as_i64().unwrap(), "{name}");
                    assert_eq!(image.format, case["format"].as_str().unwrap(), "{name}");
                    assert_eq!(
                        image.frame_count,
                        case["frame_count"].as_i64().unwrap(),
                        "{name}"
                    );
                }
                ImageProbe::Nil => assert_eq!(case["kind"], "nil", "{name}"),
                ImageProbe::Error(err) => {
                    assert_eq!(case["kind"], "error", "{name}: we failed with {err}");
                    assert_eq!(err, case["error"].as_str().unwrap(), "{name}");
                }
                ImageProbe::Unreproducible(reason) => {
                    panic!("{name}: no corpus case may be unreproducible, got {reason}")
                }
            }
        }
    }

    #[test]
    fn decode_config_matches_go() {
        for case in cases("images") {
            let name = case["name"].as_str().unwrap();
            let go_error = case["config_error"].as_str().unwrap();
            match decode_config(&input(&case)) {
                Ok(config) => {
                    assert_eq!(go_error, "", "{name}: we decoded {config:?}");
                    assert_eq!(
                        config.format,
                        case["config_format"].as_str().unwrap(),
                        "{name}"
                    );
                    assert_eq!(
                        config.width,
                        case["config_width"].as_i64().unwrap(),
                        "{name}"
                    );
                    assert_eq!(
                        config.height,
                        case["config_height"].as_i64().unwrap(),
                        "{name}"
                    );
                }
                Err(err) => assert_eq!(err, go_error, "{name}"),
            }
        }
    }

    /// `GetImageOrientation` answers `(orientation, nil)` when the tag is handled and
    /// `(Upright, …)` otherwise; this port reports only the first.
    #[test]
    fn jpeg_orientation_matches_go() {
        let mut jpegs = 0;
        for case in cases("images") {
            if case["config_format"] != "jpeg" {
                continue;
            }
            jpegs += 1;
            let name = case["name"].as_str().unwrap();
            let ours = jpeg_orientation(&input(&case)).expect("never unreproducible here");
            assert_eq!(
                i64::from(ours.unwrap_or(1)),
                case["orientation"].as_i64().unwrap(),
                "{name}"
            );
            if ours.is_some() {
                assert_eq!(case["orientation_error"], false, "{name}");
            }
        }
        assert!(jpegs >= 70, "{jpegs}");
    }

    #[test]
    fn gif_frame_count_matches_go() {
        let mut gifs = 0;
        for case in cases("images") {
            if case["config_format"] != "gif" {
                continue;
            }
            gifs += 1;
            let name = case["name"].as_str().unwrap();
            let go_error = case["frames_error"].as_str().unwrap();
            match count_gif_frames(&input(&case)) {
                Ok(n) => {
                    assert_eq!(go_error, "", "{name}: we counted {n}");
                    assert_eq!(n, case["frames"].as_i64().unwrap(), "{name}");
                }
                Err(err) => assert_eq!(err, go_error, "{name}"),
            }
        }
        assert!(gifs >= 35, "{gifs}");
    }

    #[test]
    fn detect_content_type_matches_go() {
        let cases = cases("sniff");
        assert!(cases.len() >= 130);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert_eq!(
                detect_content_type(&input(&case)),
                case["type"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    /// The corpus is not degenerate: every outcome and every format occurs.
    #[test]
    fn the_corpus_reaches_every_outcome() {
        let cases = cases("images");
        for kind in ["image", "nil", "error"] {
            assert!(cases.iter().any(|c| c["kind"] == kind), "{kind}");
        }
        for format in ["png", "jpeg", "gif", "bmp", "tiff", "webp"] {
            assert!(
                cases
                    .iter()
                    .any(|c| c["kind"] == "image" && c["format"] == format || format == "tiff"),
                "{format}"
            );
            assert!(
                cases
                    .iter()
                    .any(|c| c["config_format"] == format && c["config_error"] != ""),
                "{format} has a rejected case"
            );
        }
        // Swapped and unswapped orientations both occur.
        assert!(
            cases
                .iter()
                .any(|c| c["format"] == "jpeg" && c["orientation"] == 6 && c["width"] == 3)
        );
        assert!(cases.iter().any(|c| c["frame_count"].as_i64() == Some(3)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skip_past_the_exif_scan_limit_is_unreproducible() {
        // SOI, then 170 APP2 segments of 65,535 bytes: past 10 MiB before any APP1.
        let mut data = vec![0xff, 0xd8];
        for _ in 0..170 {
            data.extend_from_slice(&[0xff, 0xe2, 0xff, 0xff]);
            data.extend(std::iter::repeat_n(0_u8, 65_533));
        }
        assert!(jpeg_orientation(&data).is_err());
        // And parse_images turns that into a forward, not a guess — given a decodable header.
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend_from_slice(&[
            0xff, 0xc0, 0x00, 0x11, 8, 0, 3, 0, 4, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1,
        ]);
        jpeg.extend_from_slice(&data[2..]);
        jpeg.extend_from_slice(&[0xff, 0xda, 0x00, 0x02]);
        assert!(matches!(parse_images(&jpeg), ImageProbe::Unreproducible(_)));
    }

    #[test]
    fn a_short_input_is_an_unknown_format() {
        assert_eq!(decode_config(b"").unwrap_err(), "image: unknown format");
        assert_eq!(
            decode_config(b"\x89PNG").unwrap_err(),
            "image: unknown format"
        );
        assert_eq!(decode_config(b"\xff").unwrap_err(), "image: unknown format");
    }

    #[test]
    fn go_quote_escapes_like_strconv_quote() {
        assert_eq!(go_quote(b"GIF8xa"), "\"GIF8xa\"");
        assert_eq!(go_quote(b"GIF8\x00a"), "\"GIF8\\x00a\"");
        assert_eq!(go_quote(b"GIF8\xffa"), "\"GIF8\\xffa\"");
        assert_eq!(go_quote(b"GIF8\"a"), "\"GIF8\\\"a\"");
        assert_eq!(go_quote(b"GIF8\ta"), "\"GIF8\\ta\"");
    }

    #[test]
    fn peek_content_type_is_detect_content_type() {
        for data in [&b""[..], b"<html>", b"\x89PNG\r\n\x1a\n", b"\x00\x01"] {
            assert_eq!(peek_content_type(data), detect_content_type(data));
        }
    }

    #[test]
    fn only_the_first_512_bytes_are_sniffed() {
        let mut data = vec![b'a'; 600];
        data[520] = 1;
        assert_eq!(detect_content_type(&data), "text/plain; charset=utf-8");
        data[100] = 1;
        assert_eq!(detect_content_type(&data), "application/octet-stream");
    }

    #[test]
    fn a_tiff_decodes_to_nil_not_an_image() {
        let tiff = b"II\x2a\x00\x08\x00\x00\x00\x02\x00\x00\x01\x03\x00\x01\x00\x00\x00\x07\x00\x00\x00\x01\x01\x03\x00\x01\x00\x00\x00\x09\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(parse_images(tiff), ImageProbe::Nil);
        assert_eq!(
            decode_config(tiff).unwrap(),
            DecodedConfig {
                format: "tiff",
                width: 7,
                height: 9
            }
        );
    }
}
