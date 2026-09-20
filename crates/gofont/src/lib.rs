//! Byte-exact Rust port of the text rasterisation Mattermost's generated initials avatar depends
//! on: `github.com/golang/freetype`'s `truetype` glyph loader and `raster` scan converter,
//! `golang.org/x/image/font`'s `Drawer`, `golang.org/x/image/math/fixed`, and the two `image/draw`
//! paths the drawing uses.
//!
//! # Why a port and not a crate from crates.io
//!
//! `users.createProfileImage` stores a 128×128 PNG, and the client caches it for a day under an
//! etag the server mints. Every pixel of it comes out of freetype-go's anti-aliasing, so a
//! near-match is a *different file* — see [D-204]. A Rust font crate produces a valid avatar, not
//! Go's avatar.
//!
//! This crate contains no Mattermost code. The call sequence (`createProfileImage`: the FNV-1a
//! colour choice, the 64pt face, the `GlyphBounds` centring) is in `mm-app`.
