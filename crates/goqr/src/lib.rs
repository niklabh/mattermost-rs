//! Port of `github.com/mattermost/rsc/qr@v0.0.0-20160330161541-bbaefb05eaa0` — Russ Cox's
//! `rsc.io/qr`, with its `qr/coding` and `gf256` packages — byte for byte: [`encode`] picks the
//! encoding and version exactly as `qr.Encode` does, and [`Code::png`] is the library's own PNG
//! writer, not a general one. Mattermost renders its MFA enrolment QR code with it
//! (`platform/shared/mfa`, `qr.Encode(authLink, qr.H).PNG()`).
//!
//! Not Mattermost source: BSD-3-Clause (LICENSE). This crate must never depend on an `mm-*` crate.
//!
//! # Byte-exactness notes
//!
//! * The encoding is chosen for the **whole** text: numeric if every rune is a digit (so `""` is
//!   numeric), else alphanumeric if every rune is in the 45-character set, else bytes.
//! * The mask is always 0 — the Go library never evaluates the eight (`TODO: Pick appropriate
//!   mask`).
//! * `Code::stride` is `(size + 7) &^ 7`, a bit count rounded up and then used as a *byte* count:
//!   the bitmap is eight times wider than it needs to be. Kept, since `Code` exposes it.
//! * The PNG's zlib stream is one fixed-Huffman deflate block built from literal bytes and
//!   back-references to the previous row, with an Adler-32 computed in closed form per run.

mod coding;
mod gf256;
mod png;

pub use coding::Level;

/// What [`encode`] can refuse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QrError {
    /// `qr.Encode`'s one error: the text does not fit version 40 at the level.
    #[error("text too long to encode as QR")]
    TooLong,
    /// A condition the Go library panics on; unreachable through [`encode`].
    #[error("qr: internal error: {0}")]
    Internal(&'static str),
}

/// Port of `qr.Code`: the symbol as a bitmap, one bit per module, 1 for black.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    /// Row-major, `stride` bytes per row, most significant bit first.
    pub bitmap: Vec<u8>,
    /// Modules on a side.
    pub size: usize,
    /// Bytes per row of `bitmap` — see the crate notes.
    pub stride: usize,
    /// Image pixels per module; `encode` sets 8.
    pub scale: usize,
}

impl Code {
    /// Port of `Code.Black`: false outside the symbol, which is how the quiet zone is drawn.
    pub fn black(&self, x: isize, y: isize) -> bool {
        let size = self.size as isize;
        0 <= x
            && x < size
            && 0 <= y
            && y < size
            && self.bitmap[y as usize * self.stride + x as usize / 8] & (1 << (7 - (x & 7))) != 0
    }

    /// Port of `Code.PNG`.
    pub fn png(&self) -> Vec<u8> {
        png::encode(self)
    }
}

/// Port of `qr.Encode`.
pub fn encode(text: &str, level: Level) -> Result<Code, QrError> {
    use coding::Encoding;
    let enc = if Encoding::Num(text).check() {
        Encoding::Num(text)
    } else if Encoding::Alpha(text).check() {
        Encoding::Alpha(text)
    } else {
        Encoding::String(text)
    };

    let mut v = coding::MIN_VERSION;
    loop {
        if v > coding::MAX_VERSION {
            return Err(QrError::TooLong);
        }
        if enc.bits(v) <= coding::data_bytes(v, level) * 8 {
            break;
        }
        v += 1;
    }

    let plan = coding::Plan::new(v, level, 0)?;
    let cc = plan.encode(enc)?;
    Ok(Code {
        bitmap: cc.bitmap,
        size: cc.size,
        stride: cc.stride,
        scale: 8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smallest_version_that_holds_the_text() {
        assert_eq!(encode("", Level::H).unwrap().size, 21);
        assert_eq!(encode("x".repeat(7).as_str(), Level::H).unwrap().size, 21);
        assert_eq!(encode("x".repeat(8).as_str(), Level::H).unwrap().size, 25);
        assert_eq!(
            encode("x".repeat(1273).as_str(), Level::H).unwrap().size,
            177
        );
        assert_eq!(
            encode("x".repeat(1274).as_str(), Level::H),
            Err(QrError::TooLong)
        );
    }

    #[test]
    fn the_quiet_zone_is_white() {
        let code = encode("HELLO WORLD", Level::H).unwrap();
        let size = code.size as isize;
        assert!(!code.black(-1, 0) && !code.black(0, -1) && !code.black(size, 0));
        assert!(code.black(0, 0), "the position square's corner");
    }
}
