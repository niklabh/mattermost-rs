//! Port of `imaging`'s `scanner` (boxes-ltd/imaging scanner.go): reads a rectangle of any source
//! image into non-premultiplied 8-bit RGBA, with a fast path per concrete Go type.
//!
//! Which types have their own case matters, because the fast paths and the generic path round
//! differently. Go's switch (scanner.go:32) has cases for `*image.NRGBA`, `*image.NRGBA64`,
//! `*image.RGBA`, `*image.RGBA64`, `*image.Gray`, `*image.Gray16`, `*image.YCbCr` and
//! `*image.Paletted`; `*image.CMYK` is the only variant here that takes the `default:` branch
//! through `At(x, y).RGBA()`.

use crate::image::{Image, Ratio};

/// Port of `scanner` (scanner.go:8).
pub struct Scanner<'a> {
    image: &'a Image,
    /// Source width and height (`Bounds().Dx()`, `Dy()`).
    pub w: i64,
    pub h: i64,
    /// `Paletted` only: the palette converted with `NRGBAModel`, padded with zero colours to at
    /// least 256 entries (scanner.go:21), so an index past the palette reads transparent black.
    palette: Vec<[u8; 4]>,
}

impl<'a> Scanner<'a> {
    /// Port of `newScanner` (scanner.go:14).
    pub fn new(image: &'a Image) -> Scanner<'a> {
        let b = image.bounds();
        let mut palette = Vec::new();
        if let Image::Paletted(p) = image {
            palette = vec![[0u8; 4]; p.palette.len().max(256)];
            for (slot, c) in palette.iter_mut().zip(&p.palette) {
                *slot = c.to_nrgba();
            }
        }
        Scanner {
            image,
            w: b.dx(),
            h: b.dy(),
            palette,
        }
    }

    /// Port of `scanner.scan` (scanner.go:30): the rectangle `[x1,x2) × [y1,y2)`, in coordinates
    /// relative to the image's origin, row-major into `dst` (4 bytes per pixel).
    pub fn scan(&self, x1: i64, y1: i64, x2: i64, y2: i64, dst: &mut [u8]) {
        match self.image {
            Image::Nrgba(img) => {
                // scanner.go:33 — indexes from Pix[0] without subtracting Rect.Min.
                let size = ((x2 - x1) * 4) as usize;
                let mut j = 0;
                let mut i = (y1 as usize) * img.stride + (x1 as usize) * 4;
                for _ in y1..y2 {
                    dst[j..j + size].copy_from_slice(&img.pix[i..i + size]);
                    j += size;
                    i += img.stride;
                }
            }
            Image::Nrgba64(img) => {
                let mut j = 0;
                for y in y1..y2 {
                    let mut i = (y as usize) * img.stride + (x1 as usize) * 8;
                    for _ in x1..x2 {
                        let s = &img.pix[i..i + 8];
                        dst[j] = s[0];
                        dst[j + 1] = s[2];
                        dst[j + 2] = s[4];
                        dst[j + 3] = s[6];
                        j += 4;
                        i += 8;
                    }
                }
            }
            Image::Rgba(img) => {
                let mut j = 0;
                for y in y1..y2 {
                    let mut i = (y as usize) * img.stride + (x1 as usize) * 4;
                    for _ in x1..x2 {
                        let s = &img.pix[i..i + 4];
                        let a = s[3];
                        match a {
                            0 => dst[j..j + 4].copy_from_slice(&[0, 0, 0, 0]),
                            0xff => dst[j..j + 4].copy_from_slice(&[s[0], s[1], s[2], a]),
                            _ => {
                                // scanner.go:95: uint16 arithmetic, truncated to uint8.
                                let a16 = u16::from(a);
                                dst[j] = (u16::from(s[0]).wrapping_mul(0xff) / a16) as u8;
                                dst[j + 1] = (u16::from(s[1]).wrapping_mul(0xff) / a16) as u8;
                                dst[j + 2] = (u16::from(s[2]).wrapping_mul(0xff) / a16) as u8;
                                dst[j + 3] = a;
                            }
                        }
                        j += 4;
                        i += 4;
                    }
                }
            }
            Image::Rgba64(img) => {
                let mut j = 0;
                for y in y1..y2 {
                    let mut i = (y as usize) * img.stride + (x1 as usize) * 8;
                    for _ in x1..x2 {
                        let s = &img.pix[i..i + 8];
                        // scanner.go:118: the switch is on the alpha's *high byte* only.
                        let a = s[6];
                        match a {
                            0 => dst[j..j + 3].copy_from_slice(&[0, 0, 0]),
                            0xff => dst[j..j + 3].copy_from_slice(&[s[0], s[2], s[4]]),
                            _ => {
                                let r32 = u32::from(s[0]) << 8 | u32::from(s[1]);
                                let g32 = u32::from(s[2]) << 8 | u32::from(s[3]);
                                let b32 = u32::from(s[4]) << 8 | u32::from(s[5]);
                                let a32 = u32::from(s[6]) << 8 | u32::from(s[7]);
                                dst[j] = (((r32.wrapping_mul(0xffff)) / a32) >> 8) as u8;
                                dst[j + 1] = (((g32.wrapping_mul(0xffff)) / a32) >> 8) as u8;
                                dst[j + 2] = (((b32.wrapping_mul(0xffff)) / a32) >> 8) as u8;
                            }
                        }
                        dst[j + 3] = a;
                        j += 4;
                        i += 8;
                    }
                }
            }
            Image::Gray(img) => {
                let n = (x2 - x1) as usize;
                let mut j = 0;
                for y in y1..y2 {
                    let i = (y as usize) * img.stride + x1 as usize;
                    for &c in &img.pix[i..i + n] {
                        dst[j..j + 4].copy_from_slice(&[c, c, c, 0xff]);
                        j += 4;
                    }
                }
            }
            Image::Gray16(img) => {
                let mut j = 0;
                for y in y1..y2 {
                    let mut i = (y as usize) * img.stride + (x1 as usize) * 2;
                    for _ in x1..x2 {
                        let c = img.pix[i];
                        dst[j..j + 4].copy_from_slice(&[c, c, c, 0xff]);
                        j += 4;
                        i += 2;
                    }
                }
            }
            Image::YCbCr(img) => {
                // scanner.go:176.
                let r = img.rect;
                let (x1, x2, y1, y2) = (x1 + r.min_x, x2 + r.min_x, y1 + r.min_y, y2 + r.min_y);
                let hy = r.min_y / 2;
                let hx = r.min_x / 2;
                let cs = img.c_stride as i64;
                let mut j = 0;
                for y in y1..y2 {
                    let iy0 = ((y - r.min_y) as usize) * img.y_stride + (x1 - r.min_x) as usize;
                    let y_base = match img.ratio {
                        Ratio::R444 | Ratio::R422 => (y - r.min_y) * cs,
                        Ratio::R420 | Ratio::R440 => (y / 2 - hy) * cs,
                        Ratio::R411 | Ratio::R410 => 0,
                    };
                    for (k, x) in (x1..x2).enumerate() {
                        let iy = iy0 + k;
                        let ic = match img.ratio {
                            Ratio::R444 | Ratio::R440 => (y_base + (x - r.min_x)) as usize,
                            Ratio::R422 | Ratio::R420 => (y_base + (x / 2 - hx)) as usize,
                            Ratio::R411 | Ratio::R410 => img.c_offset(x, y),
                        };
                        let yy1 = i32::from(img.y[iy]) * 0x10101;
                        let cb1 = i32::from(img.cb[ic]) - 128;
                        let cr1 = i32::from(img.cr[ic]) - 128;
                        dst[j] = clamp_shift16(yy1 + 91881 * cr1);
                        dst[j + 1] = clamp_shift16(yy1 - 22554 * cb1 - 46802 * cr1);
                        dst[j + 2] = clamp_shift16(yy1 + 116130 * cb1);
                        dst[j + 3] = 0xff;
                        j += 4;
                    }
                }
            }
            Image::Paletted(img) => {
                let n = (x2 - x1) as usize;
                let mut j = 0;
                for y in y1..y2 {
                    let i = (y as usize) * img.pix.stride + x1 as usize;
                    for &idx in &img.pix.pix[i..i + n] {
                        dst[j..j + 4].copy_from_slice(&self.palette[usize::from(idx)]);
                        j += 4;
                    }
                }
            }
            Image::Cmyk(_) => {
                // scanner.go:262, the `default:` branch.
                let b = self.image.bounds();
                let (x1, x2, y1, y2) = (x1 + b.min_x, x2 + b.min_x, y1 + b.min_y, y2 + b.min_y);
                let mut j = 0;
                for y in y1..y2 {
                    for x in x1..x2 {
                        let (r16, g16, b16, a16) = self
                            .image
                            .at(x, y)
                            .map(|c| c.rgba())
                            .unwrap_or((0, 0, 0, 0));
                        let d = &mut dst[j..j + 4];
                        match a16 {
                            0xffff => {
                                d.copy_from_slice(&[
                                    (r16 >> 8) as u8,
                                    (g16 >> 8) as u8,
                                    (b16 >> 8) as u8,
                                    0xff,
                                ]);
                            }
                            0 => d.copy_from_slice(&[0, 0, 0, 0]),
                            _ => {
                                d[0] = (((r16 * 0xffff) / a16) >> 8) as u8;
                                d[1] = (((g16 * 0xffff) / a16) >> 8) as u8;
                                d[2] = (((b16 * 0xffff) / a16) >> 8) as u8;
                                d[3] = (a16 >> 8) as u8;
                            }
                        }
                        j += 4;
                    }
                }
            }
        }
    }
}

/// The Y'CbCr channel clamp (scanner.go:207): `v >> 16` when it fits in 24 bits, else 0 or 0xff
/// by sign.
fn clamp_shift16(v: i32) -> u8 {
    if (v as u32) & 0xff00_0000 == 0 {
        (v >> 16) as u8
    } else {
        !(v >> 31) as u8
    }
}
