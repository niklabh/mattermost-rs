//! Port of Go's `image/jpeg` (go1.26.4): the encoder (`writer.go`, the forward DCT) and the
//! decoder (`reader.go`, `scan.go`, `huffman.go`, the inverse DCT).

pub mod fdct;
mod huffman;
mod idct;
pub mod reader;
mod scan;
pub mod writer;

pub use reader::{Config, ConfigModel, JpegError, decode, decode_config};
pub use writer::{DEFAULT_QUALITY, JpegEncodeError, Options, encode};
