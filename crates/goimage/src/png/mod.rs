//! Port of Go's `image/png`.

pub mod paeth;
pub mod reader;
pub mod writer;

pub use reader::{Config, ConfigModel, decode, decode_config};
pub use writer::{CompressionLevel, PngEncodeError, encode};
