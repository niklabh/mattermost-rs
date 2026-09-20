//! Port of `golang.org/x/image/vp8` (RFC 6386): the VP8 lossy still-image decoder, which always
//! produces a 4:2:0 `*image.YCbCr`.
//!
//! Decoding one frame is `Decoder::new`, [`Decoder::decode_frame_header`] and then
//! [`Decoder::decode_frame`], in that order, exactly as Go's `Init`/`DecodeFrameHeader`/
//! `DecodeFrame`. Inter-frames (Golden / AltRef prediction) are not implemented in the Go package
//! either; they are video, not still images, and both answer
//! `vp8: Golden / AltRef frames are not implemented`.

pub mod filter;
pub mod idct;
pub mod partition;
pub mod pred;
pub mod predfunc;
pub mod quant;
pub mod reconstruct;
pub mod token;

use filter::FilterParam;
use partition::{Partition, UNIFORM_PROB};
use quant::Quant;
use reconstruct::btou;
use token::{DEFAULT_TOKEN_PROB, N_BAND, N_CONTEXT, N_PLANE, N_PROB};

use crate::goread::{self, Read as GoRead};
use crate::image::{Ratio, Rect, YCbCr};

/// Every error value the VP8 decoder can produce, rendered with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// decode.go:170: the three-byte key-frame sync code is wrong.
    #[error("vp8: invalid format")]
    InvalidFormat,
    /// decode.go:275: a coefficient partition of 16 MiB or more.
    #[error("vp8: too much data to decode")]
    TooMuchData,
    /// decode.go:311: an inter-frame, which is video and not a still image.
    #[error("vp8: Golden / AltRef frames are not implemented")]
    GoldenAltRef,
    /// An error from the underlying reader.
    #[error(transparent)]
    Io(#[from] goread::Error),
}

/// `FrameHeader`, as specified in section 9.1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameHeader {
    pub key_frame: bool,
    pub version_number: u8,
    pub show_frame: bool,
    pub first_partition_len: u32,
    pub width: i32,
    pub height: i32,
    pub x_scale: u8,
    pub y_scale: u8,
}

const N_SEGMENT: usize = 4;
const N_SEGMENT_PROB: usize = 3;

/// `segmentHeader`.
#[derive(Clone, Copy, Debug)]
struct SegmentHeader {
    use_segment: bool,
    update_map: bool,
    relative_delta: bool,
    quantizer: [i8; N_SEGMENT],
    filter_strength: [i8; N_SEGMENT],
    prob: [u8; N_SEGMENT_PROB],
}

impl Default for SegmentHeader {
    fn default() -> Self {
        SegmentHeader {
            use_segment: false,
            update_map: false,
            relative_delta: false,
            quantizer: [0; N_SEGMENT],
            filter_strength: [0; N_SEGMENT],
            prob: [0; N_SEGMENT_PROB],
        }
    }
}

const N_REF_LF_DELTA: usize = 4;
const N_MODE_LF_DELTA: usize = 4;

/// `filterHeader`.
#[derive(Clone, Copy, Debug, Default)]
struct FilterHeader {
    simple: bool,
    level: i8,
    sharpness: u8,
    use_lf_delta: bool,
    ref_lf_delta: [i8; N_REF_LF_DELTA],
    mode_lf_delta: [i8; N_MODE_LF_DELTA],
    /// Written by `parseFilterHeader` and read by nothing, here as in Go: `computeFilterParams`
    /// recomputes the same value from `segmentHeader.filterStrength`.
    #[allow(dead_code)]
    per_segment_level: [i8; N_SEGMENT],
}

/// `mb`: the per-macroblock decode state kept for the row above and the macroblock to the left.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Mb {
    /// The predictor mode for the four bottom or right 4x4 luma regions.
    pred: [u8; 4],
    /// Four bits for the bottom or right 4x4 luma regions and 2 + 2 for the chroma ones; a set bit
    /// means that region has non-zero coefficients.
    nz_mask: u8,
    /// 1 if the macroblock used Y16 prediction and had non-zero coefficients.
    nz_y16: u8,
}

/// `limitReader`: reads at most `n` bytes.
struct LimitReader<'r> {
    r: &'r mut dyn GoRead,
    n: usize,
}

impl LimitReader<'_> {
    /// `limitReader.ReadFull`.
    fn read_full(&mut self, p: &mut [u8]) -> Result<(), goread::Error> {
        if p.len() > self.n {
            return Err(goread::Error::UnexpectedEof);
        }
        let (n, err) = goread::read_full(&mut *self.r, p);
        self.n -= n;
        match err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// `limitReader.ReadFull` into a fresh buffer. Go sizes `make` from the caller's count, which
    /// comes from an unverified chunk header; this grows in steps instead, so a header that claims
    /// 16 MiB over a 2 KiB file costs a megabyte rather than the claim. The result is identical:
    /// either the whole buffer, or `io.ReadFull`'s error.
    fn read_full_alloc(&mut self, n: usize) -> Result<Vec<u8>, goread::Error> {
        if n > self.n {
            return Err(goread::Error::UnexpectedEof);
        }
        const STEP: usize = 1 << 20;
        let mut v: Vec<u8> = Vec::new();
        let mut total = 0usize;
        while v.len() < n {
            let base = v.len();
            v.resize(base + (n - base).min(STEP), 0);
            let (got, err) = goread::read_full(&mut *self.r, &mut v[base..]);
            self.n -= got;
            total += got;
            if let Some(e) = err {
                return Err(if e == goread::Error::Eof && total > 0 {
                    goread::Error::UnexpectedEof
                } else {
                    e
                });
            }
        }
        Ok(v)
    }
}

/// `vp8.Decoder`.
pub struct Decoder<'r> {
    /// The input bitstream.
    r: LimitReader<'r>,
    /// The YCbCr image to decode into.
    img: YCbCr,
    /// How many 16x16 macroblocks wide and high the image is.
    mbw: usize,
    mbh: usize,
    frame_header: FrameHeader,
    segment_header: SegmentHeader,
    filter_header: FilterHeader,
    /// The first partition, and between one and eight coefficient partitions.
    fp: Partition,
    op: [Partition; 8],
    n_op: usize,
    /// Quantization factors.
    quant: [Quant; N_SEGMENT],
    /// DCT/WHT coefficient decoding probabilities.
    token_prob: [[[[u8; N_PROB]; N_CONTEXT]; N_BAND]; N_PLANE],
    use_skip_prob: bool,
    skip_prob: u8,
    /// Loop filter parameters.
    filter_params: [[FilterParam; 2]; N_SEGMENT],
    per_mb_filter_params: Vec<FilterParam>,

    // The fields below relate to the macroblock currently being decoded.
    /// Segment-based adjustments.
    segment: usize,
    left_mb: Mb,
    up_mb: Vec<Mb>,
    /// Bitmasks for which 4x4 regions of `coeff` hold non-zero coefficients.
    nz_dc_mask: u32,
    nz_ac_mask: u32,
    /// Predictor modes. `use_pred_y16` is libwebp's `!is_i4x4_`.
    use_pred_y16: bool,
    pred_y16: u8,
    pred_c8: u8,
    pred_y4: [[u8; 4]; 4],

    /// The macroblock reconstruction workspace; see reconstruct.rs for the layout.
    coeff: [i16; 16 * 16 + 2 * 8 * 8 + 4 * 4],
    ybr: [[u8; 32]; 1 + 16 + 1 + 8],
}

impl<'r> Decoder<'r> {
    /// `vp8.NewDecoder` followed by `Decoder.Init`: read at most `n` bytes from `r`.
    pub fn new(r: &'r mut dyn GoRead, n: usize) -> Self {
        Decoder {
            r: LimitReader { r, n },
            img: empty_image(),
            mbw: 0,
            mbh: 0,
            frame_header: FrameHeader::default(),
            segment_header: SegmentHeader::default(),
            filter_header: FilterHeader::default(),
            fp: Partition::default(),
            op: std::array::from_fn(|_| Partition::default()),
            n_op: 0,
            quant: [Quant::default(); N_SEGMENT],
            token_prob: [[[[0; N_PROB]; N_CONTEXT]; N_BAND]; N_PLANE],
            use_skip_prob: false,
            skip_prob: 0,
            filter_params: [[FilterParam::default(); 2]; N_SEGMENT],
            per_mb_filter_params: Vec::new(),
            segment: 0,
            left_mb: Mb::default(),
            up_mb: Vec::new(),
            nz_dc_mask: 0,
            nz_ac_mask: 0,
            use_pred_y16: false,
            pred_y16: 0,
            pred_c8: 0,
            pred_y4: [[0; 4]; 4],
            coeff: [0; 400],
            ybr: [[0; 32]; 26],
        }
    }

    /// `Decoder.DecodeFrameHeader`.
    pub fn decode_frame_header(&mut self) -> Result<FrameHeader, Error> {
        // All frame headers are at least 3 bytes long.
        let mut b = [0u8; 3];
        self.r.read_full(&mut b)?;
        self.frame_header.key_frame = (b[0] & 1) == 0;
        self.frame_header.version_number = (b[0] >> 1) & 7;
        self.frame_header.show_frame = (b[0] >> 4) & 1 == 1;
        self.frame_header.first_partition_len =
            u32::from(b[0]) >> 5 | u32::from(b[1]) << 3 | u32::from(b[2]) << 11;
        if !self.frame_header.key_frame {
            return Ok(self.frame_header);
        }
        // Frame headers for key frames are an additional 7 bytes long.
        let mut b = [0u8; 7];
        self.r.read_full(&mut b)?;
        // Check the magic sync code.
        if b[0] != 0x9d || b[1] != 0x01 || b[2] != 0x2a {
            return Err(Error::InvalidFormat);
        }
        self.frame_header.width = i32::from(b[4] & 0x3f) << 8 | i32::from(b[3]);
        self.frame_header.height = i32::from(b[6] & 0x3f) << 8 | i32::from(b[5]);
        self.frame_header.x_scale = b[4] >> 6;
        self.frame_header.y_scale = b[6] >> 6;
        self.mbw = (self.frame_header.width as usize + 0x0f) >> 4;
        self.mbh = (self.frame_header.height as usize + 0x0f) >> 4;
        self.segment_header = SegmentHeader {
            prob: [0xff; N_SEGMENT_PROB],
            ..SegmentHeader::default()
        };
        self.token_prob = DEFAULT_TOKEN_PROB;
        self.segment = 0;
        Ok(self.frame_header)
    }

    /// `Decoder.ensureImg`: a 4:2:0 image of whole macroblocks, sub-imaged to the frame rectangle.
    /// Go keeps the full-size planes and their strides, so the plane lengths are
    /// `16*mbw * 16*mbh` and `8*mbw * 8*mbh` — not the frame's — and every downstream consumer
    /// indexes by the full stride.
    fn ensure_img(&mut self) {
        let (w, h) = (self.frame_header.width, self.frame_header.height);
        if self.mbw == 0 || self.mbh == 0 || w <= 0 || h <= 0 {
            // Go's SubImage of an empty rectangle keeps only the subsample ratio.
            self.img = empty_image();
            self.per_mb_filter_params = vec![FilterParam::default(); self.mbw * self.mbh];
            self.up_mb = vec![Mb::default(); self.mbw];
            return;
        }
        self.img = YCbCr {
            y: vec![0; 16 * self.mbw * 16 * self.mbh],
            cb: vec![0; 8 * self.mbw * 8 * self.mbh],
            cr: vec![0; 8 * self.mbw * 8 * self.mbh],
            y_stride: 16 * self.mbw,
            c_stride: 8 * self.mbw,
            ratio: Ratio::R420,
            rect: Rect::new(0, 0, i64::from(w), i64::from(h)),
        };
        self.per_mb_filter_params = vec![FilterParam::default(); self.mbw * self.mbh];
        self.up_mb = vec![Mb::default(); self.mbw];
    }

    /// `Decoder.parseSegmentHeader` (section 9.3).
    fn parse_segment_header(&mut self) {
        self.segment_header.use_segment = self.fp.read_bit(UNIFORM_PROB);
        if !self.segment_header.use_segment {
            self.segment_header.update_map = false;
            return;
        }
        self.segment_header.update_map = self.fp.read_bit(UNIFORM_PROB);
        if self.fp.read_bit(UNIFORM_PROB) {
            self.segment_header.relative_delta = !self.fp.read_bit(UNIFORM_PROB);
            for i in 0..N_SEGMENT {
                self.segment_header.quantizer[i] = self.fp.read_optional_int(UNIFORM_PROB, 7) as i8;
            }
            for i in 0..N_SEGMENT {
                self.segment_header.filter_strength[i] =
                    self.fp.read_optional_int(UNIFORM_PROB, 6) as i8;
            }
        }
        if !self.segment_header.update_map {
            return;
        }
        for i in 0..N_SEGMENT_PROB {
            self.segment_header.prob[i] = if self.fp.read_bit(UNIFORM_PROB) {
                self.fp.read_uint(UNIFORM_PROB, 8) as u8
            } else {
                0xff
            };
        }
    }

    /// `Decoder.parseFilterHeader` (section 9.4).
    fn parse_filter_header(&mut self) {
        self.filter_header.simple = self.fp.read_bit(UNIFORM_PROB);
        self.filter_header.level = self.fp.read_uint(UNIFORM_PROB, 6) as i8;
        self.filter_header.sharpness = self.fp.read_uint(UNIFORM_PROB, 3) as u8;
        self.filter_header.use_lf_delta = self.fp.read_bit(UNIFORM_PROB);
        if self.filter_header.use_lf_delta && self.fp.read_bit(UNIFORM_PROB) {
            for i in 0..N_REF_LF_DELTA {
                self.filter_header.ref_lf_delta[i] =
                    self.fp.read_optional_int(UNIFORM_PROB, 6) as i8;
            }
            for i in 0..N_MODE_LF_DELTA {
                self.filter_header.mode_lf_delta[i] =
                    self.fp.read_optional_int(UNIFORM_PROB, 6) as i8;
            }
        }
        if self.filter_header.level == 0 {
            return;
        }
        if self.segment_header.use_segment {
            for i in 0..N_SEGMENT {
                let mut strength = self.segment_header.filter_strength[i];
                if self.segment_header.relative_delta {
                    strength = strength.wrapping_add(self.filter_header.level);
                }
                self.filter_header.per_segment_level[i] = strength;
            }
        } else {
            self.filter_header.per_segment_level[0] = self.filter_header.level;
        }
        self.compute_filter_params();
    }

    /// `Decoder.parseOtherPartitions` (section 9.5).
    fn parse_other_partitions(&mut self) -> Result<(), Error> {
        const MAX_NOP: usize = 1 << 3;
        let mut part_lens = [0i64; MAX_NOP];
        self.n_op = 1 << self.fp.read_uint(UNIFORM_PROB, 2);

        // The final partition's length is implied by the remaining chunk data and the other
        // nOP-1 lengths, which are 24-bit uints — up to 16 MiB per partition.
        let n = 3 * (self.n_op - 1);
        let last = self.n_op - 1;
        part_lens[last] = self.r.n as i64 - n as i64;
        if part_lens[last] < 0 {
            return Err(Error::Io(goread::Error::UnexpectedEof));
        }
        if n > 0 {
            let buf = self.r.read_full_alloc(n)?;
            for i in 0..last {
                let pl = i64::from(buf[3 * i])
                    | i64::from(buf[3 * i + 1]) << 8
                    | i64::from(buf[3 * i + 2]) << 16;
                if pl > part_lens[last] {
                    return Err(Error::Io(goread::Error::UnexpectedEof));
                }
                part_lens[i] = pl;
                part_lens[last] -= pl;
            }
        }

        // The final partition length must also fit in a 24-bit uint. This is not part of the spec;
        // it guards against a WEBP image too large to read the encoded coefficients into memory,
        // whether because the file is that large or because its RIFF metadata lies.
        if 1 << 24 <= part_lens[last] {
            return Err(Error::TooMuchData);
        }

        let buf = self.r.read_full_alloc(self.r.n)?;
        // The lengths sum to exactly what was just read, so the splits always fit.
        let mut rest = &buf[..];
        for (i, &pl) in part_lens.iter().enumerate().take(self.n_op) {
            let (head, tail) = rest.split_at((pl as usize).min(rest.len()));
            self.op[i].init(head.to_vec());
            rest = tail;
        }
        Ok(())
    }

    /// `Decoder.parseOtherHeaders`.
    fn parse_other_headers(&mut self) -> Result<(), Error> {
        // Initialize and parse the first partition.
        let first_partition = self
            .r
            .read_full_alloc(self.frame_header.first_partition_len as usize)?;
        self.fp.init(first_partition);
        if self.frame_header.key_frame {
            // Read and ignore the color space and pixel clamp values (section 9.2).
            self.fp.read_bit(UNIFORM_PROB);
            self.fp.read_bit(UNIFORM_PROB);
        }
        self.parse_segment_header();
        self.parse_filter_header();
        self.parse_other_partitions()?;
        self.parse_quant();
        if !self.frame_header.key_frame {
            // Golden and AltRef frames are specified in section 9.7, and are video-only.
            return Err(Error::GoldenAltRef);
        }
        // Read and ignore the refreshLastFrameBuffer bit (section 9.8), which is video-only.
        self.fp.read_bit(UNIFORM_PROB);
        self.parse_token_prob();
        self.use_skip_prob = self.fp.read_bit(UNIFORM_PROB);
        if self.use_skip_prob {
            self.skip_prob = self.fp.read_uint(UNIFORM_PROB, 8) as u8;
        }
        if self.fp.unexpected_eof {
            return Err(Error::Io(goread::Error::UnexpectedEof));
        }
        Ok(())
    }

    /// `Decoder.DecodeFrame`.
    pub fn decode_frame(mut self) -> Result<YCbCr, Error> {
        self.ensure_img();
        self.parse_other_headers()?;
        // Reconstruct the rows.
        for mbx in 0..self.mbw {
            self.up_mb[mbx] = Mb::default();
        }
        for mby in 0..self.mbh {
            self.left_mb = Mb::default();
            for mbx in 0..self.mbw {
                let skip = self.reconstruct(mbx, mby);
                let mut fs =
                    self.filter_params[self.segment][usize::from(btou(!self.use_pred_y16))];
                fs.inner = fs.inner || !skip;
                self.per_mb_filter_params[self.mbw * mby + mbx] = fs;
            }
        }
        if self.fp.unexpected_eof {
            return Err(Error::Io(goread::Error::UnexpectedEof));
        }
        for i in 0..self.n_op {
            if self.op[i].unexpected_eof {
                return Err(Error::Io(goread::Error::UnexpectedEof));
            }
        }
        // Apply the loop filter. Even with per-segment levels, section 15 says that "loop
        // filtering must be skipped entirely if loop_filter_level at either the frame header level
        // or macroblock override level is 0".
        if self.filter_header.level != 0 {
            if self.filter_header.simple {
                self.simple_filter();
            } else {
                self.normal_filter();
            }
        }
        Ok(self.img)
    }
}

/// Go's `SubImage` of an empty rectangle: a `*image.YCbCr` with only the subsample ratio set.
fn empty_image() -> YCbCr {
    YCbCr {
        y: Vec::new(),
        cb: Vec::new(),
        cr: Vec::new(),
        y_stride: 0,
        c_stride: 0,
        ratio: Ratio::R420,
        rect: Rect::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goread::BytesReader;

    fn header_of(data: &[u8]) -> Result<FrameHeader, Error> {
        let mut r = BytesReader::new(data);
        let n = data.len();
        Decoder::new(&mut r, n).decode_frame_header()
    }

    #[test]
    fn a_key_frame_header_is_ten_bytes() {
        // tag = 0x9d012a, 150x100, no scaling, firstPartitionLen 377.
        let h = header_of(&[0x32, 0x2f, 0x00, 0x9d, 0x01, 0x2a, 0x96, 0x00, 0x64, 0x00]).unwrap();
        assert_eq!(
            h,
            FrameHeader {
                key_frame: true,
                version_number: 1,
                show_frame: true,
                first_partition_len: 1 | 0x2f << 3,
                width: 150,
                height: 100,
                x_scale: 0,
                y_scale: 0,
            }
        );
        // The top two bits of the width and height words are the scale, not the dimension.
        let h = header_of(&[0x32, 0x2f, 0x00, 0x9d, 0x01, 0x2a, 0x96, 0x40, 0x64, 0x80]).unwrap();
        assert_eq!((h.width, h.height, h.x_scale, h.y_scale), (150, 100, 1, 2));
    }

    #[test]
    fn a_bad_sync_code_is_an_invalid_format() {
        assert_eq!(
            header_of(&[0x32, 0x2f, 0x00, 0x9c, 0x01, 0x2a, 0x96, 0x00, 0x64, 0x00]).err(),
            Some(Error::InvalidFormat)
        );
        assert_eq!(
            header_of(&[0x32, 0x2f, 0x00, 0x9d, 0x02, 0x2a, 0x96, 0x00, 0x64, 0x00]).err(),
            Some(Error::InvalidFormat)
        );
    }

    #[test]
    fn an_inter_frame_header_stops_after_three_bytes() {
        let h = header_of(&[0x01, 0x00, 0x00]).unwrap();
        assert!(!h.key_frame);
        assert_eq!((h.width, h.height), (0, 0));
    }

    /// Under ten bytes, `limitReader.ReadFull` sees `len(p) > r.n` and reports an unexpected EOF
    /// without reading anything — the count comes from the chunk header, not from the data.
    #[test]
    fn a_short_key_frame_header_is_an_unexpected_eof() {
        for n in [0usize, 1, 2, 3, 4, 9] {
            let data = vec![0x32u8; n];
            assert_eq!(
                header_of(&data).err(),
                Some(Error::Io(goread::Error::UnexpectedEof)),
                "{n}"
            );
        }
        // Ten bytes is enough for the header to be parsed, and then rejected on its sync code.
        assert_eq!(header_of(&[0x32u8; 10]).err(), Some(Error::InvalidFormat));
    }
}
