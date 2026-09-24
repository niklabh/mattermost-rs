//! Ports from Go's `mime` tree: RFC 2047 encoded words ([`word`]), the quoted-printable writer
//! ([`quotedprintable`]) and the multipart writer ([`multipart`]).
//!
//! `mime.TypeByExtension` is deliberately **not** here: `mm_app::mime` already ports it with the
//! host's `globs2` / `mime.types` loading, and go-mail's embed writer takes it as a parameter
//! (see [`crate::msg::WriteEnv`]) rather than this crate carrying a second copy.

pub mod multipart;
pub mod quotedprintable;
pub mod word;

pub use word::WordEncoder;
