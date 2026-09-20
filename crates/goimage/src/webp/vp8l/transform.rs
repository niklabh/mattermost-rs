//! Port of `golang.org/x/image/vp8l/transform.go`: VP8L's four invertible image transforms
//! (section 3 of the spec).
//!
//! Every arithmetic step here is Go's. The pixel channels are `uint8` and Go's `+=` on them wraps,
//! so this port uses `wrapping_add` throughout; `avg2`, `clampAddSubtractFull` and
//! `clampAddSubtractHalf` widen to `int32` first, exactly where Go does, and the truncating `/ 2`
//! of a non-negative sum is the same operation in both languages.

/// `nTiles`: the number of tiles needed to cover `size` pixels with tiles of `1<<bits` a side.
pub(super) fn n_tiles(size: i32, bits: u32) -> i32 {
    (size + (1 << bits) - 1) >> bits
}

pub(super) const TRANSFORM_TYPE_PREDICTOR: u32 = 0;
pub(super) const TRANSFORM_TYPE_CROSS_COLOR: u32 = 1;
pub(super) const TRANSFORM_TYPE_SUBTRACT_GREEN: u32 = 2;
pub(super) const TRANSFORM_TYPE_COLOR_INDEXING: u32 = 3;
pub(super) const N_TRANSFORM_TYPES: usize = 4;

/// `transform`: the parameters of one invertible transform.
#[derive(Clone, Default)]
pub(super) struct Transform {
    /// The type of the transform.
    pub transform_type: u32,
    /// The width of the image before transformation — equivalently, after inverse transformation.
    /// The colour-indexing transform can reduce the width.
    pub old_width: i32,
    /// The log-2 size of the transform's tiles, for the predictor and cross-colour transforms.
    /// `8>>bits` is the number of bits per colour index, for the colour-index transform.
    pub bits: u32,
    /// The tile values, for the predictor and cross-colour transforms; the colour palette, for the
    /// colour-index transform.
    pub pix: Vec<u8>,
}

/// `inverseTransforms`: the dispatch table, as a match.
pub(super) fn inverse_transform(t: &Transform, pix: Vec<u8>, h: i32) -> Vec<u8> {
    match t.transform_type {
        TRANSFORM_TYPE_PREDICTOR => inverse_predictor(t, pix, h),
        TRANSFORM_TYPE_CROSS_COLOR => inverse_cross_color(t, pix, h),
        TRANSFORM_TYPE_SUBTRACT_GREEN => inverse_subtract_green(pix),
        TRANSFORM_TYPE_COLOR_INDEXING => inverse_color_indexing(t, pix, h),
        // `transformType` is a two-bit field, so there is no fifth case.
        _ => pix,
    }
}

/// `inversePredictor`.
fn inverse_predictor(t: &Transform, mut pix: Vec<u8>, h: i32) -> Vec<u8> {
    if t.old_width == 0 || h == 0 {
        return pix;
    }
    // The first pixel's predictor is mode 0 (opaque black).
    pix[3] = pix[3].wrapping_add(0xff);
    let mut p: usize = 4;
    let mask = (1i32 << t.bits) - 1;
    for _ in 1..t.old_width {
        // The rest of the first row's predictor is mode 1 (L).
        for k in 0..4 {
            pix[p + k] = pix[p + k].wrapping_add(pix[p + k - 4]);
        }
        p += 4;
    }
    let mut top: usize = 0;
    let tiles_per_row = n_tiles(t.old_width, t.bits);
    for y in 1..h {
        // The first column's predictor is mode 2 (T).
        for k in 0..4 {
            pix[p + k] = pix[p + k].wrapping_add(pix[top + k]);
        }
        p += 4;
        top += 4;

        let mut q = (4 * (y >> t.bits) * tiles_per_row) as usize;
        let mut predictor_mode = t.pix[q + 1] & 0x0f;
        q += 4;
        for x in 1..t.old_width {
            if x & mask == 0 {
                predictor_mode = t.pix[q + 1] & 0x0f;
                q += 4;
            }
            match predictor_mode {
                // Opaque black.
                0 => pix[p + 3] = pix[p + 3].wrapping_add(0xff),
                // L.
                1 => {
                    for k in 0..4 {
                        pix[p + k] = pix[p + k].wrapping_add(pix[p + k - 4]);
                    }
                }
                // T.
                2 => {
                    for k in 0..4 {
                        pix[p + k] = pix[p + k].wrapping_add(pix[top + k]);
                    }
                }
                // TR.
                3 => {
                    for k in 0..4 {
                        pix[p + k] = pix[p + k].wrapping_add(pix[top + 4 + k]);
                    }
                }
                // TL.
                4 => {
                    for k in 0..4 {
                        pix[p + k] = pix[p + k].wrapping_add(pix[top + k - 4]);
                    }
                }
                // Average2(Average2(L, TR), T).
                5 => {
                    for k in 0..4 {
                        let v = avg2(avg2(pix[p + k - 4], pix[top + 4 + k]), pix[top + k]);
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // Average2(L, TL).
                6 => {
                    for k in 0..4 {
                        let v = avg2(pix[p + k - 4], pix[top + k - 4]);
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // Average2(L, T).
                7 => {
                    for k in 0..4 {
                        let v = avg2(pix[p + k - 4], pix[top + k]);
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // Average2(TL, T).
                8 => {
                    for k in 0..4 {
                        let v = avg2(pix[top + k - 4], pix[top + k]);
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // Average2(T, TR).
                9 => {
                    for k in 0..4 {
                        let v = avg2(pix[top + k], pix[top + 4 + k]);
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // Average2(Average2(L, TL), Average2(T, TR)).
                10 => {
                    for k in 0..4 {
                        let v = avg2(
                            avg2(pix[p + k - 4], pix[top + k - 4]),
                            avg2(pix[top + k], pix[top + 4 + k]),
                        );
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // Select(L, T, TL).
                11 => {
                    let mut l_ = [0i32; 4];
                    let mut c_ = [0i32; 4];
                    let mut t_ = [0i32; 4];
                    for k in 0..4 {
                        l_[k] = i32::from(pix[p + k - 4]);
                        c_[k] = i32::from(pix[top + k - 4]);
                        t_[k] = i32::from(pix[top + k]);
                    }
                    let l: i32 = (0..4).map(|k| (c_[k] - t_[k]).abs()).sum();
                    let t: i32 = (0..4).map(|k| (c_[k] - l_[k]).abs()).sum();
                    let src = if l < t { &l_ } else { &t_ };
                    for k in 0..4 {
                        pix[p + k] = pix[p + k].wrapping_add(src[k] as u8);
                    }
                }
                // ClampAddSubtractFull(L, T, TL).
                12 => {
                    for k in 0..4 {
                        let v =
                            clamp_add_subtract_full(pix[p + k - 4], pix[top + k], pix[top + k - 4]);
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // ClampAddSubtractHalf(Average2(L, T), TL).
                13 => {
                    for k in 0..4 {
                        let v = clamp_add_subtract_half(
                            avg2(pix[p + k - 4], pix[top + k]),
                            pix[top + k - 4],
                        );
                        pix[p + k] = pix[p + k].wrapping_add(v);
                    }
                }
                // Modes 14 and 15 are not defined; Go's switch has no case for them either.
                _ => {}
            }
            p += 4;
            top += 4;
        }
    }
    pix
}

/// `inverseCrossColor`.
fn inverse_cross_color(t: &Transform, mut pix: Vec<u8>, h: i32) -> Vec<u8> {
    let (mut green_to_red, mut green_to_blue, mut red_to_blue) = (0i32, 0i32, 0i32);
    let mut p: usize = 0;
    let mask = (1i32 << t.bits) - 1;
    let tiles_per_row = n_tiles(t.old_width, t.bits);
    for y in 0..h {
        let mut q = (4 * (y >> t.bits) * tiles_per_row) as usize;
        for x in 0..t.old_width {
            if x & mask == 0 {
                red_to_blue = i32::from(t.pix[q] as i8);
                green_to_blue = i32::from(t.pix[q + 1] as i8);
                green_to_red = i32::from(t.pix[q + 2] as i8);
                q += 4;
            }
            let mut red = pix[p];
            let green = pix[p + 1];
            let mut blue = pix[p + 2];
            // Go's `uint8(uint32(x) >> 5)` on a negative product: the int32 becomes a uint32 by
            // two's-complement reinterpretation, then a logical shift, then a truncation. `as` in
            // Rust is the same reinterpretation at each step.
            red = red.wrapping_add(
                (green_to_red.wrapping_mul(i32::from(green as i8)) as u32 >> 5) as u8,
            );
            blue = blue.wrapping_add(
                (green_to_blue.wrapping_mul(i32::from(green as i8)) as u32 >> 5) as u8,
            );
            blue = blue
                .wrapping_add((red_to_blue.wrapping_mul(i32::from(red as i8)) as u32 >> 5) as u8);
            pix[p] = red;
            pix[p + 2] = blue;
            p += 4;
        }
    }
    pix
}

/// `inverseSubtractGreen`.
fn inverse_subtract_green(mut pix: Vec<u8>) -> Vec<u8> {
    let mut p = 0;
    while p < pix.len() {
        let green = pix[p + 1];
        pix[p] = pix[p].wrapping_add(green);
        pix[p + 2] = pix[p + 2].wrapping_add(green);
        p += 4;
    }
    pix
}

/// `inverseColorIndexing`.
fn inverse_color_indexing(t: &Transform, mut pix: Vec<u8>, h: i32) -> Vec<u8> {
    if t.bits == 0 {
        let mut p = 0;
        while p < pix.len() {
            let i = 4 * usize::from(pix[p + 1]);
            pix[p..p + 4].copy_from_slice(&t.pix[i..i + 4]);
            p += 4;
        }
        return pix;
    }

    let (v_mask, x_mask) = match t.bits {
        1 => (0x0fu32, 0x01i32),
        2 => (0x03, 0x03),
        3 => (0x01, 0x07),
        _ => (0, 0),
    };
    let bits_per_pixel = 8 >> t.bits;

    let (mut d, mut p, mut v) = (0usize, 0usize, 0u32);
    let mut dst = vec![0u8; (4 * t.old_width * h) as usize];
    for _y in 0..h {
        for x in 0..t.old_width {
            if x & x_mask == 0 {
                v = u32::from(pix[p + 1]);
                p += 4;
            }
            let i = 4 * (v & v_mask) as usize;
            dst[d..d + 4].copy_from_slice(&t.pix[i..i + 4]);
            d += 4;
            v >>= bits_per_pixel;
        }
    }
    dst
}

/// `avg2`.
fn avg2(a: u8, b: u8) -> u8 {
    ((i32::from(a) + i32::from(b)) / 2) as u8
}

/// `clampAddSubtractFull`.
fn clamp_add_subtract_full(a: u8, b: u8, c: u8) -> u8 {
    let x = i32::from(a) + i32::from(b) - i32::from(c);
    x.clamp(0, 255) as u8
}

/// `clampAddSubtractHalf`.
fn clamp_add_subtract_half(a: u8, b: u8) -> u8 {
    let x = i32::from(a) + (i32::from(a) - i32::from(b)) / 2;
    x.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n_tiles_rounds_up() {
        assert_eq!(n_tiles(0, 2), 0);
        assert_eq!(n_tiles(1, 2), 1);
        assert_eq!(n_tiles(4, 2), 1);
        assert_eq!(n_tiles(5, 2), 2);
        assert_eq!(n_tiles(75, 0), 75);
    }

    #[test]
    fn avg2_truncates_towards_zero() {
        assert_eq!(avg2(0, 1), 0);
        assert_eq!(avg2(255, 255), 255);
        assert_eq!(avg2(254, 255), 254);
        assert_eq!(avg2(1, 2), 1);
    }

    #[test]
    fn clamp_add_subtract_saturates_at_both_ends() {
        assert_eq!(clamp_add_subtract_full(0, 0, 1), 0);
        assert_eq!(clamp_add_subtract_full(255, 255, 0), 255);
        assert_eq!(clamp_add_subtract_full(100, 50, 30), 120);
        // (a - b) / 2 truncates towards zero, so a negative difference rounds up.
        assert_eq!(
            clamp_add_subtract_half(10, 15),
            (10 + (10i32 - 15) / 2) as u8
        );
        assert_eq!(clamp_add_subtract_half(0, 255), 0);
        assert_eq!(clamp_add_subtract_half(255, 0), 255);
        assert_eq!(clamp_add_subtract_half(100, 40), 130);
    }

    #[test]
    fn subtract_green_wraps_like_gos_uint8() {
        let pix = vec![0xf0, 0x20, 0xff, 0x01];
        let out = inverse_subtract_green(pix);
        assert_eq!(out, vec![0x10, 0x20, 0x1f, 0x01]);
    }

    #[test]
    fn cross_color_uses_a_logical_shift_of_the_reinterpreted_product() {
        // greenToRed = -1, green = -1 as int8 (0xff): the product is 1, shifted right by 5 it is 0.
        let t = Transform {
            transform_type: TRANSFORM_TYPE_CROSS_COLOR,
            old_width: 1,
            bits: 0,
            pix: vec![0x00, 0x00, 0xff, 0x00],
        };
        assert_eq!(
            inverse_cross_color(&t, vec![10, 0xff, 20, 0], 1),
            vec![10, 0xff, 20, 0]
        );
        // greenToRed = 1, green = -16 (0xf0): the product is -16, and the *logical* shift
        // 0xfffffff0 >> 5 is 0x07ffffff, whose low byte is 0xff.
        let t = Transform {
            transform_type: TRANSFORM_TYPE_CROSS_COLOR,
            old_width: 1,
            bits: 0,
            pix: vec![0x00, 0x00, 0x01, 0x00],
        };
        assert_eq!(
            inverse_cross_color(&t, vec![0, 0xf0, 0, 0], 1),
            vec![0xff, 0xf0, 0, 0]
        );
        // greenToRed = 2, green = 0x40: the product is 128 and 128 >> 5 is 4. This is the case a
        // wrong shift amount moves — >> 4 would give 8 and >> 6 would give 2.
        let t = Transform {
            transform_type: TRANSFORM_TYPE_CROSS_COLOR,
            old_width: 1,
            bits: 0,
            pix: vec![0x00, 0x00, 0x02, 0x00],
        };
        assert_eq!(
            inverse_cross_color(&t, vec![0, 0x40, 0, 0], 1),
            vec![0x04, 0x40, 0, 0]
        );
        // redToBlue reads the *updated* red, not the original: with redToBlue = 32 and a red that
        // the green term has just moved to 4, blue gains (32*4)>>5 = 4.
        let t = Transform {
            transform_type: TRANSFORM_TYPE_CROSS_COLOR,
            old_width: 1,
            bits: 0,
            pix: vec![0x20, 0x00, 0x02, 0x00],
        };
        assert_eq!(
            inverse_cross_color(&t, vec![0, 0x40, 0, 0], 1),
            vec![0x04, 0x40, 0x04, 0]
        );
    }
}
