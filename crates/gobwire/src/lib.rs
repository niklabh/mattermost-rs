//! Go's [`encoding/gob`](https://pkg.go.dev/encoding/gob) wire format.
//!
//! `gobwire` reads and writes the byte streams Go's `gob.Encoder` and `gob.Decoder` exchange —
//! the encoding under Go's `net/rpc` and HashiCorp's net/rpc plugin protocol — so a Rust program
//! can talk to a Go one without either side changing.
//!
//! ```
//! use gobwire::{Decoder, Encoder, Gob, Progress, parse_length_prefix};
//!
//! #[derive(Gob, Debug, Default, PartialEq)]
//! struct Point {
//!     #[gob(name = "X")]
//!     x: i64,
//!     #[gob(name = "Y")]
//!     y: i64,
//! }
//!
//! let bytes = Encoder::new().encode(&Point { x: 22, y: 33 }).unwrap();
//!
//! let mut decoder = Decoder::new();
//! let mut rest = &bytes[..];
//! let point: Point = loop {
//!     let (width, len) = parse_length_prefix(rest).unwrap().unwrap();
//!     let body = &rest[width..width + len];
//!     rest = &rest[width + len..];
//!     if decoder.push_message(body).unwrap() == Progress::Ready {
//!         break decoder.decode().unwrap();
//!     }
//! };
//! assert_eq!(point, Point { x: 22, y: 33 });
//! ```
//!
//! # What matches Go, and what cannot
//!
//! * **Field matching is by Go field name**, never by position or JSON tag. Name every field
//!   with `#[gob(name = "...")]` unless the snake_case→PascalCase default is exactly right.
//! * **Zero values are omitted** from structs exactly where Go omits them ([`Encode::is_zero`]).
//! * **Decoding merges** into the destination ([`Decode`]): absent fields keep their values.
//! * **A value may span several messages** ([`Decoder`]).
//! * **Divergences**, each forced by Rust's types: an empty map is sent as nil; a slice merges
//!   into existing elements only when the `Vec` is at least as long as the incoming slice (Go
//!   uses capacity); strings must be UTF-8; nesting deeper than [`MAX_DEPTH`] is refused; a nil
//!   interface being *skipped* is parsed correctly where Go's own skip misreads it.

// Lets `#[derive(Gob)]`'s `::gobwire::` paths resolve inside this crate's own tests.
extern crate self as gobwire;

mod decode;
mod encode;
mod error;
mod impls;
mod io;
mod marshaled;
mod time;
mod types;
mod value;
mod wire;

pub use decode::{
    Decode, Decoder, FieldRef, MAX_DEPTH, Progress, StructDecoder, StructPlan, ValueDecoder,
};
pub use encode::{Encode, Encoder, GobType, StructEncoder, ValueEncoder};
pub use error::{Error, Result};
pub use impls::Complex;
pub use io::{StreamDecoder, StreamEncoder, read_message};
pub use marshaled::{BinaryBytes, GobBytes};
pub use time::{GoTime, UNIX_TO_INTERNAL, Zone};
pub use types::{Describer, MarshalKind, TypeTable, WireField, WireType};
pub use value::{Dynamic, DynamicRef, Interface, StructType, Type, Value, names};
pub use wire::{MAX_MESSAGE_LEN, frame, ids, parse_length_prefix, parse_uint};

#[cfg(feature = "derive")]
pub use gobwire_derive::Gob;
