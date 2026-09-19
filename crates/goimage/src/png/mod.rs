//! Port of Go's `image/png`.

pub mod writer;

pub use writer::{CompressionLevel, PngEncodeError, encode};
