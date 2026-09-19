//! Port of Go's `image/jpeg`.

pub mod fdct;
pub mod writer;

pub use writer::{DEFAULT_QUALITY, JpegEncodeError, Options, encode};
