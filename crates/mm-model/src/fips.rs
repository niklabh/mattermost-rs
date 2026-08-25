//! Port of `model/fips.go` and `model/fips_default.go` — one constant behind a build tag.
//!
//! Go has two files with mutually exclusive `//go:build` lines: `requirefips` sets
//! `FIPSEnabled = true`, its absence sets `false`. The Rust equivalent is a Cargo feature, and
//! **the feature does not exist yet** — nothing in this workspace consumes the constant, and
//! declaring a `requirefips` feature that no dependency honours would claim a guarantee the build
//! does not make. So this is the default half only, with the switch documented rather than faked.
//!
//! When a FIPS build is actually wanted: add `requirefips = []` to `mm-model`'s `[features]` and
//! change this to `cfg!(feature = "requirefips")`.

/// Port of `model.FIPSEnabled` (fips_default.go:5) — the non-FIPS build.
pub const FIPS_ENABLED: bool = false;
