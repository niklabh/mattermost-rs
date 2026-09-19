//! Shared by every module's `go_parity` tests: the capped allocator, fixture loading, the mirror of
//! the oracle's deterministic generator (`reference/dump/behaviour_imaging.go`) and of its
//! `describe`/`encoded` summaries.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::image::{Color, Image, Paletted, Pixels, Ratio, Rect, YCbCr};

/// The decoders allocate from sizes read out of their input; a mutation that drops a bounds check
/// must fail this test binary, not the machine (memory note: runaway-test-oom-kills-session).
const ALLOC_CAP: usize = 6 << 30;

struct Capped;

static IN_USE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if IN_USE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size() > ALLOC_CAP {
            IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Capped = Capped;

/// One stage's fixture, parsed once per test binary.
pub fn fixture(stage: &str) -> &'static Json {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, &'static Json>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    if let Some(v) = map.get(stage) {
        return v;
    }
    let path = format!(
        "{}/../../fixtures/behaviour_imaging_{stage}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let v: &'static Json = Box::leak(Box::new(serde_json::from_str(&text).unwrap()));
    assert_eq!(
        v["arch"]["goarch"].as_str(),
        Some("arm64"),
        "the imaging oracle records arm64 arithmetic"
    );
    map.insert(stage.to_owned(), v);
    v
}

pub fn b64(s: &str) -> Vec<u8> {
    B64.decode(s).unwrap()
}

pub fn sha(b: &[u8]) -> String {
    let d = Sha256::digest(b);
    d.iter().map(|x| format!("{x:02x}")).collect()
}

/// Mirror of the oracle's `encoded`: compare an encoder's output to Go's, with the bytes when the
/// fixture carries them so a mismatch can say where.
pub fn assert_encoded(name: &str, got: &[u8], want: &Json) {
    if sha(got) == want["sha256"].as_str().unwrap() {
        return;
    }
    let mut msg = format!("{name}: len {} vs Go {}", got.len(), want["len"]);
    if let Some(b) = want["b64"].as_str() {
        let w = b64(b);
        let first = got.iter().zip(&w).position(|(a, b)| a != b);
        msg += &format!("; first difference at byte {first:?}");
    }
    panic!("{msg}");
}

// --- generator mirror ---------------------------------------------------------------------------

pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

pub fn hash4(seed: u64, a: i64, b: i64, c: i64) -> u64 {
    mix64(
        seed.wrapping_add((a as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .wrapping_add((b as u64).wrapping_mul(0xc2b2ae3d27d4eb4f))
            .wrapping_add((c as u64).wrapping_mul(0x165667b19e3779f9)),
    )
}

pub fn sample(pattern: &str, seed: u64, x: i64, y: i64, k: i64, w: i64, h: i64) -> u8 {
    match pattern {
        "noise" => (hash4(seed, x, y, k) >> 56) as u8,
        "gradient" => {
            let v = if k % 2 == 0 {
                x * 255 / (w - 1).max(1)
            } else {
                y * 255 / (h - 1).max(1)
            };
            ((v + k * 37) & 0xff) as u8
        }
        "blocks" => {
            let base = (hash4(seed, x / 16, y / 16, k) >> 56) as i64;
            let grain = (hash4(seed + 1, x, y, k) >> 60) as i64;
            ((base + grain) & 0xff) as u8
        }
        "smooth" => {
            let v = (x * x + 2 * y * y) / (w + h + 1)
                + 3 * x
                + 50 * k
                + (hash4(seed, x, y, k) >> 61) as i64;
            (v & 0xff) as u8
        }
        "flat" => (hash4(seed, 0, 0, k) >> 56) as u8,
        other => panic!("unknown pattern {other}"),
    }
}

pub fn alpha8(mode: &str, seed: u64, x: i64, y: i64, s: u8) -> u8 {
    match mode {
        "opaque" => 0xff,
        "binary" => {
            if hash4(seed + 2, x, y, 0) >> 63 == 0 {
                0
            } else {
                0xff
            }
        }
        "mixed" => match hash4(seed + 3, x, y, 0) >> 56 {
            r if r < 64 => 0,
            r if r < 128 => 0xff,
            _ => s,
        },
        "soft" => s,
        other => panic!("unknown alpha mode {other}"),
    }
}

pub fn alpha16(mode: &str, seed: u64, x: i64, y: i64, s: u32) -> u32 {
    match mode {
        "opaque" => 0xffff,
        "binary" => {
            if hash4(seed + 2, x, y, 0) >> 63 == 0 {
                0
            } else {
                0xffff
            }
        }
        "mixed" => match hash4(seed + 3, x, y, 0) >> 56 {
            r if r < 64 => 0,
            r if r < 128 => 0xffff,
            _ => s,
        },
        "soft" => s,
        other => panic!("unknown alpha mode {other}"),
    }
}

pub fn ratio_of(s: &str) -> Ratio {
    match s {
        "444" => Ratio::R444,
        "422" => Ratio::R422,
        "420" => Ratio::R420,
        "440" => Ratio::R440,
        "411" => Ratio::R411,
        "410" => Ratio::R410,
        other => panic!("unknown ratio {other}"),
    }
}

/// Mirror of `imgSpec.build`.
pub fn build(spec: &Json) -> Image {
    let kind = spec["kind"].as_str().unwrap();
    let w = spec["w"].as_i64().unwrap();
    let h = spec["h"].as_i64().unwrap();
    let pattern = spec["pattern"].as_str().unwrap();
    let alpha = spec["alpha"].as_str().unwrap();
    let seed = spec["seed"].as_u64().unwrap();
    let r = Rect::new(0, 0, w, h);
    let smp = |x: i64, y: i64, k: i64| sample(pattern, seed, x, y, k, w, h);
    match kind {
        "gray" => {
            let mut m = Pixels::new(r, 1);
            for y in 0..h {
                for x in 0..w {
                    let i = y as usize * m.stride + x as usize;
                    m.pix[i] = smp(x, y, 0);
                }
            }
            Image::Gray(m)
        }
        "gray16" => {
            let mut m = Pixels::new(r, 2);
            for y in 0..h {
                for x in 0..w {
                    let i = y as usize * m.stride + 2 * x as usize;
                    m.pix[i] = smp(x, y, 0);
                    m.pix[i + 1] = smp(x, y, 1);
                }
            }
            Image::Gray16(m)
        }
        "nrgba" | "rgba" => {
            let mut m = Pixels::new(r, 4);
            for y in 0..h {
                for x in 0..w {
                    let i = 4 * (y * w + x) as usize;
                    let a = alpha8(alpha, seed, x, y, smp(x, y, 3));
                    for k in 0..3 {
                        let mut c = smp(x, y, k);
                        if kind == "rgba" {
                            c = (i64::from(c) * i64::from(a) / 255) as u8;
                        }
                        m.pix[i + k as usize] = c;
                    }
                    m.pix[i + 3] = a;
                }
            }
            if kind == "rgba" {
                Image::Rgba(m)
            } else {
                Image::Nrgba(m)
            }
        }
        "nrgba64" | "rgba64" => {
            let mut m = Pixels::new(r, 8);
            for y in 0..h {
                for x in 0..w {
                    let i = 8 * (y * w + x) as usize;
                    let a = alpha16(
                        alpha,
                        seed,
                        x,
                        y,
                        u32::from(smp(x, y, 6)) << 8 | u32::from(smp(x, y, 7)),
                    );
                    for k in 0..3 {
                        let mut c =
                            u32::from(smp(x, y, 2 * k)) << 8 | u32::from(smp(x, y, 2 * k + 1));
                        if kind == "rgba64" {
                            c = c * a / 0xffff;
                        }
                        m.pix[i + 2 * k as usize] = (c >> 8) as u8;
                        m.pix[i + 2 * k as usize + 1] = c as u8;
                    }
                    m.pix[i + 6] = (a >> 8) as u8;
                    m.pix[i + 7] = a as u8;
                }
            }
            if kind == "rgba64" {
                Image::Rgba64(m)
            } else {
                Image::Nrgba64(m)
            }
        }
        "paletted" => {
            let n = spec["palette"].as_i64().unwrap();
            let palette = (0..n)
                .map(|i| {
                    let cr = (hash4(seed + 7, i, 0, 0) >> 56) as u8;
                    let cg = (hash4(seed + 7, i, 0, 1) >> 56) as u8;
                    let cb = (hash4(seed + 7, i, 0, 2) >> 56) as u8;
                    let a = alpha8(
                        alpha,
                        seed + 7,
                        i,
                        0,
                        (hash4(seed + 7, i, 0, 3) >> 56) as u8,
                    );
                    if a == 0xff {
                        Color::Rgba([cr, cg, cb, 0xff])
                    } else {
                        Color::Nrgba([cr, cg, cb, a])
                    }
                })
                .collect();
            let mut m = Pixels::new(r, 1);
            for y in 0..h {
                for x in 0..w {
                    let i = y as usize * m.stride + x as usize;
                    m.pix[i] = (i64::from(smp(x, y, 0)) % n) as u8;
                }
            }
            Image::Paletted(Paletted { pix: m, palette })
        }
        "ycbcr" => {
            let mut m = YCbCr::new(r, ratio_of(spec["ratio"].as_str().unwrap()));
            for y in 0..h {
                for x in 0..w {
                    let i = y as usize * m.y_stride + x as usize;
                    m.y[i] = smp(x, y, 0);
                }
            }
            let cw = m.c_stride;
            let ch = m.cb.len() / cw.max(1);
            for cy in 0..ch {
                for cx in 0..cw {
                    m.cb[cy * cw + cx] = smp(cx as i64, cy as i64, 1);
                    m.cr[cy * cw + cx] = smp(cx as i64, cy as i64, 2);
                }
            }
            Image::YCbCr(m)
        }
        "cmyk" => {
            let mut m = Pixels::new(r, 4);
            for y in 0..h {
                for x in 0..w {
                    let i = y as usize * m.stride + 4 * x as usize;
                    for k in 0..4 {
                        m.pix[i + k as usize] = smp(x, y, k);
                    }
                }
            }
            Image::Cmyk(m)
        }
        other => panic!("unknown kind {other}"),
    }
}

/// Mirror of the oracle's `describe`, as JSON, so a fixture's `describe` object compares with `==`.
pub fn describe(m: &Image) -> Json {
    use serde_json::json;
    let b = m.bounds();
    let mut d = json!({ "rect": [b.min_x, b.min_y, b.max_x, b.max_y] });
    let o = d.as_object_mut().unwrap();
    let mut plain = |t: &str, p: &Pixels| {
        o.insert("type".into(), json!(t));
        o.insert("stride".into(), json!(p.stride));
        o.insert("pix_sha256".into(), json!(sha(&p.pix)));
    };
    match m {
        Image::Gray(p) => plain("gray", p),
        Image::Gray16(p) => plain("gray16", p),
        Image::Rgba(p) => plain("rgba", p),
        Image::Rgba64(p) => plain("rgba64", p),
        Image::Nrgba(p) => plain("nrgba", p),
        Image::Nrgba64(p) => plain("nrgba64", p),
        Image::Cmyk(p) => plain("cmyk", p),
        Image::Paletted(p) => {
            plain("paletted", &p.pix);
            o.insert(
                "palette".into(),
                Json::Array(p.palette.iter().map(palette_entry).collect()),
            );
        }
        Image::YCbCr(p) => {
            o.insert("type".into(), json!("ycbcr"));
            o.insert("ratio".into(), json!(p.ratio.go_name()));
            o.insert("y_stride".into(), json!(p.y_stride));
            o.insert("c_stride".into(), json!(p.c_stride));
            o.insert("y_sha256".into(), json!(sha(&p.y)));
            o.insert("cb_sha256".into(), json!(sha(&p.cb)));
            o.insert("cr_sha256".into(), json!(sha(&p.cr)));
            o.insert("y_len".into(), json!(p.y.len()));
            o.insert("c_len".into(), json!(p.cb.len()));
        }
    }
    d
}

pub fn palette_entry(c: &Color) -> Json {
    use serde_json::json;
    match *c {
        Color::Rgba([r, g, b, a]) => json!(["rgba", r, g, b, a]),
        Color::Nrgba([r, g, b, a]) => json!(["nrgba", r, g, b, a]),
        other => panic!("palette entry {other:?} is not one the oracle can describe"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator mirror against every generated input the oracle hashed. Everything else in
    /// the crate's parity suite rests on this.
    #[test]
    fn the_generator_mirror_reproduces_every_oracle_input() {
        let mut n = 0;
        for stage in ["png", "jpeg"] {
            for c in fixture(stage)["encode"].as_array().unwrap() {
                assert_eq!(
                    describe(&build(&c["spec"])),
                    c["input"],
                    "{stage} {}",
                    c["spec"]
                );
                n += 1;
            }
        }
        for c in fixture("resize")["cases"].as_array().unwrap() {
            assert_eq!(
                describe(&build(&c["spec"])),
                c["input"],
                "resize {}",
                c["spec"]
            );
            n += 1;
        }
        assert!(n > 3000, "{n}");
    }
}
