//! Port of Go's `compress/zlib`.

pub mod writer;

pub use writer::{Writer, ZlibError};
