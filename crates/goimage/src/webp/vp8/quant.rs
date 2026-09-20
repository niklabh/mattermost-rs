//! Port of `golang.org/x/image/vp8/quant.go`: parsing the quantization factors (section 9.6) and
//! the dequantization tables of section 14.1.

use super::Decoder;
use super::partition::UNIFORM_PROB;

/// `quant`: the DC/AC quantization factors of one segment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Quant {
    pub y1: [u16; 2],
    pub y2: [u16; 2],
    pub uv: [u16; 2],
}

/// `clip`: clip `x` to `[min, max]` inclusive.
pub(super) fn clip(x: i32, min: i32, max: i32) -> i32 {
    if x < min {
        return min;
    }
    if x > max {
        return max;
    }
    x
}

impl Decoder<'_> {
    /// `Decoder.parseQuant`.
    pub(super) fn parse_quant(&mut self) {
        let base_q0 = self.fp.read_uint(UNIFORM_PROB, 7);
        let dqy1_dc = self.fp.read_optional_int(UNIFORM_PROB, 4);
        const DQY1_AC: i32 = 0;
        let dqy2_dc = self.fp.read_optional_int(UNIFORM_PROB, 4);
        let dqy2_ac = self.fp.read_optional_int(UNIFORM_PROB, 4);
        let dquv_dc = self.fp.read_optional_int(UNIFORM_PROB, 4);
        let dquv_ac = self.fp.read_optional_int(UNIFORM_PROB, 4);
        for i in 0..super::N_SEGMENT {
            let mut q = base_q0 as i32;
            if self.segment_header.use_segment {
                if self.segment_header.relative_delta {
                    q += i32::from(self.segment_header.quantizer[i]);
                } else {
                    q = i32::from(self.segment_header.quantizer[i]);
                }
            }
            let quant = &mut self.quant[i];
            quant.y1[0] = DEQUANT_TABLE_DC[clip(q + dqy1_dc, 0, 127) as usize];
            quant.y1[1] = DEQUANT_TABLE_AC[clip(q + DQY1_AC, 0, 127) as usize];
            quant.y2[0] = DEQUANT_TABLE_DC[clip(q + dqy2_dc, 0, 127) as usize] * 2;
            quant.y2[1] = DEQUANT_TABLE_AC[clip(q + dqy2_ac, 0, 127) as usize] * 155 / 100;
            if quant.y2[1] < 8 {
                quant.y2[1] = 8;
            }
            // The 117 is not a typo. The dequant_init function in the spec's Reference Decoder
            // Source Code says to clamp the LHS value at 132, which is dequantTableDC[117].
            quant.uv[0] = DEQUANT_TABLE_DC[clip(q + dquv_dc, 0, 117) as usize];
            quant.uv[1] = DEQUANT_TABLE_AC[clip(q + dquv_ac, 0, 127) as usize];
        }
    }
}

/// `dequantTableDC`.
const DEQUANT_TABLE_DC: [u16; 128] = [
    4, 5, 6, 7, 8, 9, 10, 10, //
    11, 12, 13, 14, 15, 16, 17, 17, //
    18, 19, 20, 20, 21, 21, 22, 22, //
    23, 23, 24, 25, 25, 26, 27, 28, //
    29, 30, 31, 32, 33, 34, 35, 36, //
    37, 37, 38, 39, 40, 41, 42, 43, //
    44, 45, 46, 46, 47, 48, 49, 50, //
    51, 52, 53, 54, 55, 56, 57, 58, //
    59, 60, 61, 62, 63, 64, 65, 66, //
    67, 68, 69, 70, 71, 72, 73, 74, //
    75, 76, 76, 77, 78, 79, 80, 81, //
    82, 83, 84, 85, 86, 87, 88, 89, //
    91, 93, 95, 96, 98, 100, 101, 102, //
    104, 106, 108, 110, 112, 114, 116, 118, //
    122, 124, 126, 128, 130, 132, 134, 136, //
    138, 140, 143, 145, 148, 151, 154, 157, //
];

/// `dequantTableAC`.
const DEQUANT_TABLE_AC: [u16; 128] = [
    4, 5, 6, 7, 8, 9, 10, 11, //
    12, 13, 14, 15, 16, 17, 18, 19, //
    20, 21, 22, 23, 24, 25, 26, 27, //
    28, 29, 30, 31, 32, 33, 34, 35, //
    36, 37, 38, 39, 40, 41, 42, 43, //
    44, 45, 46, 47, 48, 49, 50, 51, //
    52, 53, 54, 55, 56, 57, 58, 60, //
    62, 64, 66, 68, 70, 72, 74, 76, //
    78, 80, 82, 84, 86, 88, 90, 92, //
    94, 96, 98, 100, 102, 104, 106, 108, //
    110, 112, 114, 116, 119, 122, 125, 128, //
    131, 134, 137, 140, 143, 146, 149, 152, //
    155, 158, 161, 164, 167, 170, 173, 177, //
    181, 185, 189, 193, 197, 201, 205, 209, //
    213, 217, 221, 225, 229, 234, 239, 245, //
    249, 254, 259, 264, 269, 274, 279, 284, //
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_is_inclusive_at_both_ends() {
        assert_eq!(clip(-1, 0, 127), 0);
        assert_eq!(clip(0, 0, 127), 0);
        assert_eq!(clip(127, 0, 127), 127);
        assert_eq!(clip(128, 0, 127), 127);
        assert_eq!(clip(200, 0, 117), 117);
    }

    /// The tables are monotonic, start at 4 and end where Go's do; `dequantTableDC[117]` is the
    /// 132 the `uv[0]` clamp is spelled as 117 to reach.
    #[test]
    fn the_dequantization_tables_are_gos() {
        assert_eq!(DEQUANT_TABLE_DC[0], 4);
        assert_eq!(DEQUANT_TABLE_DC[117], 132);
        assert_eq!(DEQUANT_TABLE_DC[127], 157);
        assert_eq!(DEQUANT_TABLE_AC[0], 4);
        assert_eq!(DEQUANT_TABLE_AC[127], 284);
        for w in DEQUANT_TABLE_AC.windows(2) {
            assert!(w[1] > w[0], "{w:?}");
        }
        for w in DEQUANT_TABLE_DC.windows(2) {
            assert!(w[1] >= w[0], "{w:?}");
        }
        assert_eq!(
            DEQUANT_TABLE_DC.iter().map(|&v| u32::from(v)).sum::<u32>(),
            8168
        );
        assert_eq!(
            DEQUANT_TABLE_AC.iter().map(|&v| u32::from(v)).sum::<u32>(),
            12723
        );
    }
}
