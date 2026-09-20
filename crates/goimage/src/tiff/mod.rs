//! Port of `golang.org/x/image/tiff`'s decoder, and of `golang.org/x/image/tiff/lzw` beneath it.
//!
//! The encoder (`tiff/writer.go`) is not ported: Mattermost decodes TIFF uploads and re-encodes
//! them as PNG or JPEG, so no code path in the server writes one.
//!
//! Group 3 and Group 4 fax compression go through [`ccitt`], a port of
//! `golang.org/x/image/ccitt`'s reader — `tiff.Decode` calls `ccitt.NewReader` for compression
//! values 3 and 4.

pub mod ccitt;
mod ccitt_tables;
pub mod lzw;
pub mod reader;

pub use reader::{Config, ConfigModel, Error, decode, decode_config};
