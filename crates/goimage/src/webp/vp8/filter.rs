//! Port of `golang.org/x/image/vp8/filter.go`: the loop filter of chapter 15.
//!
//! `filter2` is the simple filter, `filter246` the normal one; both walk a band along a macroblock
//! edge, `iStep` pixels apart, reading `jStep` pixels to either side. All the arithmetic is on Go
//! `int`s derived from `uint8`s, so nothing here can overflow an `i32`; only the clamps and the
//! comparison direction of the thresholds decide the output.

use super::Decoder;

/// `filter2`: a 2-pixel wide or high band along an edge.
fn filter2(pix: &mut [u8], level: i32, mut index: usize, i_step: usize, j_step: usize) {
    for _ in 0..16 {
        let p1 = i32::from(pix[index - 2 * j_step]);
        let p0 = i32::from(pix[index - j_step]);
        let q0 = i32::from(pix[index]);
        let q1 = i32::from(pix[index + j_step]);
        if ((p0 - q0).abs() << 1) + ((p1 - q1).abs() >> 1) > level {
            index += i_step;
            continue;
        }
        let a = 3 * (q0 - p0) + clamp127(p1 - q1);
        let a1 = clamp15((a + 4) >> 3);
        let a2 = clamp15((a + 3) >> 3);
        pix[index - j_step] = clamp255(p0 + a2);
        pix[index] = clamp255(q0 - a1);
        index += i_step;
    }
}

/// `filter246`: a 2-, 4- or 6-pixel wide or high band along an edge.
#[allow(clippy::too_many_arguments)]
fn filter246(
    pix: &mut [u8],
    n: usize,
    level: i32,
    ilevel: i32,
    hlevel: i32,
    mut index: usize,
    i_step: usize,
    j_step: usize,
    four_not_six: bool,
) {
    for _ in 0..n {
        let p3 = i32::from(pix[index - 4 * j_step]);
        let p2 = i32::from(pix[index - 3 * j_step]);
        let p1 = i32::from(pix[index - 2 * j_step]);
        let p0 = i32::from(pix[index - j_step]);
        let q0 = i32::from(pix[index]);
        let q1 = i32::from(pix[index + j_step]);
        let q2 = i32::from(pix[index + 2 * j_step]);
        let q3 = i32::from(pix[index + 3 * j_step]);
        if ((p0 - q0).abs() << 1) + ((p1 - q1).abs() >> 1) > level {
            index += i_step;
            continue;
        }
        if (p3 - p2).abs() > ilevel
            || (p2 - p1).abs() > ilevel
            || (p1 - p0).abs() > ilevel
            || (q1 - q0).abs() > ilevel
            || (q2 - q1).abs() > ilevel
            || (q3 - q2).abs() > ilevel
        {
            index += i_step;
            continue;
        }
        if (p1 - p0).abs() > hlevel || (q1 - q0).abs() > hlevel {
            // Filter 2 pixels.
            let a = 3 * (q0 - p0) + clamp127(p1 - q1);
            let a1 = clamp15((a + 4) >> 3);
            let a2 = clamp15((a + 3) >> 3);
            pix[index - j_step] = clamp255(p0 + a2);
            pix[index] = clamp255(q0 - a1);
        } else if four_not_six {
            // Filter 4 pixels.
            let a = 3 * (q0 - p0);
            let a1 = clamp15((a + 4) >> 3);
            let a2 = clamp15((a + 3) >> 3);
            let a3 = (a1 + 1) >> 1;
            pix[index - 2 * j_step] = clamp255(p1 + a3);
            pix[index - j_step] = clamp255(p0 + a2);
            pix[index] = clamp255(q0 - a1);
            pix[index + j_step] = clamp255(q1 - a3);
        } else {
            // Filter 6 pixels.
            let a = clamp127(3 * (q0 - p0) + clamp127(p1 - q1));
            let a1 = (27 * a + 63) >> 7;
            let a2 = (18 * a + 63) >> 7;
            let a3 = (9 * a + 63) >> 7;
            pix[index - 3 * j_step] = clamp255(p2 + a3);
            pix[index - 2 * j_step] = clamp255(p1 + a2);
            pix[index - j_step] = clamp255(p0 + a1);
            pix[index] = clamp255(q0 - a1);
            pix[index + j_step] = clamp255(q1 - a2);
            pix[index + 2 * j_step] = clamp255(q2 - a3);
        }
        index += i_step;
    }
}

/// `filterParam`: the loop filter parameters of one macroblock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FilterParam {
    /// Thresholds used to smooth over the edges and interior of a macroblock. `level` is used by
    /// both filters; the inner level and high edge variance level only by the normal filter.
    pub level: u8,
    pub ilevel: u8,
    pub hlevel: u8,
    /// Whether the inner loop filter cannot be optimized out as a no-op for this macroblock.
    pub inner: bool,
}

impl Decoder<'_> {
    /// `Decoder.simpleFilter` (section 15.2).
    pub(super) fn simple_filter(&mut self) {
        let y_stride = self.img.y_stride;
        for mby in 0..self.mbh {
            for mbx in 0..self.mbw {
                let f = self.per_mb_filter_params[self.mbw * mby + mbx];
                if f.level == 0 {
                    continue;
                }
                let l = i32::from(f.level);
                let y_index = (mby * y_stride + mbx) * 16;
                if mbx > 0 {
                    filter2(&mut self.img.y, l + 4, y_index, y_stride, 1);
                }
                if f.inner {
                    filter2(&mut self.img.y, l, y_index + 0x4, y_stride, 1);
                    filter2(&mut self.img.y, l, y_index + 0x8, y_stride, 1);
                    filter2(&mut self.img.y, l, y_index + 0xc, y_stride, 1);
                }
                if mby > 0 {
                    filter2(&mut self.img.y, l + 4, y_index, 1, y_stride);
                }
                if f.inner {
                    filter2(&mut self.img.y, l, y_index + y_stride * 0x4, 1, y_stride);
                    filter2(&mut self.img.y, l, y_index + y_stride * 0x8, 1, y_stride);
                    filter2(&mut self.img.y, l, y_index + y_stride * 0xc, 1, y_stride);
                }
            }
        }
    }

    /// `Decoder.normalFilter` (section 15.3).
    pub(super) fn normal_filter(&mut self) {
        let y_stride = self.img.y_stride;
        let c_stride = self.img.c_stride;
        for mby in 0..self.mbh {
            for mbx in 0..self.mbw {
                let f = self.per_mb_filter_params[self.mbw * mby + mbx];
                if f.level == 0 {
                    continue;
                }
                let (l, il, hl) = (i32::from(f.level), i32::from(f.ilevel), i32::from(f.hlevel));
                let y_index = (mby * y_stride + mbx) * 16;
                let c_index = (mby * c_stride + mbx) * 8;
                if mbx > 0 {
                    filter246(
                        &mut self.img.y,
                        16,
                        l + 4,
                        il,
                        hl,
                        y_index,
                        y_stride,
                        1,
                        false,
                    );
                    filter246(
                        &mut self.img.cb,
                        8,
                        l + 4,
                        il,
                        hl,
                        c_index,
                        c_stride,
                        1,
                        false,
                    );
                    filter246(
                        &mut self.img.cr,
                        8,
                        l + 4,
                        il,
                        hl,
                        c_index,
                        c_stride,
                        1,
                        false,
                    );
                }
                if f.inner {
                    for k in [0x4usize, 0x8, 0xc] {
                        filter246(
                            &mut self.img.y,
                            16,
                            l,
                            il,
                            hl,
                            y_index + k,
                            y_stride,
                            1,
                            true,
                        );
                    }
                    filter246(
                        &mut self.img.cb,
                        8,
                        l,
                        il,
                        hl,
                        c_index + 0x4,
                        c_stride,
                        1,
                        true,
                    );
                    filter246(
                        &mut self.img.cr,
                        8,
                        l,
                        il,
                        hl,
                        c_index + 0x4,
                        c_stride,
                        1,
                        true,
                    );
                }
                if mby > 0 {
                    filter246(
                        &mut self.img.y,
                        16,
                        l + 4,
                        il,
                        hl,
                        y_index,
                        1,
                        y_stride,
                        false,
                    );
                    filter246(
                        &mut self.img.cb,
                        8,
                        l + 4,
                        il,
                        hl,
                        c_index,
                        1,
                        c_stride,
                        false,
                    );
                    filter246(
                        &mut self.img.cr,
                        8,
                        l + 4,
                        il,
                        hl,
                        c_index,
                        1,
                        c_stride,
                        false,
                    );
                }
                if f.inner {
                    for k in [0x4usize, 0x8, 0xc] {
                        let i = y_index + y_stride * k;
                        filter246(&mut self.img.y, 16, l, il, hl, i, 1, y_stride, true);
                    }
                    let i = c_index + c_stride * 0x4;
                    filter246(&mut self.img.cb, 8, l, il, hl, i, 1, c_stride, true);
                    filter246(&mut self.img.cr, 8, l, il, hl, i, 1, c_stride, true);
                }
            }
        }
    }

    /// `Decoder.computeFilterParams` (section 15.4).
    pub(super) fn compute_filter_params(&mut self) {
        for i in 0..super::N_SEGMENT {
            let mut base_level = self.filter_header.level;
            if self.segment_header.use_segment {
                base_level = self.segment_header.filter_strength[i];
                if self.segment_header.relative_delta {
                    base_level = base_level.wrapping_add(self.filter_header.level);
                }
            }

            for j in 0..2 {
                let p = &mut self.filter_params[i][j];
                p.inner = j != 0;
                let mut level = base_level;
                if self.filter_header.use_lf_delta {
                    // The libwebp C code has a "TODO: only CURRENT is handled for now."
                    level = level.wrapping_add(self.filter_header.ref_lf_delta[0]);
                    if j != 0 {
                        level = level.wrapping_add(self.filter_header.mode_lf_delta[0]);
                    }
                }
                if level <= 0 {
                    p.level = 0;
                    continue;
                }
                if level > 63 {
                    level = 63;
                }
                let mut ilevel = level;
                if self.filter_header.sharpness > 0 {
                    if self.filter_header.sharpness > 4 {
                        ilevel >>= 2;
                    } else {
                        ilevel >>= 1;
                    }
                    let x = 9i8.wrapping_sub(self.filter_header.sharpness as i8);
                    if ilevel > x {
                        ilevel = x;
                    }
                }
                if ilevel < 1 {
                    ilevel = 1;
                }
                p.ilevel = ilevel as u8;
                // `2*level + ilevel` is at most 2*63 + 63 = 189, inside a uint8.
                p.level = (2 * level as i32 + ilevel as i32) as u8;
                if self.frame_header.key_frame {
                    p.hlevel = if level < 15 {
                        0
                    } else if level < 40 {
                        1
                    } else {
                        2
                    };
                } else {
                    p.hlevel = if level < 15 {
                        0
                    } else if level < 20 {
                        1
                    } else if level < 40 {
                        2
                    } else {
                        3
                    };
                }
            }
        }
    }
}

/// `clamp15`.
fn clamp15(x: i32) -> i32 {
    x.clamp(-16, 15)
}

/// `clamp127`.
fn clamp127(x: i32) -> i32 {
    x.clamp(-128, 127)
}

/// `clamp255`.
fn clamp255(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clamps_have_gos_bounds() {
        // clamp15's low bound is -16, not -15: the name is about the positive end.
        assert_eq!(clamp15(-17), -16);
        assert_eq!(clamp15(-16), -16);
        assert_eq!(clamp15(15), 15);
        assert_eq!(clamp15(16), 15);
        assert_eq!(clamp127(-129), -128);
        assert_eq!(clamp127(-128), -128);
        assert_eq!(clamp127(127), 127);
        assert_eq!(clamp127(128), 127);
        assert_eq!(clamp255(-1), 0);
        assert_eq!(clamp255(256), 255);
    }

    /// The edge test is `abs(p0-q0)<<1 + abs(p1-q1)>>1 > level`, and Go's `<<`/`>>` bind tighter
    /// than `+`, so it is `(2*abs(p0-q0)) + (abs(p1-q1)/2)` and not `2*abs(p0-q0)+abs(p1-q1)` then
    /// halved. A band exactly at the threshold is filtered; one above it is left alone.
    #[test]
    fn filter2_respects_the_threshold_and_its_precedence() {
        // p1 p0 | q0 q1 laid out along a row, with a 4-pixel guard either side.
        let row = |p1: u8, p0: u8, q0: u8, q1: u8| {
            let mut v = vec![0u8; 16 * 4];
            for n in 0..16 {
                let i = 4 * n;
                v[i] = p1;
                v[i + 1] = p0;
                v[i + 2] = q0;
                v[i + 3] = q1;
            }
            v
        };
        // abs(p0-q0) = 10, abs(p1-q1) = 6: the measure is 20 + 3 = 23.
        let mut v = row(100, 100, 110, 106);
        filter2(&mut v, 23, 2, 4, 1);
        assert_ne!(&v[..4], &[100, 100, 110, 106]);
        let mut v = row(100, 100, 110, 106);
        filter2(&mut v, 22, 2, 4, 1);
        assert_eq!(&v[..4], &[100, 100, 110, 106]);
        // Had the expression been (2*10 + 6) >> 1 = 13, a level of 22 would have filtered.
    }

    #[test]
    fn filter2_walks_sixteen_bands() {
        let mut v = vec![0u8; 17 * 4];
        for n in 0..17 {
            v[4 * n] = 100;
            v[4 * n + 1] = 100;
            v[4 * n + 2] = 110;
            v[4 * n + 3] = 106;
        }
        filter2(&mut v, 255, 2, 4, 1);
        // The first sixteen rows moved; the seventeenth did not.
        assert_ne!(v[1], 100);
        assert_eq!(&v[64..68], &[100, 100, 110, 106]);
    }
}
