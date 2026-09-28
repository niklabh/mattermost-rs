//! Port of `qr/coding` (qr/coding/qr.go): the bit stream, the three encodings, the version table,
//! and the plan that places every bit of a symbol.

use crate::QrError;
use crate::gf256::RsEncoder;

/// `coding.Level`: L, M, Q, H.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 20% redundant.
    L = 0,
    /// 38% redundant.
    M = 1,
    /// 55% redundant.
    Q = 2,
    /// 65% redundant.
    H = 3,
}

/// `coding.Version`, 1 to 40.
pub(crate) type Version = usize;

pub(crate) const MIN_VERSION: Version = 1;
pub(crate) const MAX_VERSION: Version = 40;

/// `Version.sizeClass`.
fn size_class(v: Version) -> usize {
    if v <= 9 {
        0
    } else if v <= 26 {
        1
    } else {
        2
    }
}

/// `Version.DataBytes`.
pub(crate) fn data_bytes(v: Version, l: Level) -> usize {
    let vt = &VTAB[v];
    let lev = vt.level[l as usize];
    vt.bytes - lev.0 * lev.1
}

/// Port of `coding.Bits`: a bit stream, most significant bit first.
#[derive(Debug, Default)]
pub(crate) struct Bits {
    b: Vec<u8>,
    nbit: usize,
}

impl Bits {
    fn bits(&self) -> usize {
        self.nbit
    }

    fn append(&mut self, p: &[u8]) {
        self.b.extend_from_slice(p);
        self.nbit += 8 * p.len();
    }

    /// `Bits.Write`: the low `nbit` bits of `v`, most significant first. `-b.nbit & 7` is the
    /// number of bits left in the last byte.
    pub(crate) fn write(&mut self, mut v: u64, mut nbit: usize) {
        while nbit > 0 {
            let mut n = nbit.min(8);
            if self.nbit % 8 == 0 {
                self.b.push(0);
            } else {
                let m = (8 - self.nbit % 8) % 8;
                if n > m {
                    n = m;
                }
            }
            self.nbit += n;
            let sh = (nbit - n) as u32;
            let left = ((8 - self.nbit % 8) % 8) as u32;
            if let Some(last) = self.b.last_mut() {
                *last |= ((v >> sh) << left) as u8;
            }
            v -= (v >> sh) << sh;
            nbit -= n;
        }
    }

    /// `Bits.Pad`: the terminator, then byte alignment, then alternating `0xec`/`0x11`.
    fn pad(&mut self, mut n: usize) {
        if n <= 4 {
            self.write(0, n);
        } else {
            self.write(0, 4);
            n -= 4;
            let align = (8 - self.bits() % 8) % 8;
            n -= align;
            self.write(0, align);
            let pad = n / 8;
            let mut i = 0;
            while i < pad {
                self.write(0xec, 8);
                if i + 1 >= pad {
                    break;
                }
                self.write(0x11, 8);
                i += 2;
            }
        }
    }

    /// `Bits.AddCheckBytes`: pad to the version's data capacity, then append each block's
    /// Reed–Solomon check bytes (the last `extra` blocks one data byte longer).
    fn add_check_bytes(&mut self, v: Version, l: Level) -> Result<(), QrError> {
        let nd = data_bytes(v, l);
        if self.nbit < nd * 8 {
            self.pad(nd * 8 - self.nbit);
        }
        if self.nbit != nd * 8 {
            return Err(QrError::Internal("too much data"));
        }
        let data = self.b.clone();
        let vt = &VTAB[v];
        let (nblock, check) = vt.level[l as usize];
        let mut db = nd / nblock;
        let extra = nd % nblock;
        let rs = RsEncoder::new(check);
        let mut rest: &[u8] = &data;
        for i in 0..nblock {
            if i == nblock - extra {
                db += 1;
            }
            let chk = rs.ecc(&rest[..db]);
            self.append(&chk);
            rest = &rest[db..];
        }
        if self.b.len() != vt.bytes {
            return Err(QrError::Internal("check byte count"));
        }
        Ok(())
    }
}

/// `alphabet` (qr/coding/qr.go): the 45 characters of alphanumeric mode.
const ALPHABET: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";

fn alpha_index(c: u8) -> u64 {
    ALPHABET.bytes().position(|a| a == c).unwrap_or(0) as u64
}

/// The three `coding.Encoding`s `qr.Encode` chooses between.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Encoding<'a> {
    /// `coding.Num`.
    Num(&'a str),
    /// `coding.Alpha`.
    Alpha(&'a str),
    /// `coding.String`: bytes.
    String(&'a str),
}

impl Encoding<'_> {
    /// `Check`, per rune as Go ranges over the string.
    pub(crate) fn check(&self) -> bool {
        match self {
            Encoding::Num(s) => s.chars().all(|c| c.is_ascii_digit()),
            Encoding::Alpha(s) => s.chars().all(|c| ALPHABET.contains(c)),
            Encoding::String(_) => true,
        }
    }

    /// `Bits`: the mode indicator, the length field and the data, for version `v`.
    pub(crate) fn bits(&self, v: Version) -> usize {
        let class = size_class(v);
        match self {
            Encoding::Num(s) => 4 + [10, 12, 14][class] + (10 * s.len()).div_ceil(3),
            Encoding::Alpha(s) => 4 + [9, 11, 13][class] + (11 * s.len()).div_ceil(2),
            Encoding::String(s) => 4 + [8, 16, 16][class] + 8 * s.len(),
        }
    }

    /// `Encode`.
    fn encode(&self, b: &mut Bits, v: Version) {
        let class = size_class(v);
        match self {
            Encoding::Num(s) => {
                let s = s.as_bytes();
                b.write(1, 4);
                b.write(s.len() as u64, [10, 12, 14][class]);
                let digit = |c: u8| u64::from(c - b'0');
                let mut i = 0;
                while i + 3 <= s.len() {
                    b.write(
                        digit(s[i]) * 100 + digit(s[i + 1]) * 10 + digit(s[i + 2]),
                        10,
                    );
                    i += 3;
                }
                match s.len() - i {
                    1 => b.write(digit(s[i]), 4),
                    2 => b.write(digit(s[i]) * 10 + digit(s[i + 1]), 7),
                    _ => {}
                }
            }
            Encoding::Alpha(s) => {
                let s = s.as_bytes();
                b.write(2, 4);
                b.write(s.len() as u64, [9, 11, 13][class]);
                let mut i = 0;
                while i + 2 <= s.len() {
                    b.write(alpha_index(s[i]) * 45 + alpha_index(s[i + 1]), 11);
                    i += 2;
                }
                if i < s.len() {
                    b.write(alpha_index(s[i]), 6);
                }
            }
            Encoding::String(s) => {
                b.write(4, 4);
                b.write(s.len() as u64, [8, 16, 16][class]);
                for &c in s.as_bytes() {
                    b.write(u64::from(c), 8);
                }
            }
        }
    }
}

// `Pixel`: bit 0 black, bit 1 invert, bits 2..6 the role, bits 6.. the offset.
type Pixel = u32;
const BLACK: Pixel = 1;
const INVERT: Pixel = 2;

// `PixelRole`s.
const POSITION: u32 = 1;
const ALIGNMENT: u32 = 2;
const TIMING: u32 = 3;
const FORMAT: u32 = 4;
const PVERSION: u32 = 5;
const UNUSED: u32 = 6;
const DATA: u32 = 7;
const CHECK: u32 = 8;
const EXTRA: u32 = 9;

fn role_pixel(role: u32) -> Pixel {
    role << 2
}

fn role(p: Pixel) -> u32 {
    (p >> 2) & 15
}

fn offset(p: Pixel) -> u32 {
    p >> 6
}

fn offset_pixel(o: u32) -> Pixel {
    o << 6
}

/// `mfunc`: the eight mask conditions, on (row, column).
fn mask_inverts(mask: i32, i: usize, j: usize) -> bool {
    match mask {
        0 => (i + j) % 2 == 0,
        1 => i % 2 == 0,
        2 => j % 3 == 0,
        3 => (i + j) % 3 == 0,
        4 => (i / 2 + j / 3) % 2 == 0,
        5 => i * j % 2 + i * j % 3 == 0,
        6 => (i * j % 2 + i * j % 3) % 2 == 0,
        7 => (i * j % 3 + (i + j) % 2) % 2 == 0,
        _ => false,
    }
}

/// Port of `coding.Plan`.
pub(crate) struct Plan {
    version: Version,
    level: Level,
    data_bytes: usize,
    pixel: Vec<Vec<Pixel>>,
}

/// Port of `coding.Code`.
pub(crate) struct CodingCode {
    pub(crate) bitmap: Vec<u8>,
    pub(crate) size: usize,
    pub(crate) stride: usize,
}

impl Plan {
    /// Port of `NewPlan`.
    pub(crate) fn new(version: Version, level: Level, mask: i32) -> Result<Plan, QrError> {
        let mut p = vplan(version)?;
        fplan(level, mask, &mut p);
        lplan(version, level, &mut p)?;
        mplan(mask, &mut p);
        Ok(p)
    }

    /// Port of `Plan.Encode` for one encoding.
    pub(crate) fn encode(&self, text: Encoding<'_>) -> Result<CodingCode, QrError> {
        let mut b = Bits::default();
        if !text.check() {
            return Err(QrError::Internal("text does not fit its encoding"));
        }
        text.encode(&mut b, self.version);
        if b.bits() > self.data_bytes * 8 {
            return Err(QrError::Internal("cannot encode the bits into the code"));
        }
        b.add_check_bytes(self.version, self.level)?;
        let bytes = &b.b;

        let size = self.pixel.len();
        // Go's `(len(p.Pixel) + 7) &^ 7`: a bit count rounded up, used as a byte count.
        let stride = (size + 7) & !7;
        let mut bitmap = vec![0u8; stride * size];
        for (y, row) in self.pixel.iter().enumerate() {
            let crow = &mut bitmap[y * stride..(y + 1) * stride];
            for (x, &pix) in row.iter().enumerate() {
                let mut pix = pix;
                let r = role(pix);
                if r == DATA || r == CHECK {
                    let o = offset(pix) as usize;
                    if bytes[o / 8] & (1 << (7 - (o & 7))) != 0 {
                        pix ^= BLACK;
                    }
                }
                if pix & BLACK != 0 {
                    crow[x / 8] |= 1 << (7 - (x & 7));
                }
            }
        }
        Ok(CodingCode {
            bitmap,
            size,
            stride,
        })
    }
}

/// `vtab`: alignment position and stride, total bytes, version pattern, and per level the block
/// count and check bytes per block.
struct VersionInfo {
    apos: usize,
    astride: usize,
    bytes: usize,
    pattern: u32,
    level: [(usize, usize); 4],
}

const fn vi(
    apos: usize,
    astride: usize,
    bytes: usize,
    pattern: u32,
    level: [(usize, usize); 4],
) -> VersionInfo {
    VersionInfo {
        apos,
        astride,
        bytes,
        pattern,
        level,
    }
}

static VTAB: [VersionInfo; 41] = [
    vi(0, 0, 0, 0, [(0, 0); 4]),
    vi(100, 100, 26, 0x0, [(1, 7), (1, 10), (1, 13), (1, 17)]),
    vi(16, 100, 44, 0x0, [(1, 10), (1, 16), (1, 22), (1, 28)]),
    vi(20, 100, 70, 0x0, [(1, 15), (1, 26), (2, 18), (2, 22)]),
    vi(24, 100, 100, 0x0, [(1, 20), (2, 18), (2, 26), (4, 16)]),
    vi(28, 100, 134, 0x0, [(1, 26), (2, 24), (4, 18), (4, 22)]),
    vi(32, 100, 172, 0x0, [(2, 18), (4, 16), (4, 24), (4, 28)]),
    vi(20, 16, 196, 0x7c94, [(2, 20), (4, 18), (6, 18), (5, 26)]),
    vi(22, 18, 242, 0x85bc, [(2, 24), (4, 22), (6, 22), (6, 26)]),
    vi(24, 20, 292, 0x9a99, [(2, 30), (5, 22), (8, 20), (8, 24)]),
    vi(26, 22, 346, 0xa4d3, [(4, 18), (5, 26), (8, 24), (8, 28)]),
    vi(28, 24, 404, 0xbbf6, [(4, 20), (5, 30), (8, 28), (11, 24)]),
    vi(30, 26, 466, 0xc762, [(4, 24), (8, 22), (10, 26), (11, 28)]),
    vi(32, 28, 532, 0xd847, [(4, 26), (9, 22), (12, 24), (16, 22)]),
    vi(24, 20, 581, 0xe60d, [(4, 30), (9, 24), (16, 20), (16, 24)]),
    vi(24, 22, 655, 0xf928, [(6, 22), (10, 24), (12, 30), (18, 24)]),
    vi(
        24,
        24,
        733,
        0x10b78,
        [(6, 24), (10, 28), (17, 24), (16, 30)],
    ),
    vi(
        28,
        24,
        815,
        0x1145d,
        [(6, 28), (11, 28), (16, 28), (19, 28)],
    ),
    vi(
        28,
        26,
        901,
        0x12a17,
        [(6, 30), (13, 26), (18, 28), (21, 28)],
    ),
    vi(
        28,
        28,
        991,
        0x13532,
        [(7, 28), (14, 26), (21, 26), (25, 26)],
    ),
    vi(
        32,
        28,
        1085,
        0x149a6,
        [(8, 28), (16, 26), (20, 30), (25, 28)],
    ),
    vi(
        26,
        22,
        1156,
        0x15683,
        [(8, 28), (17, 26), (23, 28), (25, 30)],
    ),
    vi(
        24,
        24,
        1258,
        0x168c9,
        [(9, 28), (17, 28), (23, 30), (34, 24)],
    ),
    vi(
        28,
        24,
        1364,
        0x177ec,
        [(9, 30), (18, 28), (25, 30), (30, 30)],
    ),
    vi(
        26,
        26,
        1474,
        0x18ec4,
        [(10, 30), (20, 28), (27, 30), (32, 30)],
    ),
    vi(
        30,
        26,
        1588,
        0x191e1,
        [(12, 26), (21, 28), (29, 30), (35, 30)],
    ),
    vi(
        28,
        28,
        1706,
        0x1afab,
        [(12, 28), (23, 28), (34, 28), (37, 30)],
    ),
    vi(
        32,
        28,
        1828,
        0x1b08e,
        [(12, 30), (25, 28), (34, 30), (40, 30)],
    ),
    vi(
        24,
        24,
        1921,
        0x1cc1a,
        [(13, 30), (26, 28), (35, 30), (42, 30)],
    ),
    vi(
        28,
        24,
        2051,
        0x1d33f,
        [(14, 30), (28, 28), (38, 30), (45, 30)],
    ),
    vi(
        24,
        26,
        2185,
        0x1ed75,
        [(15, 30), (29, 28), (40, 30), (48, 30)],
    ),
    vi(
        28,
        26,
        2323,
        0x1f250,
        [(16, 30), (31, 28), (43, 30), (51, 30)],
    ),
    vi(
        32,
        26,
        2465,
        0x209d5,
        [(17, 30), (33, 28), (45, 30), (54, 30)],
    ),
    vi(
        28,
        28,
        2611,
        0x216f0,
        [(18, 30), (35, 28), (48, 30), (57, 30)],
    ),
    vi(
        32,
        28,
        2761,
        0x228ba,
        [(19, 30), (37, 28), (51, 30), (60, 30)],
    ),
    vi(
        28,
        24,
        2876,
        0x2379f,
        [(19, 30), (38, 28), (53, 30), (63, 30)],
    ),
    vi(
        22,
        26,
        3034,
        0x24b0b,
        [(20, 30), (40, 28), (56, 30), (66, 30)],
    ),
    vi(
        26,
        26,
        3196,
        0x2542e,
        [(21, 30), (43, 28), (59, 30), (70, 30)],
    ),
    vi(
        30,
        26,
        3362,
        0x26a64,
        [(22, 30), (45, 28), (62, 30), (74, 30)],
    ),
    vi(
        24,
        28,
        3532,
        0x27541,
        [(24, 30), (47, 28), (65, 30), (77, 30)],
    ),
    vi(
        28,
        28,
        3706,
        0x28c69,
        [(25, 30), (49, 28), (68, 30), (81, 30)],
    ),
];

// Go's `m[i][ti] = p; m[ti][i] = p` and the version-pattern loop index two ways at once.
#[allow(clippy::needless_range_loop)]
/// `vplan`: the fixed patterns of version `v` — timing, position and alignment boxes, the
/// version pattern and the one dark module.
fn vplan(v: Version) -> Result<Plan, QrError> {
    if !(MIN_VERSION..=MAX_VERSION).contains(&v) {
        return Err(QrError::Internal("invalid QR version"));
    }
    let siz = 17 + v * 4;
    let mut m = vec![vec![0 as Pixel; siz]; siz];

    const TI: usize = 6;
    for i in 0..siz {
        let mut p = role_pixel(TIMING);
        if i & 1 == 0 {
            p |= BLACK;
        }
        m[i][TI] = p;
        m[TI][i] = p;
    }

    pos_box(&mut m, 0, 0);
    pos_box(&mut m, siz - 7, 0);
    pos_box(&mut m, 0, siz - 7);

    let info = &VTAB[v];
    let mut x = 4;
    while x + 5 < siz {
        let mut y = 4;
        while y + 5 < siz {
            // Not where the position boxes are.
            if !((x < 7 && y < 7) || (x < 7 && y + 5 >= siz - 7) || (x + 5 >= siz - 7 && y < 7)) {
                align_box(&mut m, x, y);
            }
            if y == 4 {
                y = info.apos;
            } else {
                y += info.astride;
            }
        }
        if x == 4 {
            x = info.apos;
        } else {
            x += info.astride;
        }
    }

    let pat = info.pattern;
    if pat != 0 {
        let mut v = pat;
        for x in 0..6 {
            for y in 0..3 {
                let mut p = role_pixel(PVERSION);
                if v & 1 != 0 {
                    p |= BLACK;
                }
                m[siz - 11 + y][x] = p;
                m[x][siz - 11 + y] = p;
                v >>= 1;
            }
        }
    }

    // One dark module, never used for data.
    m[siz - 8][8] = role_pixel(UNUSED) | BLACK;

    Ok(Plan {
        version: v,
        level: Level::L,
        data_bytes: 0,
        pixel: m,
    })
}

/// `fplan`: the format information — level and mask, BCH-coded and masked — in both copies.
fn fplan(l: Level, mask: i32, p: &mut Plan) {
    let mut fb: u32 = ((l as u32) ^ 1) << 13;
    fb |= (mask as u32) << 10;
    const FORMAT_POLY: u32 = 0x537;
    let mut rem = fb;
    for i in (10..=14).rev() {
        if rem & (1 << i) != 0 {
            rem ^= FORMAT_POLY << (i - 10);
        }
    }
    fb |= rem;
    let invert: u32 = 0x5412;
    let siz = p.pixel.len();
    for i in 0..15u32 {
        let mut pix = role_pixel(FORMAT) + offset_pixel(i);
        if (fb >> i) & 1 == 1 {
            pix |= BLACK;
        }
        if (invert >> i) & 1 == 1 {
            pix ^= INVERT | BLACK;
        }
        let iu = i as usize;
        if i < 6 {
            p.pixel[iu][8] = pix;
        } else if i < 8 {
            p.pixel[iu + 1][8] = pix;
        } else if i < 9 {
            p.pixel[8][7] = pix;
        } else {
            p.pixel[8][14 - iu] = pix;
        }
        if i < 8 {
            p.pixel[8][siz - 1 - iu] = pix;
        } else {
            p.pixel[siz - 1 - (14 - iu)][8] = pix;
        }
    }
}

/// `lplan`: interleave the data and check bits by block and lay them down in the zig-zag.
fn lplan(v: Version, l: Level, p: &mut Plan) -> Result<(), QrError> {
    p.level = l;
    let (nblock, ne) = VTAB[v].level[l as usize];
    let total = VTAB[v].bytes;
    let nde = (total - ne * nblock) / nblock;
    let extra = (total - ne * nblock) % nblock;
    let data_bits = (nde * nblock + extra) * 8;
    let check_bits = ne * nblock * 8;
    p.data_bytes = total - ne * nblock;

    let data: Vec<Pixel> = (0..data_bits)
        .map(|i| role_pixel(DATA) | offset_pixel(i as u32))
        .collect();
    let check: Vec<Pixel> = (0..check_bits)
        .map(|i| role_pixel(CHECK) | offset_pixel((i + data_bits) as u32))
        .collect();

    let mut data_list: Vec<&[Pixel]> = Vec::with_capacity(nblock);
    let mut check_list: Vec<&[Pixel]> = Vec::with_capacity(nblock);
    let (mut d, mut c): (&[Pixel], &[Pixel]) = (&data, &check);
    for i in 0..nblock {
        let mut nd = nde;
        if i >= nblock - extra {
            nd += 1;
        }
        let (block, rest) = d.split_at(nd * 8);
        data_list.push(block);
        d = rest;
        let (block, rest) = c.split_at(ne * 8);
        check_list.push(block);
        c = rest;
    }
    if !d.is_empty() || !c.is_empty() {
        return Err(QrError::Internal("data/check math"));
    }

    let mut bits: Vec<Pixel> = Vec::with_capacity(data_bits + check_bits);
    for i in 0..nde + 1 {
        for b in &data_list {
            if i * 8 < b.len() {
                bits.extend_from_slice(&b[i * 8..(i + 1) * 8]);
            }
        }
    }
    for i in 0..ne {
        for b in &check_list {
            if i * 8 < b.len() {
                bits.extend_from_slice(&b[i * 8..(i + 1) * 8]);
            }
        }
    }
    if bits.len() != data_bits + check_bits {
        return Err(QrError::Internal("dst math"));
    }

    let siz = p.pixel.len();
    bits.extend(std::iter::repeat_n(role_pixel(EXTRA), 7));
    let mut src = bits.into_iter();
    let mut place = |pixel: &mut Pixel| -> Result<(), QrError> {
        if role(*pixel) == 0 {
            *pixel = src.next().ok_or(QrError::Internal("ran out of bits"))?;
        }
        Ok(())
    };
    let mut x = siz;
    while x > 0 {
        for y in (0..siz).rev() {
            place(&mut p.pixel[y][x - 1])?;
            place(&mut p.pixel[y][x - 2])?;
        }
        x -= 2;
        if x == 7 {
            // The vertical timing strip.
            x -= 1;
        }
        for y in 0..siz {
            place(&mut p.pixel[y][x - 1])?;
            place(&mut p.pixel[y][x - 2])?;
        }
        x -= 2;
    }
    Ok(())
}

/// `mplan`: apply the mask to the data, check and extra modules.
fn mplan(mask: i32, p: &mut Plan) {
    for (y, row) in p.pixel.iter_mut().enumerate() {
        for (x, pix) in row.iter_mut().enumerate() {
            let r = role(*pix);
            if (r == DATA || r == CHECK || r == EXTRA) && mask_inverts(mask, y, x) {
                *pix ^= BLACK | INVERT;
            }
        }
    }
}

/// `posBox`: a position square at (`x`, `y`) and its white separator.
fn pos_box(m: &mut [Vec<Pixel>], x: usize, y: usize) {
    let pos = role_pixel(POSITION);
    for dy in 0..7 {
        for dx in 0..7 {
            let mut p = pos;
            if dx == 0
                || dx == 6
                || dy == 0
                || dy == 6
                || (2..=4).contains(&dx) && (2..=4).contains(&dy)
            {
                p |= BLACK;
            }
            m[y + dy][x + dx] = p;
        }
    }
    let len = m.len() as isize;
    let (xi, yi) = (x as isize, y as isize);
    for dy in -1..8isize {
        if 0 <= yi + dy && yi + dy < len {
            let row = (yi + dy) as usize;
            if x > 0 {
                m[row][x - 1] = pos;
            }
            if x + 7 < m.len() {
                m[row][x + 7] = pos;
            }
        }
    }
    for dx in -1..8isize {
        if 0 <= xi + dx && xi + dx < len {
            let col = (xi + dx) as usize;
            if y > 0 {
                m[y - 1][col] = pos;
            }
            if y + 7 < m.len() {
                m[y + 7][col] = pos;
            }
        }
    }
}

/// `alignBox`: an alignment square with its top-left at (`x`, `y`).
fn align_box(m: &mut [Vec<Pixel>], x: usize, y: usize) {
    let align = role_pixel(ALIGNMENT);
    for dy in 0..5 {
        for dx in 0..5 {
            let mut p = align;
            if dx == 0 || dx == 4 || dy == 0 || dy == 4 || dx == 2 && dy == 2 {
                p |= BLACK;
            }
            m[y + dy][x + dx] = p;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_written_most_significant_first_across_bytes() {
        let mut b = Bits::default();
        b.write(0b101, 3);
        b.write(0b1_1111_0000, 9);
        b.write(0xabc, 12);
        assert_eq!(b.bits(), 24);
        assert_eq!(b.b, [0b1011_1111, 0b0000_1010, 0b1011_1100]);
    }

    #[test]
    fn padding_alternates_ec_and_11() {
        let mut b = Bits::default();
        b.write(1, 4);
        b.pad(4 + 4 + 8 * 5);
        assert_eq!(b.b, [0x10, 0xec, 0x11, 0xec, 0x11, 0xec]);
    }

    #[test]
    fn the_data_capacity_at_h() {
        assert_eq!(data_bytes(1, Level::H), 9);
        assert_eq!(data_bytes(40, Level::H), 1276);
        assert_eq!(data_bytes(40, Level::L), 2956);
    }
}
