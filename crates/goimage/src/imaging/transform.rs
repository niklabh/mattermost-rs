//! Port of `imaging`'s transform.go: the eight EXIF orientations' flips and quarter turns.

use super::scanner::Scanner;
use super::tools::{new_nrgba, reverse};
use crate::image::Image;

/// Which source line or column each destination row comes from, and whether it is reversed.
enum RowSource {
    /// Source row `n`.
    Row(i64),
    /// Source column `n`, top to bottom.
    Col(i64),
}

fn transform(img: &Image, swap: bool, source: impl Fn(i64, i64) -> RowSource, rev: bool) -> Image {
    let src = Scanner::new(img);
    let (dst_w, dst_h) = if swap { (src.h, src.w) } else { (src.w, src.h) };
    let mut dst = new_nrgba(dst_w, dst_h);
    let row = (dst_w * 4) as usize;
    for dst_y in 0..dst_h {
        let i = dst_y as usize * dst.stride;
        let out = &mut dst.pix[i..i + row];
        match source(dst_y, dst_h) {
            RowSource::Row(y) => src.scan(0, y, src.w, y + 1, out),
            RowSource::Col(x) => src.scan(x, 0, x + 1, src.h, out),
        }
        if rev {
            reverse(out);
        }
    }
    Image::Nrgba(dst)
}

/// Port of `FlipH` (transform.go:10).
pub fn flip_h(img: &Image) -> Image {
    transform(img, false, |y, _| RowSource::Row(y), true)
}

/// Port of `FlipV` (transform.go:28).
pub fn flip_v(img: &Image) -> Image {
    transform(img, false, |y, h| RowSource::Row(h - y - 1), false)
}

/// Port of `Transpose` (transform.go:45).
pub fn transpose(img: &Image) -> Image {
    transform(img, true, |y, _| RowSource::Col(y), false)
}

/// Port of `Transverse` (transform.go:62).
pub fn transverse(img: &Image) -> Image {
    transform(img, true, |y, h| RowSource::Col(h - y - 1), true)
}

/// Port of `Rotate90` (transform.go:80), counter-clockwise.
pub fn rotate90(img: &Image) -> Image {
    transform(img, true, |y, h| RowSource::Col(h - y - 1), false)
}

/// Port of `Rotate180` (transform.go:97).
pub fn rotate180(img: &Image) -> Image {
    transform(img, false, |y, h| RowSource::Row(h - y - 1), true)
}

/// Port of `Rotate270` (transform.go:115), counter-clockwise.
pub fn rotate270(img: &Image) -> Image {
    transform(img, true, |y, _| RowSource::Col(y), true)
}
