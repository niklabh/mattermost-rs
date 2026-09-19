//! Port of the parts of Go's `image` and `image/color` packages the codecs and the resampler
//! observe: the concrete image types a decoder can return, `At`, `Opaque`, the colour models'
//! conversions and the Y'CbCr arithmetic.
//!
//! Go dispatches on the *dynamic type* of an `image.Image` everywhere that matters here — the PNG
//! encoder picks a colour type from `m.ColorModel()`, the JPEG encoder and `imaging`'s scanner take
//! fast paths for `*image.RGBA`, `*image.YCbCr`, `*image.Paletted` and fall back to `m.At(x,
//! y).RGBA()` for everything else. So the Rust image is an enum over exactly those types, and each
//! fast path is a `match` arm. A variant Go never produces on these paths (`Alpha`, `NYCbCrA`) is
//! not modelled.

/// Port of `image.Rectangle`: `[min, max)` on both axes. Coordinates are Go `int`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub min_x: i64,
    pub min_y: i64,
    pub max_x: i64,
    pub max_y: i64,
}

impl Rect {
    /// Port of `image.Rect` — which also canonicalises swapped corners.
    pub fn new(x0: i64, y0: i64, x1: i64, y1: i64) -> Rect {
        let (min_x, max_x) = if x0 > x1 { (x1, x0) } else { (x0, x1) };
        let (min_y, max_y) = if y0 > y1 { (y1, y0) } else { (y0, y1) };
        Rect {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// `Rectangle.Dx`.
    pub fn dx(&self) -> i64 {
        self.max_x - self.min_x
    }

    /// `Rectangle.Dy`.
    pub fn dy(&self) -> i64 {
        self.max_y - self.min_y
    }

    /// `Rectangle.Empty`.
    pub fn is_empty(&self) -> bool {
        self.min_x >= self.max_x || self.min_y >= self.max_y
    }

    /// `Point{x, y}.In(r)`.
    pub fn contains(&self, x: i64, y: i64) -> bool {
        self.min_x <= x && x < self.max_x && self.min_y <= y && y < self.max_y
    }

    /// `Rectangle.Intersect`: the largest rectangle inside both, or the zero rectangle.
    pub fn intersect(&self, s: &Rect) -> Rect {
        let r = Rect {
            min_x: self.min_x.max(s.min_x),
            min_y: self.min_y.max(s.min_y),
            max_x: self.max_x.min(s.max_x),
            max_y: self.max_y.min(s.max_y),
        };
        // Go's `Empty` test here is `r.Min.X > r.Max.X || r.Min.Y > r.Max.Y` — strict, so a
        // zero-width overlap keeps its position.
        if r.min_x > r.max_x || r.min_y > r.max_y {
            return Rect::default();
        }
        r
    }

    /// `Rectangle.Add(p)`.
    pub fn add(&self, x: i64, y: i64) -> Rect {
        Rect {
            min_x: self.min_x + x,
            min_y: self.min_y + y,
            max_x: self.max_x + x,
            max_y: self.max_y + y,
        }
    }

    /// `Rectangle.Sub(p)`.
    pub fn sub(&self, x: i64, y: i64) -> Rect {
        self.add(-x, -y)
    }

    /// `Rectangle.Eq`: equal, or both empty.
    pub fn eq_go(&self, s: &Rect) -> bool {
        self == s || (self.is_empty() && s.is_empty())
    }
}

/// A `color.Color` of one of the concrete types these packages produce. The variant is the Go type,
/// which matters: `NRGBAModel.Convert` returns an `NRGBA` unchanged but converts an `RGBA` through
/// its premultiplied value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Rgba([u8; 4]),
    Rgba64([u16; 4]),
    Nrgba([u8; 4]),
    Nrgba64([u16; 4]),
    Gray(u8),
    Gray16(u16),
    YCbCr(u8, u8, u8),
    Cmyk([u8; 4]),
}

impl Color {
    /// `color.Color.RGBA`: alpha-premultiplied 16-bit channels in `u32`s.
    pub fn rgba(&self) -> (u32, u32, u32, u32) {
        match *self {
            Color::Rgba([r, g, b, a]) => (x101(r), x101(g), x101(b), x101(a)),
            Color::Rgba64([r, g, b, a]) => (u32::from(r), u32::from(g), u32::from(b), u32::from(a)),
            Color::Nrgba([r, g, b, a]) => {
                let a32 = u32::from(a);
                (
                    x101(r) * a32 / 0xff,
                    x101(g) * a32 / 0xff,
                    x101(b) * a32 / 0xff,
                    x101(a),
                )
            }
            Color::Nrgba64([r, g, b, a]) => {
                let a32 = u32::from(a);
                (
                    u32::from(r) * a32 / 0xffff,
                    u32::from(g) * a32 / 0xffff,
                    u32::from(b) * a32 / 0xffff,
                    a32,
                )
            }
            Color::Gray(y) => {
                let y = x101(y);
                (y, y, y, 0xffff)
            }
            Color::Gray16(y) => {
                let y = u32::from(y);
                (y, y, y, 0xffff)
            }
            Color::YCbCr(y, cb, cr) => ycbcr_rgba(y, cb, cr),
            Color::Cmyk([c, m, y, k]) => {
                let w = 0xffff - u32::from(k) * 0x101;
                (
                    (0xffff - u32::from(c) * 0x101) * w / 0xffff,
                    (0xffff - u32::from(m) * 0x101) * w / 0xffff,
                    (0xffff - u32::from(y) * 0x101) * w / 0xffff,
                    0xffff,
                )
            }
        }
    }

    /// `color.NRGBAModel.Convert(c).(color.NRGBA)`.
    pub fn to_nrgba(&self) -> [u8; 4] {
        if let Color::Nrgba(c) = *self {
            return c;
        }
        let (r, g, b, a) = self.rgba();
        if a == 0xffff {
            return [(r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8, 0xff];
        }
        if a == 0 {
            return [0, 0, 0, 0];
        }
        let r = (r * 0xffff) / a;
        let g = (g * 0xffff) / a;
        let b = (b * 0xffff) / a;
        [
            (r >> 8) as u8,
            (g >> 8) as u8,
            (b >> 8) as u8,
            (a >> 8) as u8,
        ]
    }

    /// `color.NRGBA64Model.Convert(c).(color.NRGBA64)`.
    pub fn to_nrgba64(&self) -> [u16; 4] {
        if let Color::Nrgba64(c) = *self {
            return c;
        }
        let (r, g, b, a) = self.rgba();
        if a == 0xffff {
            return [r as u16, g as u16, b as u16, 0xffff];
        }
        if a == 0 {
            return [0, 0, 0, 0];
        }
        let r = (r * 0xffff) / a;
        let g = (g * 0xffff) / a;
        let b = (b * 0xffff) / a;
        [r as u16, g as u16, b as u16, a as u16]
    }

    /// `color.GrayModel.Convert(c).(color.Gray).Y`.
    pub fn to_gray(&self) -> u8 {
        if let Color::Gray(y) = *self {
            return y;
        }
        let (r, g, b, _) = self.rgba();
        ((19595 * r + 38470 * g + 7471 * b + (1 << 15)) >> 24) as u8
    }

    /// `color.Gray16Model.Convert(c).(color.Gray16).Y`.
    pub fn to_gray16(&self) -> u16 {
        if let Color::Gray16(y) = *self {
            return y;
        }
        let (r, g, b, _) = self.rgba();
        ((19595 * r + 38470 * g + 7471 * b + (1 << 15)) >> 16) as u16
    }
}

/// `v | v << 8` — 8-bit colour to 16-bit.
fn x101(v: u8) -> u32 {
    let v = u32::from(v);
    v | v << 8
}

/// Port of `color.YCbCr.RGBA` (color/ycbcr.go:171) — 16-bit output, *not* `YCbCrToRGB` widened.
pub fn ycbcr_rgba(y: u8, cb: u8, cr: u8) -> (u32, u32, u32, u32) {
    let yy1 = i32::from(y) * 0x10101;
    let cb1 = i32::from(cb) - 128;
    let cr1 = i32::from(cr) - 128;
    let clamp16 = |v: i32| -> u32 {
        if (v as u32) & 0xff00_0000 == 0 {
            (v >> 8) as u32
        } else {
            (!(v >> 31) & 0xffff) as u32
        }
    };
    (
        clamp16(yy1 + 91881 * cr1),
        clamp16(yy1 - 22554 * cb1 - 46802 * cr1),
        clamp16(yy1 + 116130 * cb1),
        0xffff,
    )
}

/// Port of `color.YCbCrToRGB` (color/ycbcr.go:59).
pub fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let yy1 = i32::from(y) * 0x10101;
    let cb1 = i32::from(cb) - 128;
    let cr1 = i32::from(cr) - 128;
    let clamp8 = |v: i32| -> u8 {
        if (v as u32) & 0xff00_0000 == 0 {
            (v >> 16) as u8
        } else {
            !(v >> 31) as u8
        }
    };
    (
        clamp8(yy1 + 91881 * cr1),
        clamp8(yy1 - 22554 * cb1 - 46802 * cr1),
        clamp8(yy1 + 116130 * cb1),
    )
}

/// Port of `color.RGBToYCbCr` (color/ycbcr.go:8).
pub fn rgb_to_ycbcr(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let r1 = i32::from(r);
    let g1 = i32::from(g);
    let b1 = i32::from(b);
    let yy = (19595 * r1 + 38470 * g1 + 7471 * b1 + (1 << 15)) >> 16;
    let clamp = |v: i32| -> u8 {
        if (v as u32) & 0xff00_0000 == 0 {
            (v >> 16) as u8
        } else {
            !(v >> 31) as u8
        }
    };
    (
        yy as u8,
        clamp(-11056 * r1 - 21712 * g1 + 32768 * b1 + (257 << 15)),
        clamp(32768 * r1 - 27440 * g1 - 5328 * b1 + (257 << 15)),
    )
}

/// `image.YCbCrSubsampleRatio`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ratio {
    R444,
    R422,
    R420,
    R440,
    R411,
    R410,
}

impl Ratio {
    /// `YCbCrSubsampleRatio.String`.
    pub fn go_name(&self) -> &'static str {
        match self {
            Ratio::R444 => "YCbCrSubsampleRatio444",
            Ratio::R422 => "YCbCrSubsampleRatio422",
            Ratio::R420 => "YCbCrSubsampleRatio420",
            Ratio::R440 => "YCbCrSubsampleRatio440",
            Ratio::R411 => "YCbCrSubsampleRatio411",
            Ratio::R410 => "YCbCrSubsampleRatio410",
        }
    }
}

/// A plain interleaved-pixel image: `Gray`, `Gray16`, `RGBA`, `RGBA64`, `NRGBA`, `NRGBA64`, `CMYK`
/// all share this layout and differ only in bytes per pixel and meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels {
    pub pix: Vec<u8>,
    pub stride: usize,
    pub rect: Rect,
}

impl Pixels {
    /// `New<Type>(r)`: a zeroed buffer, `stride = bpp * r.Dx()`.
    pub fn new(rect: Rect, bpp: usize) -> Pixels {
        let w = rect.dx().max(0) as usize;
        let h = rect.dy().max(0) as usize;
        Pixels {
            pix: vec![0; w * h * bpp],
            stride: w * bpp,
            rect,
        }
    }

    /// `PixOffset(x, y)` for `bpp` bytes per pixel.
    pub fn offset(&self, x: i64, y: i64, bpp: usize) -> usize {
        ((y - self.rect.min_y) as usize) * self.stride + ((x - self.rect.min_x) as usize) * bpp
    }
}

/// `*image.Paletted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paletted {
    pub pix: Pixels,
    pub palette: Vec<Color>,
}

/// `*image.YCbCr`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YCbCr {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    pub y_stride: usize,
    pub c_stride: usize,
    pub ratio: Ratio,
    pub rect: Rect,
}

impl YCbCr {
    /// Port of `image.NewYCbCr` (image/ycbcr.go:175) with `yCbCrSize`'s plane geometry.
    pub fn new(rect: Rect, ratio: Ratio) -> YCbCr {
        let (w, h, cw, ch) = ycbcr_size(&rect, ratio);
        let (w, h, cw, ch) = (
            w.max(0) as usize,
            h.max(0) as usize,
            cw.max(0) as usize,
            ch.max(0) as usize,
        );
        YCbCr {
            y: vec![0; w * h],
            cb: vec![0; cw * ch],
            cr: vec![0; cw * ch],
            y_stride: w,
            c_stride: cw,
            ratio,
            rect,
        }
    }

    /// `YCbCr.YOffset`.
    pub fn y_offset(&self, x: i64, y: i64) -> usize {
        ((y - self.rect.min_y) as usize) * self.y_stride + (x - self.rect.min_x) as usize
    }

    /// `YCbCr.COffset` — Go's truncating division, which is floor for the non-negative
    /// coordinates every decoder produces.
    pub fn c_offset(&self, x: i64, y: i64) -> usize {
        let r = &self.rect;
        let cs = self.c_stride as i64;
        let v = match self.ratio {
            Ratio::R422 => (y - r.min_y) * cs + (x / 2 - r.min_x / 2),
            Ratio::R420 => (y / 2 - r.min_y / 2) * cs + (x / 2 - r.min_x / 2),
            Ratio::R440 => (y / 2 - r.min_y / 2) * cs + (x - r.min_x),
            Ratio::R411 => (y - r.min_y) * cs + (x / 4 - r.min_x / 4),
            Ratio::R410 => (y / 2 - r.min_y / 2) * cs + (x / 4 - r.min_x / 4),
            Ratio::R444 => (y - r.min_y) * cs + (x - r.min_x),
        };
        v as usize
    }
}

/// Port of `image.yCbCrSize` (image/ycbcr.go:147).
pub fn ycbcr_size(r: &Rect, ratio: Ratio) -> (i64, i64, i64, i64) {
    let (w, h) = (r.dx(), r.dy());
    let (cw, ch) = match ratio {
        Ratio::R422 => ((r.max_x + 1) / 2 - r.min_x / 2, h),
        Ratio::R420 => (
            (r.max_x + 1) / 2 - r.min_x / 2,
            (r.max_y + 1) / 2 - r.min_y / 2,
        ),
        Ratio::R440 => (w, (r.max_y + 1) / 2 - r.min_y / 2),
        Ratio::R411 => ((r.max_x + 3) / 4 - r.min_x / 4, h),
        Ratio::R410 => (
            (r.max_x + 3) / 4 - r.min_x / 4,
            (r.max_y + 1) / 2 - r.min_y / 2,
        ),
        Ratio::R444 => (w, h),
    };
    (w, h, cw, ch)
}

/// A Go `image.Image` of one of the concrete types a decoder or `imaging` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Image {
    Gray(Pixels),
    Gray16(Pixels),
    Rgba(Pixels),
    Rgba64(Pixels),
    Nrgba(Pixels),
    Nrgba64(Pixels),
    Cmyk(Pixels),
    Paletted(Paletted),
    YCbCr(YCbCr),
}

/// The identity of `m.ColorModel()`, which is what the PNG encoder switches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Gray,
    Gray16,
    Rgba,
    Rgba64,
    Nrgba,
    Nrgba64,
    Cmyk,
    YCbCr,
    Palette,
}

impl Image {
    /// `Bounds()`.
    pub fn bounds(&self) -> Rect {
        match self {
            Image::Gray(p)
            | Image::Gray16(p)
            | Image::Rgba(p)
            | Image::Rgba64(p)
            | Image::Nrgba(p)
            | Image::Nrgba64(p)
            | Image::Cmyk(p) => p.rect,
            Image::Paletted(p) => p.pix.rect,
            Image::YCbCr(p) => p.rect,
        }
    }

    /// `ColorModel()`.
    pub fn model(&self) -> Model {
        match self {
            Image::Gray(_) => Model::Gray,
            Image::Gray16(_) => Model::Gray16,
            Image::Rgba(_) => Model::Rgba,
            Image::Rgba64(_) => Model::Rgba64,
            Image::Nrgba(_) => Model::Nrgba,
            Image::Nrgba64(_) => Model::Nrgba64,
            Image::Cmyk(_) => Model::Cmyk,
            Image::Paletted(_) => Model::Palette,
            Image::YCbCr(_) => Model::YCbCr,
        }
    }

    /// `At(x, y)`. Outside the bounds Go returns the zero colour of the type (the first palette
    /// entry for `Paletted`); `None` only for a `Paletted` with an empty palette, where Go
    /// returns a nil `color.Color`.
    pub fn at(&self, x: i64, y: i64) -> Option<Color> {
        let inside = self.bounds().contains(x, y);
        Some(match self {
            Image::Gray(p) => Color::Gray(if inside { p.pix[p.offset(x, y, 1)] } else { 0 }),
            Image::Gray16(p) => Color::Gray16(if inside {
                let i = p.offset(x, y, 2);
                u16::from(p.pix[i]) << 8 | u16::from(p.pix[i + 1])
            } else {
                0
            }),
            Image::Rgba(p) => Color::Rgba(if inside { quad(p, x, y) } else { [0; 4] }),
            Image::Nrgba(p) => Color::Nrgba(if inside { quad(p, x, y) } else { [0; 4] }),
            Image::Cmyk(p) => Color::Cmyk(if inside { quad(p, x, y) } else { [0; 4] }),
            Image::Rgba64(p) => Color::Rgba64(if inside { quad16(p, x, y) } else { [0; 4] }),
            Image::Nrgba64(p) => Color::Nrgba64(if inside { quad16(p, x, y) } else { [0; 4] }),
            Image::Paletted(p) => {
                let first = *p.palette.first()?;
                if !inside {
                    first
                } else {
                    // Go indexes the palette directly and panics past its end; the PNG decoder
                    // guarantees every index is inside it (it extends the palette to 256
                    // opaque-black entries first), so this fallback is never reached from one.
                    let idx = usize::from(p.pix.pix[p.pix.offset(x, y, 1)]);
                    p.palette.get(idx).copied().unwrap_or(first)
                }
            }
            Image::YCbCr(p) => {
                if !inside {
                    Color::YCbCr(0, 0, 0)
                } else {
                    let yi = p.y_offset(x, y);
                    let ci = p.c_offset(x, y);
                    Color::YCbCr(p.y[yi], p.cb[ci], p.cr[ci])
                }
            }
        })
    }

    /// `Opaque()` for the types that have it; `None` for the ones that do not (`YCbCr` and
    /// `CMYK` have no `Opaque` method, so Go's `opaque()` helpers fall back to scanning `At`).
    pub fn opaque_method(&self) -> Option<bool> {
        match self {
            Image::Gray(_) | Image::Gray16(_) => Some(true),
            Image::Rgba(p) | Image::Nrgba(p) => Some(scan_alpha(p, 4, 3, |s| s[0] == 0xff)),
            Image::Rgba64(p) | Image::Nrgba64(p) => {
                Some(scan_alpha(p, 8, 6, |s| s[0] == 0xff && s[1] == 0xff))
            }
            Image::Cmyk(_) => Some(true),
            Image::YCbCr(_) => Some(true),
            Image::Paletted(p) => Some(paletted_opaque(p)),
        }
    }
}

fn quad(p: &Pixels, x: i64, y: i64) -> [u8; 4] {
    let i = p.offset(x, y, 4);
    [p.pix[i], p.pix[i + 1], p.pix[i + 2], p.pix[i + 3]]
}

fn quad16(p: &Pixels, x: i64, y: i64) -> [u16; 4] {
    let i = p.offset(x, y, 8);
    let s = &p.pix[i..i + 8];
    [
        u16::from(s[0]) << 8 | u16::from(s[1]),
        u16::from(s[2]) << 8 | u16::from(s[3]),
        u16::from(s[4]) << 8 | u16::from(s[5]),
        u16::from(s[6]) << 8 | u16::from(s[7]),
    ]
}

/// The shared loop of `RGBA.Opaque`, `NRGBA.Opaque` and their 64-bit forms: walk every pixel's
/// alpha bytes (at `alpha_at` within a `bpp`-byte pixel) row by row.
fn scan_alpha(p: &Pixels, bpp: usize, alpha_at: usize, opaque: impl Fn(&[u8]) -> bool) -> bool {
    if p.rect.is_empty() {
        return true;
    }
    let row = p.rect.dx() as usize * bpp;
    for y in 0..p.rect.dy() as usize {
        let base = y * p.stride;
        let mut i = alpha_at;
        while i < row {
            if !opaque(&p.pix[base + i..]) {
                return false;
            }
            i += bpp;
        }
    }
    true
}

/// Port of `Paletted.Opaque` (image/image.go:1258): only the palette entries actually used.
fn paletted_opaque(p: &Paletted) -> bool {
    let mut present = [false; 256];
    let w = p.pix.rect.dx().max(0) as usize;
    for y in 0..p.pix.rect.dy().max(0) as usize {
        let base = y * p.pix.stride;
        for &c in &p.pix.pix[base..base + w] {
            present[usize::from(c)] = true;
        }
    }
    for (i, c) in p.palette.iter().enumerate() {
        if !present.get(i).copied().unwrap_or(false) {
            continue;
        }
        if c.rgba().3 != 0xffff {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ycbcr_rgba_matches_the_documented_example() {
        // color/ycbcr.go:178-187 documents 0x808d for green; that comment is stale. go1.26.4
        // computes 0x808e (measured with a one-line program), and so does this port.
        assert_eq!(
            ycbcr_rgba(0x7f, 0x7f, 0x7f),
            (0x7e18, 0x808e, 0x7db9, 0xffff)
        );
        assert_eq!(ycbcr_to_rgb(0x7f, 0x7f, 0x7f), (0x7e, 0x80, 0x7d));
    }

    #[test]
    fn gray_ycbcr_is_exact_at_the_ends() {
        for y in [0u8, 1, 0x80, 0xfe, 0xff] {
            assert_eq!(ycbcr_rgba(y, 0x80, 0x80), Color::Gray(y).rgba());
        }
    }

    #[test]
    fn nrgba_conversion_passes_nrgba_through_and_unpremultiplies_rgba() {
        assert_eq!(Color::Nrgba([10, 20, 30, 40]).to_nrgba(), [10, 20, 30, 40]);
        assert_eq!(Color::Rgba([0, 0, 0, 0]).to_nrgba(), [0, 0, 0, 0]);
        assert_eq!(Color::Rgba([1, 2, 3, 0xff]).to_nrgba(), [1, 2, 3, 0xff]);
        // 0x4040 * 0xffff / 0x8080 = 0x7fff → 0x7f.
        assert_eq!(
            Color::Rgba([0x40, 0, 0, 0x80]).to_nrgba(),
            [0x7f, 0, 0, 0x80]
        );
    }

    #[test]
    fn intersect_keeps_a_zero_width_overlap() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(
            a.intersect(&Rect::new(10, 2, 20, 5)),
            Rect::new(10, 2, 10, 5)
        );
        assert_eq!(a.intersect(&Rect::new(11, 2, 20, 5)), Rect::default());
    }

    #[test]
    fn ycbcr_planes_follow_go_geometry() {
        let m = YCbCr::new(Rect::new(0, 0, 5, 3), Ratio::R420);
        assert_eq!((m.y_stride, m.c_stride, m.cb.len()), (5, 3, 6));
        let m = YCbCr::new(Rect::new(0, 0, 5, 3), Ratio::R410);
        assert_eq!((m.c_stride, m.cb.len()), (2, 4));
    }
}
