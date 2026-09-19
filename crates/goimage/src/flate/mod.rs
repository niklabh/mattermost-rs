//! Port of Go's `compress/flate`: the compressor (levels 0, 2-9, default, HuffmanOnly) and the
//! decompressor.

pub mod deflate;
pub mod dict_decoder;
pub(crate) mod huffman_bit_writer;
pub(crate) mod huffman_code;
pub mod inflate;
pub(crate) mod token;

pub use deflate::{FlateError, Writer};
