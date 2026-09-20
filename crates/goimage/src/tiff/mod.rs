//! Port of `golang.org/x/image/tiff`'s decoder, and of `golang.org/x/image/tiff/lzw` beneath it.
//!
//! The encoder (`tiff/writer.go`) is not ported: Mattermost decodes TIFF uploads and re-encodes
//! them as PNG or JPEG, so no code path in the server writes one.
//!
//! Group 3 and Group 4 fax compression are **not** ported either — `tiff.Decode` hands those to
//! `golang.org/x/image/ccitt`, and [`reader::Error::CcittNotPorted`] names that gap so a caller
//! forwards exactly those files and no others.

pub mod lzw;
pub mod reader;

pub use reader::{Config, ConfigModel, Error, decode, decode_config};
