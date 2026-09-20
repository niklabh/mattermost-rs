//! Port of Go's `image/jpeg/scan.go` (go1.26.4): image allocation, SOS processing (baseline,
//! extended and progressive, with restart intervals), successive-approximation refinement, and
//! block reconstruction.

use super::huffman::Table;
use super::idct::{Block, idct};
use super::reader::{
    AC_TABLE, BLOCK_SIZE, DC_TABLE, Decoder, JpegError, MAX_TH, RST0_MARKER, RST7_MARKER, UNZIG,
    rect,
};
use crate::image::{Pixels, Ratio, Rect, YCbCr};

/// One scan component: which frame component, and its DC/AC table selectors (scan.go:73).
#[derive(Clone, Copy, Default)]
struct ScanComp {
    comp_index: usize,
    td: u8,
    ta: u8,
}

impl Decoder<'_> {
    /// Port of `makeImg` (scan.go:12): the MCU-aligned image, then its `SubImage` to the true
    /// size — which keeps the MCU-sized backing buffers and strides.
    fn make_img(&mut self, mxx: usize, myy: usize) -> Result<(), JpegError> {
        let sub = rect(self.width, self.height);
        if self.n_comp == 1 {
            let m = Pixels::new(rect(8 * mxx, 8 * myy), 1);
            let r = sub.intersect(&m.rect);
            // `Gray.SubImage` of an empty rectangle is the zero `Gray`.
            self.img1 = Some(if r.is_empty() {
                Pixels {
                    pix: Vec::new(),
                    stride: 0,
                    rect: Rect::default(),
                }
            } else {
                Pixels { rect: r, ..m }
            });
            return Ok(());
        }

        let h0 = self.comp[0].h;
        let v0 = self.comp[0].v;
        let h_ratio = h0 / self.comp[1].h;
        let v_ratio = v0 / self.comp[1].v;
        let ratio = match h_ratio << 4 | v_ratio {
            0x11 => Ratio::R444,
            0x12 => Ratio::R440,
            0x21 => Ratio::R422,
            0x22 => Ratio::R420,
            0x41 => Ratio::R411,
            0x42 => Ratio::R410,
            // Go panics("unreachable"): processSOF admits no other ratio.
            _ => return Err(JpegError::Unsupported("luma/chroma subsampling ratio")),
        };
        let m = YCbCr::new(rect(8 * h0 * mxx, 8 * v0 * myy), ratio);
        let r = sub.intersect(&m.rect);
        self.img3 = Some(if r.is_empty() {
            YCbCr {
                y: Vec::new(),
                cb: Vec::new(),
                cr: Vec::new(),
                y_stride: 0,
                c_stride: 0,
                ratio,
                rect: Rect::default(),
            }
        } else {
            // YOffset and COffset of (0, 0) are 0, so the sub-image's planes are the whole
            // backing planes.
            YCbCr { rect: r, ..m }
        });

        if self.n_comp == 4 {
            let (h3, v3) = (self.comp[3].h, self.comp[3].v);
            self.black_pix = Some(vec![0; 8 * h3 * mxx * 8 * v3 * myy]);
            self.black_stride = 8 * h3 * mxx;
        }
        Ok(())
    }

    /// Port of `processSOS` (scan.go:57), section B.2.3.
    pub(crate) fn process_sos(&mut self, n: usize) -> Result<(), JpegError> {
        if self.n_comp == 0 {
            return Err(JpegError::Format("missing SOF marker"));
        }
        if n < 6 || 4 + 2 * self.n_comp < n || n % 2 != 0 {
            return Err(JpegError::Format("SOS has wrong length"));
        }
        self.read_full_tmp(0, n)?;
        let n_comp = usize::from(self.tmp[0]);
        if n != 4 + 2 * n_comp {
            return Err(JpegError::Format(
                "SOS length inconsistent with number of components",
            ));
        }
        let mut scan = [ScanComp::default(); 4];
        let mut total_hv = 0;
        for i in 0..n_comp {
            let cs = self.tmp[1 + 2 * i];
            let mut comp_index = None;
            for (j, comp) in self.comp[..self.n_comp].iter().enumerate() {
                if cs == comp.c {
                    comp_index = Some(j);
                }
            }
            let Some(comp_index) = comp_index else {
                return Err(JpegError::Format("unknown component selector"));
            };
            scan[i].comp_index = comp_index;
            for j in 0..i {
                if scan[i].comp_index == scan[j].comp_index {
                    return Err(JpegError::Format("repeated component selector"));
                }
            }
            total_hv += self.comp[comp_index].h * self.comp[comp_index].v;

            scan[i].td = self.tmp[2 + 2 * i] >> 4;
            let t = scan[i].td;
            if t > MAX_TH || (self.baseline && t > 1) {
                return Err(JpegError::Format("bad Td value"));
            }
            scan[i].ta = self.tmp[2 + 2 * i] & 0x0f;
            let t = scan[i].ta;
            if t > MAX_TH || (self.baseline && t > 1) {
                return Err(JpegError::Format("bad Ta value"));
            }
        }
        if self.n_comp > 1 && total_hv > 10 {
            return Err(JpegError::Format("total sampling factors too large"));
        }

        let (mut zig_start, mut zig_end, mut ah, mut al) =
            (0i32, BLOCK_SIZE as i32 - 1, 0u32, 0u32);
        if self.progressive {
            zig_start = i32::from(self.tmp[1 + 2 * n_comp]);
            zig_end = i32::from(self.tmp[2 + 2 * n_comp]);
            ah = u32::from(self.tmp[3 + 2 * n_comp] >> 4);
            al = u32::from(self.tmp[3 + 2 * n_comp] & 0x0f);
            if (zig_start == 0 && zig_end != 0)
                || zig_start > zig_end
                || BLOCK_SIZE as i32 <= zig_end
            {
                return Err(JpegError::Format("bad spectral selection bounds"));
            }
            if zig_start != 0 && n_comp != 1 {
                return Err(JpegError::Format(
                    "progressive AC coefficients for more than one component",
                ));
            }
            if ah != 0 && ah != al + 1 {
                return Err(JpegError::Format("bad successive approximation values"));
            }
        }

        let (h0, v0) = (self.comp[0].h, self.comp[0].v);
        let mxx = self.width.div_ceil(8 * h0);
        let myy = self.height.div_ceil(8 * v0);
        if self.img1.is_none() && self.img3.is_none() {
            self.make_img(mxx, myy)?;
        }
        if self.progressive {
            for s in &scan[..n_comp] {
                let ci = s.comp_index;
                if self.prog_coeffs[ci].is_none() {
                    let len = mxx * myy * self.comp[ci].h * self.comp[ci].v;
                    self.prog_coeffs[ci] = Some(vec![[0; BLOCK_SIZE]; len]);
                }
            }
        }

        self.bits = Default::default();
        let mut mcu = 0usize;
        let mut expected_rst = RST0_MARKER;
        let mut dc = [0i32; 4];
        let mut block_count = 0usize;
        let (mut bx, mut by);
        for my in 0..myy {
            for mx in 0..mxx {
                for s in &scan[..n_comp] {
                    let comp_index = s.comp_index;
                    let hi = self.comp[comp_index].h;
                    let vi = self.comp[comp_index].v;
                    for j in 0..hi * vi {
                        if n_comp != 1 {
                            bx = hi * mx + j % hi;
                            by = vi * my + j / hi;
                        } else {
                            let q = mxx * hi;
                            bx = block_count % q;
                            by = block_count / q;
                            block_count += 1;
                            if bx * 8 >= self.width || by * 8 >= self.height {
                                continue;
                            }
                        }

                        let slot = by * mxx * hi + bx;
                        let mut b: Block = if self.progressive {
                            match self.prog_coeffs[comp_index]
                                .as_ref()
                                .and_then(|c| c.get(slot))
                            {
                                Some(b) => *b,
                                None => return Err(JpegError::Format("block index out of range")),
                            }
                        } else {
                            [0; BLOCK_SIZE]
                        };

                        if ah != 0 {
                            self.refine(
                                &mut b,
                                Table {
                                    tc: AC_TABLE,
                                    th: usize::from(s.ta),
                                },
                                zig_start,
                                zig_end,
                                1i32.wrapping_shl(al),
                            )?;
                        } else {
                            let mut zig = zig_start;
                            if zig == 0 {
                                zig += 1;
                                // DC coefficient, section F.2.2.1.
                                let value = self.decode_huffman(Table {
                                    tc: DC_TABLE,
                                    th: usize::from(s.td),
                                })?;
                                if value > 16 {
                                    return Err(JpegError::Unsupported("excessive DC component"));
                                }
                                let dc_delta = self.receive_extend(value)?;
                                dc[comp_index] = dc[comp_index].wrapping_add(dc_delta);
                                b[0] = dc[comp_index].wrapping_shl(al);
                            }

                            if zig <= zig_end && self.eob_run > 0 {
                                self.eob_run -= 1;
                            } else {
                                // AC coefficients, section F.2.2.2.
                                let huff = Table {
                                    tc: AC_TABLE,
                                    th: usize::from(s.ta),
                                };
                                while zig <= zig_end {
                                    let value = self.decode_huffman(huff)?;
                                    let val0 = value >> 4;
                                    let val1 = value & 0x0f;
                                    if val1 != 0 {
                                        zig += i32::from(val0);
                                        if zig > zig_end {
                                            break;
                                        }
                                        let ac = self.receive_extend(val1)?;
                                        b[UNZIG[zig as usize]] = ac.wrapping_shl(al);
                                    } else {
                                        if val0 != 0x0f {
                                            self.eob_run = 1u16 << val0;
                                            if val0 != 0 {
                                                let bits = self.decode_bits(i32::from(val0))?;
                                                self.eob_run |= bits as u16;
                                            }
                                            self.eob_run = self.eob_run.wrapping_sub(1);
                                            break;
                                        }
                                        zig += 0x0f;
                                    }
                                    zig += 1;
                                }
                            }
                        }

                        if self.progressive {
                            if let Some(slot) = self.prog_coeffs[comp_index]
                                .as_mut()
                                .and_then(|c| c.get_mut(slot))
                            {
                                *slot = b;
                            }
                            continue;
                        }
                        self.reconstruct_block(&mut b, bx, by, comp_index)?;
                    }
                }
                mcu += 1;
                if self.ri > 0 && mcu % self.ri == 0 && mcu < mxx * myy {
                    // The RST marker should follow immediately; findRST resynchronises.
                    self.read_full_tmp(0, 2)?;
                    if self.tmp[0] != 0xff || self.tmp[1] != expected_rst {
                        self.find_rst(expected_rst)?;
                    }
                    expected_rst += 1;
                    if expected_rst == RST7_MARKER + 1 {
                        expected_rst = RST0_MARKER;
                    }
                    self.bits = Default::default();
                    dc = [0; 4];
                    self.eob_run = 0;
                }
            }
        }
        Ok(())
    }

    /// Port of `refine` (scan.go:343), section G.1.2.
    fn refine(
        &mut self,
        b: &mut Block,
        h: Table,
        zig_start: i32,
        zig_end: i32,
        delta: i32,
    ) -> Result<(), JpegError> {
        if zig_start == 0 {
            if zig_end != 0 {
                // Go panics("unreachable"): processSOS rejects these bounds first.
                return Err(JpegError::Format("bad spectral selection bounds"));
            }
            if self.decode_bit()? {
                b[0] |= delta;
            }
            return Ok(());
        }

        let mut zig = zig_start;
        if self.eob_run == 0 {
            while zig <= zig_end {
                let mut z = 0i32;
                let value = self.decode_huffman(h)?;
                let val0 = value >> 4;
                let val1 = value & 0x0f;

                match val1 {
                    0 => {
                        if val0 != 0x0f {
                            self.eob_run = 1u16 << val0;
                            if val0 != 0 {
                                let bits = self.decode_bits(i32::from(val0))?;
                                self.eob_run |= bits as u16;
                            }
                            break;
                        }
                    }
                    1 => {
                        z = delta;
                        if !self.decode_bit()? {
                            z = z.wrapping_neg();
                        }
                    }
                    _ => return Err(JpegError::Format("unexpected Huffman code")),
                }

                zig = self.refine_non_zeroes(b, zig, zig_end, i32::from(val0), delta)?;
                if zig > zig_end {
                    return Err(JpegError::Format("too many coefficients"));
                }
                if z != 0 {
                    b[UNZIG[zig as usize]] = z;
                }
                zig += 1;
            }
        }
        if self.eob_run > 0 {
            self.eob_run -= 1;
            self.refine_non_zeroes(b, zig, zig_end, -1, delta)?;
        }
        Ok(())
    }

    /// Port of `refineNonZeroes` (scan.go:409).
    fn refine_non_zeroes(
        &mut self,
        b: &mut Block,
        mut zig: i32,
        zig_end: i32,
        mut nz: i32,
        delta: i32,
    ) -> Result<i32, JpegError> {
        while zig <= zig_end {
            let u = UNZIG[zig as usize];
            if b[u] == 0 {
                if nz == 0 {
                    break;
                }
                nz -= 1;
                zig += 1;
                continue;
            }
            if !self.decode_bit()? {
                zig += 1;
                continue;
            }
            if b[u] >= 0 {
                b[u] = b[u].wrapping_add(delta);
            } else {
                b[u] = b[u].wrapping_sub(delta);
            }
            zig += 1;
        }
        Ok(zig)
    }

    /// Port of `reconstructProgressiveImage` (scan.go:433).
    pub(crate) fn reconstruct_progressive_image(&mut self) -> Result<(), JpegError> {
        let h0 = self.comp[0].h;
        if h0 == 0 {
            // No SOF was ever processed, so no scan allocated coefficients either.
            return Ok(());
        }
        let mxx = self.width.div_ceil(8 * h0);
        for i in 0..self.n_comp {
            let Some(mut coeffs) = self.prog_coeffs[i].take() else {
                continue;
            };
            let v = 8 * self.comp[0].v / self.comp[i].v;
            let h = 8 * self.comp[0].h / self.comp[i].h;
            let stride = mxx * self.comp[i].h;
            let mut by = 0;
            while by * v < self.height {
                let mut bx = 0;
                while bx * h < self.width {
                    let Some(b) = coeffs.get_mut(by * stride + bx) else {
                        return Err(JpegError::Format("block index out of range"));
                    };
                    self.reconstruct_block(b, bx, by, i)?;
                    bx += 1;
                }
                by += 1;
            }
            self.prog_coeffs[i] = Some(coeffs);
        }
        Ok(())
    }

    /// Port of `reconstructBlock` (scan.go:458): dequantise, inverse DCT, level shift and clip
    /// into the component's plane.
    fn reconstruct_block(
        &mut self,
        b: &mut Block,
        bx: usize,
        by: usize,
        comp_index: usize,
    ) -> Result<(), JpegError> {
        let qt = &self.quant[usize::from(self.comp[comp_index].tq)];
        for zig in 0..BLOCK_SIZE {
            b[UNZIG[zig]] = b[UNZIG[zig]].wrapping_mul(qt[zig]);
        }
        idct(b);
        let (dst, stride): (&mut [u8], usize) = if self.n_comp == 1 {
            match self.img1.as_mut() {
                Some(m) => (&mut m.pix, m.stride),
                None => return Err(JpegError::Format("missing image")),
            }
        } else {
            match (comp_index, self.img3.as_mut()) {
                (0, Some(m)) => (&mut m.y, m.y_stride),
                (1, Some(m)) => (&mut m.cb, m.c_stride),
                (2, Some(m)) => (&mut m.cr, m.c_stride),
                (3, _) => match self.black_pix.as_mut() {
                    Some(p) => (p, self.black_stride),
                    None => return Err(JpegError::Unsupported("too many components")),
                },
                _ => return Err(JpegError::Unsupported("too many components")),
            }
        };
        let base = 8 * (by * stride + bx);
        for y in 0..8 {
            let y8 = y * 8;
            let row = base + y * stride;
            for x in 0..8 {
                let mut c = b[y8 + x];
                if c < -128 {
                    c = 0;
                } else if c > 127 {
                    c = 255;
                } else {
                    c += 128;
                }
                // Go indexes `dst[yStride+x]`, which is always inside the MCU-aligned plane.
                match dst.get_mut(row + x) {
                    Some(p) => *p = c as u8,
                    None => return Err(JpegError::Format("block outside the image")),
                }
            }
        }
        Ok(())
    }

    /// Port of `findRST` (scan.go:502): advance past the next matching RST marker.
    fn find_rst(&mut self, expected_rst: u8) -> Result<(), JpegError> {
        loop {
            let mut i = 0;
            if self.tmp[0] == 0xff {
                if self.tmp[1] == expected_rst {
                    return Ok(());
                } else if self.tmp[1] == 0xff {
                    i = 1;
                } else if self.tmp[1] != 0x00 {
                    return Err(JpegError::Format("bad RST marker"));
                }
            } else if self.tmp[1] == 0xff {
                self.tmp[0] = 0xff;
                i = 1;
            }
            self.read_full_tmp(i, 2)?;
        }
    }
}
