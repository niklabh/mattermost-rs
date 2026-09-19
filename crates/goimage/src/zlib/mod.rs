//! Port of Go's `compress/zlib`.

pub mod reader;
pub mod writer;

pub use writer::{Writer, ZlibError};
