//! Port of `github.com/golang/freetype/truetype/truetype.go`: parsing a TrueType font and the
//! metric look-ups the glyph loader and the face need.
//!
//! # Where this differs from Go, deliberately
//!
//! Go indexes the font's byte slices directly, so a malformed font that points past the end of a
//! table **panics**. Library code here may not panic, so every read is bounds-checked and a read
//! past the end is [`FontError::Format`] (`"index out of range"`). On a well-formed font the two
//! are the same; on a malformed one Go crashes the goroutine and this returns an error.
//!
//! The `name` table (`Font.Name`) and `VMetric` are not ported: nothing on the avatar's path reads
//! them. `unscaledVMetric` is, because the glyph loader's phantom points use it.

use crate::fixed::{Int26_6, Rectangle26_6};

/// `truetype.Index`: a glyph index.
pub type Index = u16;

/// `FormatError` / `UnsupportedError`, with Go's message text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FontError {
    #[error("freetype: invalid TrueType format: {0}")]
    Format(String),
    #[error("freetype: unsupported TrueType feature: {0}")]
    Unsupported(String),
}

fn oob() -> FontError {
    FontError::Format("index out of range".to_owned())
}

/// `u32(b, i)`, bounds-checked.
pub(crate) fn u32_at(b: &[u8], i: usize) -> Result<u32, FontError> {
    let s = b
        .get(i..i.checked_add(4).ok_or_else(oob)?)
        .ok_or_else(oob)?;
    Ok(u32::from(s[0]) << 24 | u32::from(s[1]) << 16 | u32::from(s[2]) << 8 | u32::from(s[3]))
}

/// `u16(b, i)`, bounds-checked.
pub(crate) fn u16_at(b: &[u8], i: usize) -> Result<u16, FontError> {
    let s = b
        .get(i..i.checked_add(2).ok_or_else(oob)?)
        .ok_or_else(oob)?;
    Ok(u16::from(s[0]) << 8 | u16::from(s[1]))
}

/// `b[i]`, bounds-checked.
pub(crate) fn u8_at(b: &[u8], i: usize) -> Result<u8, FontError> {
    b.get(i).copied().ok_or_else(oob)
}

const UNICODE_ENCODING_BMP_ONLY: u32 = 0x0000_0003;
const UNICODE_ENCODING_FULL: u32 = 0x0000_0004;
const MICROSOFT_SYMBOL_ENCODING: u32 = 0x0003_0000;
const MICROSOFT_UCS2_ENCODING: u32 = 0x0003_0001;
const MICROSOFT_UCS4_ENCODING: u32 = 0x0003_000a;

/// `truetype.HMetric`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HMetric {
    pub advance_width: Int26_6,
    pub left_side_bearing: Int26_6,
}

/// `truetype.VMetric`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VMetric {
    pub advance_height: Int26_6,
    pub top_side_bearing: Int26_6,
}

#[derive(Clone, Copy, Debug, Default)]
struct Cm {
    start: u32,
    end: u32,
    delta: u32,
    offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocaFormat {
    Short,
    Long,
}

/// `truetype.Font`.
#[derive(Clone, Debug)]
pub struct Font {
    cmap: Vec<u8>,
    pub(crate) glyf: Vec<u8>,
    head: Vec<u8>,
    hhea: Vec<u8>,
    hmtx: Vec<u8>,
    kern: Vec<u8>,
    pub(crate) loca: Vec<u8>,
    maxp: Vec<u8>,
    os2: Vec<u8>,
    vmtx: Vec<u8>,
    cmap_indexes: Vec<u8>,
    cm: Vec<Cm>,
    pub(crate) loca_offset_format: LocaFormat,
    n_glyph: i64,
    n_hmetric: i64,
    n_kern: i64,
    units_per_em: i32,
    ascent: i32,
    descent: i32,
    bounds: Rectangle26_6,
}

/// `readTable`.
fn read_table(ttf: &[u8], offset_length: &[u8]) -> Result<Vec<u8>, FontError> {
    // Go converts each `uint32` to `int`, which on a 64-bit build never goes negative.
    let offset = u32_at(offset_length, 0)? as usize;
    let length = u32_at(offset_length, 4)? as usize;
    let end = offset + length;
    if end > ttf.len() {
        return Err(FontError::Format(format!(
            "offset + length too large: {}",
            (offset as u32).wrapping_add(length as u32)
        )));
    }
    Ok(ttf[offset..end].to_vec())
}

/// `parseSubtables` for the `cmap` (the only caller ported): the first Unicode subtable, else the
/// last Microsoft Symbol/UCS-2/UCS-4 one.
fn parse_subtables(
    table: &[u8],
    name: &str,
    mut offset: usize,
    size: usize,
) -> Result<usize, FontError> {
    if table.len() < 4 {
        return Err(FontError::Format(format!("{name} too short")));
    }
    let n_subtables = usize::from(u16_at(table, 2)?);
    if table.len() < size * n_subtables + offset {
        return Err(FontError::Format(format!("{name} too short")));
    }
    let mut best = None;
    for _ in 0..n_subtables {
        let pid_psid = u32_at(table, offset)?;
        if pid_psid == UNICODE_ENCODING_BMP_ONLY || pid_psid == UNICODE_ENCODING_FULL {
            best = Some(offset);
            break;
        } else if pid_psid == MICROSOFT_SYMBOL_ENCODING
            || pid_psid == MICROSOFT_UCS2_ENCODING
            || pid_psid == MICROSOFT_UCS4_ENCODING
        {
            best = Some(offset);
        }
        offset += size;
    }
    best.ok_or_else(|| FontError::Unsupported(format!("{name} encoding")))
}

impl Font {
    /// Port of `truetype.Parse`.
    pub fn parse(ttf: &[u8]) -> Result<Font, FontError> {
        Self::parse_at(ttf, 0)
    }

    fn parse_at(ttf: &[u8], mut offset: usize) -> Result<Font, FontError> {
        if ttf.len() < offset + 12 {
            return Err(FontError::Format("TTF data is too short".to_owned()));
        }
        let original_offset = offset;
        let magic = u32_at(ttf, offset)?;
        offset += 4;
        match magic {
            0x0001_0000 => {}
            0x7474_6366 => {
                if original_offset != 0 {
                    return Err(FontError::Format("recursive TTC".to_owned()));
                }
                let ttc_version = u32_at(ttf, offset)?;
                offset += 4;
                if ttc_version != 0x0001_0000 && ttc_version != 0x0002_0000 {
                    return Err(FontError::Format("bad TTC version".to_owned()));
                }
                let num_fonts = u32_at(ttf, offset)? as i64;
                offset += 4;
                if num_fonts <= 0 {
                    return Err(FontError::Format("bad number of TTC fonts".to_owned()));
                }
                if ((ttf.len() - offset) / 4) < num_fonts as usize {
                    return Err(FontError::Format(
                        "TTC offset table is too short".to_owned(),
                    ));
                }
                let next = u32_at(ttf, offset)? as usize;
                if next == 0 || next > ttf.len() {
                    return Err(FontError::Format("bad TTC offset".to_owned()));
                }
                return Self::parse_at(ttf, next);
            }
            _ => return Err(FontError::Format("bad TTF version".to_owned())),
        }
        let n = usize::from(u16_at(ttf, offset)?);
        offset += 2;
        offset += 6;
        if ttf.len() < 16 * n + offset {
            return Err(FontError::Format("TTF data is too short".to_owned()));
        }
        let mut f = Font {
            cmap: Vec::new(),
            glyf: Vec::new(),
            head: Vec::new(),
            hhea: Vec::new(),
            hmtx: Vec::new(),
            kern: Vec::new(),
            loca: Vec::new(),
            maxp: Vec::new(),
            os2: Vec::new(),
            vmtx: Vec::new(),
            cmap_indexes: Vec::new(),
            cm: Vec::new(),
            loca_offset_format: LocaFormat::Short,
            n_glyph: 0,
            n_hmetric: 0,
            n_kern: 0,
            units_per_em: 0,
            ascent: 0,
            descent: 0,
            bounds: Rectangle26_6::default(),
        };
        for idx in 0..n {
            let x = 16 * idx + offset;
            let tag = &ttf[x..x + 4];
            let loc = &ttf[x + 8..x + 16];
            match tag {
                b"cmap" => f.cmap = read_table(ttf, loc)?,
                b"glyf" => f.glyf = read_table(ttf, loc)?,
                b"head" => f.head = read_table(ttf, loc)?,
                b"hhea" => f.hhea = read_table(ttf, loc)?,
                b"hmtx" => f.hmtx = read_table(ttf, loc)?,
                b"kern" => f.kern = read_table(ttf, loc)?,
                b"loca" => f.loca = read_table(ttf, loc)?,
                b"maxp" => f.maxp = read_table(ttf, loc)?,
                b"OS/2" => f.os2 = read_table(ttf, loc)?,
                b"vmtx" => f.vmtx = read_table(ttf, loc)?,
                // `cvt `, `fpgm`, `hdmx`, `name` and `prep` are read by Go too, and only its
                // hinter and `Name` use them. Their offsets are still validated, as Go's
                // `readTable` would.
                b"cvt " | b"fpgm" | b"hdmx" | b"name" | b"prep" => {
                    read_table(ttf, loc)?;
                }
                _ => {}
            }
        }
        f.parse_head()?;
        f.parse_maxp()?;
        f.parse_cmap()?;
        f.parse_kern()?;
        f.parse_hhea()?;
        Ok(f)
    }

    fn parse_cmap(&mut self) -> Result<(), FontError> {
        let offset = parse_subtables(&self.cmap, "cmap", 4, 8)?;
        let offset = u32_at(&self.cmap, offset + 4)? as usize;
        if offset == 0 || offset > self.cmap.len() {
            return Err(FontError::Format("bad cmap offset".to_owned()));
        }
        let format = u16_at(&self.cmap, offset)?;
        match format {
            4 => {
                let language = u16_at(&self.cmap, offset + 4)?;
                if language != 0 {
                    return Err(FontError::Unsupported(format!("language: {language}")));
                }
                let seg_count_x2 = usize::from(u16_at(&self.cmap, offset + 6)?);
                if seg_count_x2 % 2 == 1 {
                    return Err(FontError::Format(format!("bad segCountX2: {seg_count_x2}")));
                }
                let seg_count = seg_count_x2 / 2;
                let mut o = offset + 14;
                let mut cm = vec![Cm::default(); seg_count];
                for c in cm.iter_mut() {
                    c.end = u32::from(u16_at(&self.cmap, o)?);
                    o += 2;
                }
                o += 2;
                for c in cm.iter_mut() {
                    c.start = u32::from(u16_at(&self.cmap, o)?);
                    o += 2;
                }
                for c in cm.iter_mut() {
                    c.delta = u32::from(u16_at(&self.cmap, o)?);
                    o += 2;
                }
                for c in cm.iter_mut() {
                    c.offset = u32::from(u16_at(&self.cmap, o)?);
                    o += 2;
                }
                self.cm = cm;
                self.cmap_indexes = self.cmap.get(o..).ok_or_else(oob)?.to_vec();
                Ok(())
            }
            12 => {
                if u16_at(&self.cmap, offset + 2)? != 0 {
                    let head = self.cmap.get(offset..offset + 4).ok_or_else(oob)?;
                    let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
                    return Err(FontError::Format(format!("cmap format: {}", hex.join(" "))));
                }
                let length = u32_at(&self.cmap, offset + 4)?;
                let language = u32_at(&self.cmap, offset + 8)?;
                if language != 0 {
                    return Err(FontError::Unsupported(format!("language: {language}")));
                }
                let n_groups = u32_at(&self.cmap, offset + 12)?;
                if length != n_groups.wrapping_mul(12).wrapping_add(16) {
                    return Err(FontError::Format("inconsistent cmap length".to_owned()));
                }
                let mut o = offset + 16;
                let mut cm = Vec::with_capacity(n_groups as usize);
                for _ in 0..n_groups {
                    let start = u32_at(&self.cmap, o)?;
                    let end = u32_at(&self.cmap, o + 4)?;
                    let delta = u32_at(&self.cmap, o + 8)?.wrapping_sub(start);
                    cm.push(Cm {
                        start,
                        end,
                        delta,
                        offset: 0,
                    });
                    o += 12;
                }
                self.cm = cm;
                Ok(())
            }
            other => Err(FontError::Unsupported(format!("cmap format: {other}"))),
        }
    }

    fn parse_head(&mut self) -> Result<(), FontError> {
        if self.head.len() != 54 {
            return Err(FontError::Format(format!(
                "bad head length: {}",
                self.head.len()
            )));
        }
        self.units_per_em = i32::from(u16_at(&self.head, 18)?);
        self.bounds.min.x = Int26_6::from(u16_at(&self.head, 36)? as i16);
        self.bounds.min.y = Int26_6::from(u16_at(&self.head, 38)? as i16);
        self.bounds.max.x = Int26_6::from(u16_at(&self.head, 40)? as i16);
        self.bounds.max.y = Int26_6::from(u16_at(&self.head, 42)? as i16);
        self.loca_offset_format = match u16_at(&self.head, 50)? {
            0 => LocaFormat::Short,
            1 => LocaFormat::Long,
            other => {
                return Err(FontError::Format(format!("bad indexToLocFormat: {other}")));
            }
        };
        Ok(())
    }

    fn parse_hhea(&mut self) -> Result<(), FontError> {
        if self.hhea.len() != 36 {
            return Err(FontError::Format(format!(
                "bad hhea length: {}",
                self.hhea.len()
            )));
        }
        self.ascent = i32::from(u16_at(&self.hhea, 4)? as i16);
        self.descent = i32::from(u16_at(&self.hhea, 6)? as i16);
        self.n_hmetric = i64::from(u16_at(&self.hhea, 34)?);
        if 4 * self.n_hmetric + 2 * (self.n_glyph - self.n_hmetric) != self.hmtx.len() as i64 {
            return Err(FontError::Format(format!(
                "bad hmtx length: {}",
                self.hmtx.len()
            )));
        }
        Ok(())
    }

    fn parse_kern(&mut self) -> Result<(), FontError> {
        if self.kern.is_empty() {
            if self.n_kern != 0 {
                return Err(FontError::Format("bad kern table length".to_owned()));
            }
            return Ok(());
        }
        if self.kern.len() < 18 {
            return Err(FontError::Format("kern data too short".to_owned()));
        }
        let version = u16_at(&self.kern, 0)?;
        if version != 0 {
            return Err(FontError::Unsupported(format!("kern version: {version}")));
        }
        let n = u16_at(&self.kern, 2)?;
        if n == 0 {
            return Err(FontError::Unsupported("kern nTables: 0".to_owned()));
        }
        let length = i64::from(u16_at(&self.kern, 6)?);
        let coverage = u16_at(&self.kern, 8)?;
        if coverage != 0x0001 {
            return Err(FontError::Unsupported(format!(
                "kern coverage: 0x{coverage:04x}"
            )));
        }
        self.n_kern = i64::from(u16_at(&self.kern, 10)?);
        if 6 * self.n_kern != length - 14 {
            return Err(FontError::Format("bad kern table length".to_owned()));
        }
        Ok(())
    }

    fn parse_maxp(&mut self) -> Result<(), FontError> {
        if self.maxp.len() != 32 {
            return Err(FontError::Format(format!(
                "bad maxp length: {}",
                self.maxp.len()
            )));
        }
        self.n_glyph = i64::from(u16_at(&self.maxp, 4)?);
        Ok(())
    }

    /// `Font.scale`: `x / FUnitsPerEm`, rounded half away from zero, in `int32`.
    pub(crate) fn scale(&self, x: Int26_6) -> Int26_6 {
        let half = self.units_per_em / 2;
        let x = if x >= 0 {
            x.wrapping_add(half)
        } else {
            x.wrapping_sub(half)
        };
        if self.units_per_em == 0 {
            // Go divides by zero and panics; a zero `unitsPerEm` is a malformed font.
            return 0;
        }
        x.wrapping_div(self.units_per_em)
    }

    /// `Font.Bounds(scale)`.
    #[must_use]
    pub fn bounds(&self, scale: Int26_6) -> Rectangle26_6 {
        let b = self.bounds;
        Rectangle26_6 {
            min: crate::fixed::Point26_6::new(
                self.scale(scale.wrapping_mul(b.min.x)),
                self.scale(scale.wrapping_mul(b.min.y)),
            ),
            max: crate::fixed::Point26_6::new(
                self.scale(scale.wrapping_mul(b.max.x)),
                self.scale(scale.wrapping_mul(b.max.y)),
            ),
        }
    }

    /// `Font.FUnitsPerEm`.
    #[must_use]
    pub fn units_per_em(&self) -> i32 {
        self.units_per_em
    }

    pub(crate) fn ascent(&self) -> i32 {
        self.ascent
    }

    pub(crate) fn descent(&self) -> i32 {
        self.descent
    }

    /// `Font.Index`: the glyph for a rune, 0 (`.notdef`) when there is none.
    #[must_use]
    pub fn index(&self, x: char) -> Index {
        let c = u32::from(x);
        let (mut i, mut j) = (0usize, self.cm.len());
        while i < j {
            let h = i + (j - i) / 2;
            let cm = self.cm[h];
            if c < cm.start {
                j = h;
            } else if cm.end < c {
                i = h + 1;
            } else if cm.offset == 0 {
                return c.wrapping_add(cm.delta) as Index;
            } else {
                // `int(cm.offset) + 2*(h-len(f.cm)+int(c-cm.start))`, which can be negative
                // before it is added to — Go's arithmetic is on `int`.
                let offset = i64::from(cm.offset)
                    + 2 * (h as i64 - self.cm.len() as i64 + i64::from(c - cm.start));
                return usize::try_from(offset)
                    .ok()
                    .and_then(|o| u16_at(&self.cmap_indexes, o).ok())
                    .unwrap_or(0);
            }
        }
        0
    }

    /// `Font.unscaledHMetric`.
    pub(crate) fn unscaled_hmetric(&self, i: Index) -> Result<HMetric, FontError> {
        let j = i64::from(i);
        if j < 0 || self.n_glyph <= j {
            return Ok(HMetric::default());
        }
        if j >= self.n_hmetric {
            let p = (4 * (self.n_hmetric - 1)) as usize;
            return Ok(HMetric {
                advance_width: Int26_6::from(u16_at(&self.hmtx, p)?),
                left_side_bearing: Int26_6::from(u16_at(
                    &self.hmtx,
                    p + (2 * (j - self.n_hmetric)) as usize + 4,
                )? as i16),
            });
        }
        let j = j as usize;
        Ok(HMetric {
            advance_width: Int26_6::from(u16_at(&self.hmtx, 4 * j)?),
            left_side_bearing: Int26_6::from(u16_at(&self.hmtx, 4 * j + 2)? as i16),
        })
    }

    /// `Font.unscaledVMetric`.
    pub(crate) fn unscaled_vmetric(&self, i: Index, y_max: Int26_6) -> Result<VMetric, FontError> {
        let j = i64::from(i);
        if j < 0 || self.n_glyph <= j {
            return Ok(VMetric::default());
        }
        let j = j as usize;
        if 4 * j + 4 <= self.vmtx.len() {
            return Ok(VMetric {
                advance_height: Int26_6::from(u16_at(&self.vmtx, 4 * j)?),
                top_side_bearing: Int26_6::from(u16_at(&self.vmtx, 4 * j + 2)? as i16),
            });
        }
        if self.os2.len() >= 72 {
            let ascender = Int26_6::from(u16_at(&self.os2, 68)? as i16);
            let descender = Int26_6::from(u16_at(&self.os2, 70)? as i16);
            return Ok(VMetric {
                advance_height: ascender.wrapping_sub(descender),
                top_side_bearing: ascender.wrapping_sub(y_max),
            });
        }
        Ok(VMetric {
            advance_height: self.units_per_em,
            top_side_bearing: 0,
        })
    }

    /// `Font.Kern`: the kerning between two glyphs, 0 without a `kern` table.
    pub fn kern(&self, scale: Int26_6, i0: Index, i1: Index) -> Result<Int26_6, FontError> {
        if self.n_kern == 0 {
            return Ok(0);
        }
        let g = u32::from(i0) << 16 | u32::from(i1);
        let (mut lo, mut hi) = (0i64, self.n_kern);
        while lo < hi {
            let i = (lo + hi) / 2;
            let ig = u32_at(&self.kern, (18 + 6 * i) as usize)?;
            if ig < g {
                lo = i + 1;
            } else if ig > g {
                hi = i;
            } else {
                let v = Int26_6::from(u16_at(&self.kern, (22 + 6 * i) as usize)? as i16);
                return Ok(self.scale(scale.wrapping_mul(v)));
            }
        }
        Ok(0)
    }
}
