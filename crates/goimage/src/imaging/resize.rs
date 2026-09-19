//! Port of `imaging`'s resize.go: separable resampling (`Resize`), `Fit`, `Fill`.
//!
//! # The fused operations
//!
//! go1.26.4 on arm64 compiles this file with these fusions (`go tool objdump`, and nowhere
//! else — every other float operation is a separate FMULD/FADDD/FDIVD):
//!
//! | Go source | instruction | here |
//! |---|---|---|
//! | resize.go:25 `(float64(v)+0.5)*du - 0.5` | `FNMSUBD` | [`crate::fma::nmsub`] |
//! | resize.go:124-126 `r += float64(s[0]) * aw` (and g, b; also :158-160) | `FMADDD` | [`madd`] |
//! | resize.go:127 `a += aw`, where `aw := float64(s[3]) * w.weight` | `FMADDD` — the product is recomputed unrounded, while `aw` itself is still rounded for r/g/b | [`madd`] |
//! | utils.go:62 `x + 0.5` in `clamp(r * aInv)` (inlined; r, g, b) | `FMADDD` | [`clamp_madd`] |
//! | utils.go:62 `x + 0.5` in `clamp(a)` | `FADDD` — no product to fuse | [`clamp`] |
//!
//! The Lanczos kernel (resize.go:534, `init.0.func9`) and `sinc` (resize.go:436) fuse nothing;
//! `math.Sin` fuses internally ([`crate::gomath`]).

use super::scanner::Scanner;
use super::tools::{Anchor, clamp, clamp_i, clone, crop_anchor, empty_nrgba, new_nrgba, to_nrgba};
use crate::fma::{madd, nmsub};
use crate::gomath;
use crate::image::Image;

/// Port of `ResampleFilter` (resize.go:393): a kernel and its support radius. A support of zero
/// means nearest-neighbour, which does not evaluate the kernel.
#[derive(Clone, Copy)]
pub struct ResampleFilter {
    pub support: f64,
    pub kernel: fn(f64) -> f64,
}

impl std::fmt::Debug for ResampleFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResampleFilter")
            .field("support", &self.support)
            .finish()
    }
}

/// `imaging.NearestNeighbor` (resize.go:447).
pub const NEAREST_NEIGHBOR: ResampleFilter = ResampleFilter {
    support: 0.0,
    kernel: |_| 0.0,
};

/// `imaging.Lanczos` (resize.go:531): three-lobed Lanczos, the only filter Mattermost uses.
pub const LANCZOS: ResampleFilter = ResampleFilter {
    support: 3.0,
    kernel: lanczos,
};

/// Port of `sinc` (resize.go:432). `math.Pi * x` is computed once and used as both the argument
/// and the divisor (one FMULD, resize.go:436).
fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let px = std::f64::consts::PI * x;
    gomath::sin(px) / px
}

/// Port of the Lanczos kernel (resize.go:533).
fn lanczos(x: f64) -> f64 {
    let x = x.abs();
    if x < 3.0 {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// `clamp(x * y)` as compiled when `clamp` is inlined at a product: `int64(x*y + 0.5)` with the
/// multiply-add fused (utils.go:62, `FMADDD` in resizeHorizontal.func1/resizeVertical.func1).
#[inline]
fn clamp_madd(x: f64, y: f64) -> u8 {
    clamp_i(madd(0.5, x, y))
}

/// One destination sample's contributions: `(source index, weight)`.
type Weights = Vec<Vec<(usize, f64)>>;

/// Port of `precomputeWeights` (resize.go:13).
fn precompute_weights(dst_size: i64, src_size: i64, filter: &ResampleFilter) -> Weights {
    let du = src_size as f64 / dst_size as f64;
    let scale = if du < 1.0 { 1.0 } else { du };
    // resize.go:19: FMULD then FRINTPD — not fused.
    let ru = (scale * filter.support).ceil();
    let mut out = Vec::with_capacity(dst_size.max(0) as usize);
    for v in 0..dst_size {
        // resize.go:25: `FNMSUBD` — (v+0.5)*du - 0.5 in one rounding.
        let fu = nmsub(v as f64 + 0.5, du, 0.5);
        let begin = ((fu - ru).ceil() as i64).max(0);
        let end = ((fu + ru).floor() as i64).min(src_size - 1);
        let mut sum = 0.0;
        let mut row = Vec::new();
        for u in begin..=end {
            let w = (filter.kernel)((u as f64 - fu) / scale);
            if w != 0.0 {
                sum += w;
                row.push((u as usize, w));
            }
        }
        if sum != 0.0 {
            for entry in &mut row {
                entry.1 /= sum;
            }
        }
        out.push(row);
    }
    out
}

/// The inner product of resizeHorizontal/resizeVertical (resize.go:120-137, 154-171) for one
/// destination pixel, reading 4-byte source pixels out of `line`.
fn accumulate(line: &[u8], weights: &[(usize, f64)], d: &mut [u8]) {
    let (mut r, mut g, mut b, mut a) = (0.0, 0.0, 0.0, 0.0);
    for &(index, weight) in weights {
        let s = &line[index * 4..index * 4 + 4];
        let s3 = f64::from(s[3]);
        let aw = s3 * weight;
        r = madd(r, f64::from(s[0]), aw);
        g = madd(g, f64::from(s[1]), aw);
        b = madd(b, f64::from(s[2]), aw);
        // resize.go:127: `a += aw` fuses with aw's own multiply.
        a = madd(a, s3, weight);
    }
    if a != 0.0 {
        let a_inv = 1.0 / a;
        d[0] = clamp_madd(r, a_inv);
        d[1] = clamp_madd(g, a_inv);
        d[2] = clamp_madd(b, a_inv);
        d[3] = clamp(a);
    }
}

/// Port of `resizeHorizontal` (resize.go:113).
fn resize_horizontal(img: &Image, width: i64, filter: &ResampleFilter) -> Image {
    let src = Scanner::new(img);
    let mut dst = new_nrgba(width, src.h);
    let weights = precompute_weights(width, src.w, filter);
    let mut line = vec![0u8; (src.w * 4) as usize];
    for y in 0..src.h {
        src.scan(0, y, src.w, y + 1, &mut line);
        let j0 = y as usize * dst.stride;
        for (x, ws) in weights.iter().enumerate() {
            let j = j0 + x * 4;
            accumulate(&line, ws, &mut dst.pix[j..j + 4]);
        }
    }
    Image::Nrgba(dst)
}

/// Port of `resizeVertical` (resize.go:147).
fn resize_vertical(img: &Image, height: i64, filter: &ResampleFilter) -> Image {
    let src = Scanner::new(img);
    let mut dst = new_nrgba(src.w, height);
    let weights = precompute_weights(height, src.h, filter);
    let mut line = vec![0u8; (src.h * 4) as usize];
    for x in 0..src.w {
        src.scan(x, 0, x + 1, src.h, &mut line);
        for (y, ws) in weights.iter().enumerate() {
            let j = y * dst.stride + x as usize * 4;
            accumulate(&line, ws, &mut dst.pix[j..j + 4]);
        }
    }
    Image::Nrgba(dst)
}

/// Port of `resizeNearest` (resize.go:179).
fn resize_nearest(img: &Image, width: i64, height: i64) -> Image {
    let mut dst = new_nrgba(width, height);
    let b = img.bounds();
    let dx = b.dx() as f64 / width as f64;
    let dy = b.dy() as f64 / height as f64;
    if dx > 1.0 && dy > 1.0 {
        let src = Scanner::new(img);
        for y in 0..height {
            let src_y = ((y as f64 + 0.5) * dy) as i64;
            let mut off = y as usize * dst.stride;
            for x in 0..width {
                let src_x = ((x as f64 + 0.5) * dx) as i64;
                src.scan(
                    src_x,
                    src_y,
                    src_x + 1,
                    src_y + 1,
                    &mut dst.pix[off..off + 4],
                );
                off += 4;
            }
        }
    } else {
        let src = to_nrgba(img);
        for y in 0..height {
            let src_y = ((y as f64 + 0.5) * dy) as i64;
            let src_off0 = src_y as usize * src.stride;
            let mut off = y as usize * dst.stride;
            for x in 0..width {
                let src_x = ((x as f64 + 0.5) * dx) as i64;
                let s = src_off0 + src_x as usize * 4;
                dst.pix[off..off + 4].copy_from_slice(&src.pix[s..s + 4]);
                off += 4;
            }
        }
    }
    Image::Nrgba(dst)
}

/// Port of `Resize` (resize.go:65). A zero width or height preserves the aspect ratio (minimum
/// one pixel); both zero, a negative size, or an empty source give the empty image.
pub fn resize(img: &Image, width: i64, height: i64, filter: &ResampleFilter) -> Image {
    let (mut dst_w, mut dst_h) = (width, height);
    if dst_w < 0 || dst_h < 0 || (dst_w == 0 && dst_h == 0) {
        return empty_nrgba();
    }
    let b = img.bounds();
    let (src_w, src_h) = (b.dx(), b.dy());
    if src_w <= 0 || src_h <= 0 {
        return empty_nrgba();
    }
    if dst_w == 0 {
        // resize.go:82-83: FMULD, FDIVD, then FADDD and FRINTMD — nothing fused.
        let tmp = dst_h as f64 * src_w as f64 / src_h as f64;
        dst_w = 1.0f64.max((tmp + 0.5).floor()) as i64;
    }
    if dst_h == 0 {
        let tmp = dst_w as f64 * src_h as f64 / src_w as f64;
        dst_h = 1.0f64.max((tmp + 0.5).floor()) as i64;
    }
    if src_w == dst_w && src_h == dst_h {
        return clone(img);
    }
    if filter.support <= 0.0 {
        return resize_nearest(img, dst_w, dst_h);
    }
    if src_w != dst_w && src_h != dst_h {
        return resize_vertical(&resize_horizontal(img, dst_w, filter), dst_h, filter);
    }
    if src_w != dst_w {
        return resize_horizontal(img, dst_w, filter);
    }
    resize_vertical(img, dst_h, filter)
}

/// Port of `Fit` (resize.go:224): scale down to fit `width × height`, keeping the aspect ratio;
/// a source already inside the box is cloned, never enlarged.
pub fn fit(img: &Image, width: i64, height: i64, filter: &ResampleFilter) -> Image {
    if width <= 0 || height <= 0 {
        return empty_nrgba();
    }
    let b = img.bounds();
    let (src_w, src_h) = (b.dx(), b.dy());
    if src_w <= 0 || src_h <= 0 {
        return empty_nrgba();
    }
    if src_w <= width && src_h <= height {
        return clone(img);
    }
    let src_ar = src_w as f64 / src_h as f64;
    let max_ar = width as f64 / height as f64;
    let (new_w, new_h) = if src_ar > max_ar {
        (width, (width as f64 / src_ar) as i64)
    } else {
        ((height as f64 * src_ar) as i64, height)
    };
    resize(img, new_w, new_h, filter)
}

/// Port of `Fill` (resize.go:269): cover `width × height` and crop the overflow at `anchor`.
/// Sources at least 100×100 are cropped first and then resized; smaller ones the other way round.
pub fn fill(
    img: &Image,
    width: i64,
    height: i64,
    anchor: Anchor,
    filter: &ResampleFilter,
) -> Image {
    if width <= 0 || height <= 0 {
        return empty_nrgba();
    }
    let b = img.bounds();
    let (src_w, src_h) = (b.dx(), b.dy());
    if src_w <= 0 || src_h <= 0 {
        return empty_nrgba();
    }
    if src_w == width && src_h == height {
        return clone(img);
    }
    if src_w >= 100 && src_h >= 100 {
        return crop_and_resize(img, width, height, anchor, filter);
    }
    resize_and_crop(img, width, height, anchor, filter)
}

/// Port of `cropAndResize` (resize.go:294).
fn crop_and_resize(img: &Image, w: i64, h: i64, anchor: Anchor, filter: &ResampleFilter) -> Image {
    let b = img.bounds();
    let (src_w, src_h) = (b.dx(), b.dy());
    let src_ar = src_w as f64 / src_h as f64;
    let dst_ar = w as f64 / h as f64;
    let tmp = if src_ar < dst_ar {
        // resize.go:305-306: FMULD, FDIVD, math.Max, FADDD — not fused.
        let crop_h = src_w as f64 * h as f64 / w as f64;
        crop_anchor(img, src_w, (1.0f64.max(crop_h) + 0.5) as i64, anchor)
    } else {
        let crop_w = src_h as f64 * w as f64 / h as f64;
        crop_anchor(img, (1.0f64.max(crop_w) + 0.5) as i64, src_h, anchor)
    };
    resize(&tmp, w, h, filter)
}

/// Port of `resizeAndCrop` (resize.go:318).
fn resize_and_crop(img: &Image, w: i64, h: i64, anchor: Anchor, filter: &ResampleFilter) -> Image {
    let b = img.bounds();
    let src_ar = b.dx() as f64 / b.dy() as f64;
    let dst_ar = w as f64 / h as f64;
    let tmp = if src_ar < dst_ar {
        resize(img, w, 0, filter)
    } else {
        resize(img, 0, h, filter)
    };
    crop_anchor(&tmp, w, h, anchor)
}
