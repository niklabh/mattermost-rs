//! Port of Go's `compress/flate`.

pub mod deflate;
pub(crate) mod huffman_bit_writer;
pub(crate) mod huffman_code;
pub(crate) mod token;

pub use deflate::{FlateError, Writer};
