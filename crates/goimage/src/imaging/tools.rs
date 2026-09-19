//! Port of `imaging`'s tools.go (Clone, Crop, CropAnchor, anchorPt) and utils.go's helpers.

use std::borrow::Cow;

use super::scanner::Scanner;
use crate::image::{Image, Pixels, Rect};

/// A new zeroed NRGBA image `(0,0)-(w,h)`: `image.NewNRGBA(image.Rect(0, 0, w, h))`.
pub(crate) fn new_nrgba(w: i64, h: i64) -> Pixels {
    Pixels::new(Rect::new(0, 0, w, h), 4)
}

/// `&image.NRGBA{}` — the empty image the library returns for degenerate sizes.
pub(crate) fn empty_nrgba() -> Image {
    Image::Nrgba(Pixels {
        pix: Vec::new(),
        stride: 0,
        rect: Rect::default(),
    })
}

/// Port of `Clone` (tools.go:29): a scanned copy of the whole image.
pub fn clone(img: &Image) -> Image {
    let src = Scanner::new(img);
    let mut dst = new_nrgba(src.w, src.h);
    let size = (src.w * 4) as usize;
    for y in 0..src.h {
        let i = y as usize * dst.stride;
        src.scan(0, y, src.w, y + 1, &mut dst.pix[i..i + size]);
    }
    Image::Nrgba(dst)
}

/// `imaging.Anchor` (tools.go:43).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Center,
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// Port of `anchorPt` (tools.go:58). Go's `/` truncates toward zero, as Rust's does.
fn anchor_pt(b: &Rect, w: i64, h: i64, anchor: Anchor) -> (i64, i64) {
    match anchor {
        Anchor::TopLeft => (b.min_x, b.min_y),
        Anchor::Top => (b.min_x + (b.dx() - w) / 2, b.min_y),
        Anchor::TopRight => (b.max_x - w, b.min_y),
        Anchor::Left => (b.min_x, b.min_y + (b.dy() - h) / 2),
        Anchor::Right => (b.max_x - w, b.min_y + (b.dy() - h) / 2),
        Anchor::BottomLeft => (b.min_x, b.max_y - h),
        Anchor::Bottom => (b.min_x + (b.dx() - w) / 2, b.max_y - h),
        Anchor::BottomRight => (b.max_x - w, b.max_y - h),
        Anchor::Center => (b.min_x + (b.dx() - w) / 2, b.min_y + (b.dy() - h) / 2),
    }
}

/// Port of `Crop` (tools.go:94).
pub fn crop(img: &Image, rect: Rect) -> Image {
    let b = img.bounds();
    let r = rect.intersect(&b).sub(b.min_x, b.min_y);
    if r.is_empty() {
        return empty_nrgba();
    }
    if r.eq_go(&b.sub(b.min_x, b.min_y)) {
        return clone(img);
    }
    let src = Scanner::new(img);
    let mut dst = new_nrgba(r.dx(), r.dy());
    let row = (r.dx() * 4) as usize;
    for y in r.min_y..r.max_y {
        let i = (y - r.min_y) as usize * dst.stride;
        src.scan(r.min_x, y, r.max_x, y + 1, &mut dst.pix[i..i + row]);
    }
    Image::Nrgba(dst)
}

/// Port of `CropAnchor` (tools.go:117).
pub fn crop_anchor(img: &Image, width: i64, height: i64, anchor: Anchor) -> Image {
    let b = img.bounds();
    let (x, y) = anchor_pt(&b, width, height, anchor);
    let r = Rect::new(0, 0, width, height).add(x, y);
    crop(img, b.intersect(&r))
}

/// Port of `toNRGBA` (utils.go:92): an NRGBA source is used in place (its pixels shared, as Go
/// shares the slice); anything else is cloned into a new NRGBA.
pub(crate) fn to_nrgba(img: &Image) -> Cow<'_, Pixels> {
    match img {
        Image::Nrgba(p) => Cow::Borrowed(p),
        other => match clone(other) {
            Image::Nrgba(p) => Cow::Owned(p),
            _ => Cow::Owned(Pixels::new(Rect::default(), 4)),
        },
    }
}

/// Port of `clamp` (utils.go:60): round half up by truncation and clamp to a byte. `x + 0.5` is a
/// separate FADDD here; the call sites that pass a product fuse it (see `resize::clamp_madd`).
#[inline]
pub(crate) fn clamp(x: f64) -> u8 {
    clamp_i(x + 0.5)
}

/// The integer half of `clamp`: `int64(v)` (FCVTZS, saturating, as Rust's `as` is), then clamp.
#[inline]
pub(crate) fn clamp_i(v: f64) -> u8 {
    let v = v as i64;
    if v > 255 {
        255
    } else if v > 0 {
        v as u8
    } else {
        0
    }
}

/// Port of `reverse` (utils.go:71): reverse the order of the 4-byte pixels in `pix`.
pub(crate) fn reverse(pix: &mut [u8]) {
    if pix.len() <= 4 {
        return;
    }
    let mut i = 0;
    let mut j = pix.len() - 4;
    while i < j {
        for k in 0..4 {
            pix.swap(i + k, j + k);
        }
        i += 4;
        j -= 4;
    }
}
