//! Port of Go's `image/gif` decoder and the `compress/lzw` reader it is built on.
//!
//! The encoder (`image/gif/writer.go`, `compress/lzw/writer.go` and the Plan 9 quantiser) is not
//! ported.

pub mod lzw;
pub mod reader;

pub use reader::{
    Config, DISPOSAL_BACKGROUND, DISPOSAL_NONE, DISPOSAL_PREVIOUS, Error, Gif, decode, decode_all,
    decode_config,
};
