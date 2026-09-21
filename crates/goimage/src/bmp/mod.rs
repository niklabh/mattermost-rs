//! Port of `golang.org/x/image/bmp`'s decoder. The encoder is out of scope: Mattermost registers
//! this package in `channels/app/imaging/decode.go` to *read* uploads and never writes a BMP.

pub mod reader;

pub use reader::{Config, ConfigModel, Error, decode, decode_config};
