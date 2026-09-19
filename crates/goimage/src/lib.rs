//! Byte-exact Rust ports of the Go code that decides the bytes of a Mattermost image upload's
//! derived files: Go's `image/png` and `image/jpeg` (both directions), `compress/flate` and
//! `compress/zlib`, the `image/color` arithmetic, `github.com/boxes-ltd/imaging`'s resampling and
//! transforms, and `github.com/bep/imagemeta`'s EXIF orientation walk.
//!
//! # Why a port and not a crate from crates.io
//!
//! Mattermost stores what these produce: a thumbnail, a preview, a 16×16 JPEG mini preview inside
//! the FileInfo row, a 128×128 profile picture. A second implementation of "PNG at best
//! compression" or "Lanczos to 120×100" produces *a* valid file, not *the* file — a different
//! deflate match, a different row filter, a different rounding of one weight — and a strangler
//! proxy that must be byte-compatible with the Go server it replaces cannot store a different
//! file. Every module here is therefore a line-by-line port, verified against the oracle in
//! `reference/dump/behaviour_imaging*.go` (fixtures `behaviour_imaging_<stage>.json`).
//!
//! # Architecture-dependent arithmetic
//!
//! Go on arm64 fuses `a + x*y` into a single FMADD; on amd64 (at the default `GOAMD64=v1`) it does
//! not. The resampler's output therefore depends on the architecture the Go server runs on, and
//! [`fma`] mirrors that per target: the oracle was generated on arm64.
//!
//! This crate contains no Mattermost code. The Mattermost call sequences (`GenerateThumbnail`,
//! `postprocessImage`, `AdjustImage`, …) are in `mm-app`.

pub mod bufio;
pub mod flate;
pub mod fma;
pub mod gomath;
pub mod hash;
pub mod image;
pub mod imaging;
pub mod jpeg;
pub mod png;
pub mod sink;
pub mod zlib;

#[cfg(test)]
mod testsupport;
