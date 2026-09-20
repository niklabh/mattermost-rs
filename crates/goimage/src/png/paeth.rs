//! Port of image/png/paeth.go: the Paeth predictor used by filter type 4.

/// `paeth` (paeth.go:21), as the PNG spec defines it.
pub fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let pc = i32::from(c);
    let mut pa = i32::from(b) - pc;
    let mut pb = i32::from(a) - pc;
    let pc = (pa + pb).abs();
    pa = pa.abs();
    pb = pb.abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// `filterPaeth` (paeth.go:46): undo the Paeth filter on `cdat` given the previous row `pdat`.
pub fn filter_paeth(cdat: &mut [u8], pdat: &[u8], bytes_per_pixel: usize) {
    for i in 0..bytes_per_pixel {
        let (mut a, mut c) = (0i32, 0i32);
        let mut j = i;
        while j < cdat.len() {
            let b = i32::from(pdat[j]);
            let pa = b - c;
            let pb = a - c;
            let pc = (pa + pb).abs();
            let (pa, pb) = (pa.abs(), pb.abs());
            if pa <= pb && pa <= pc {
                // No-op.
            } else if pb <= pc {
                a = b;
            } else {
                a = c;
            }
            a += i32::from(cdat[j]);
            a &= 0xff;
            cdat[j] = a as u8;
            c = b;
            j += bytes_per_pixel;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `filterPaeth` is the inverse of applying `paeth` byte by byte.
    #[test]
    fn filter_paeth_inverts_the_predictor() {
        let prev: Vec<u8> = (0..24u32).map(|i| (i * 37 + 5) as u8).collect();
        let row: Vec<u8> = (0..24u32).map(|i| (i * 91 + 200) as u8).collect();
        for bpp in [1, 2, 3, 4, 6, 8] {
            let mut filtered = row.clone();
            for i in 0..row.len() {
                let a = if i >= bpp { row[i - bpp] } else { 0 };
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                filtered[i] = row[i].wrapping_sub(paeth(a, prev[i], c));
            }
            filter_paeth(&mut filtered, &prev, bpp);
            assert_eq!(filtered, row, "bpp {bpp}");
        }
    }
}
