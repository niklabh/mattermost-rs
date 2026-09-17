//! Opaque values of Go types that marshal themselves.
//!
//! Only `GobEncoder` and `BinaryMarshaler` count: gob's `TextMarshaler` support is commented out
//! (type.go:85-89, "incompatibility with older encodings for net.IP"), so a Go type that only
//! implements `MarshalText` is sent as its underlying kind, and Go rejects a text-marshaled value
//! it receives. [`MarshalKind::Text`](crate::MarshalKind::Text) exists only to decode the wire
//! type definition.

use crate::decode::{Decode, ValueDecoder};
use crate::encode::{Encode, GobType, ValueEncoder};
use crate::error::Result;
use crate::types::{Describer, MarshalKind, TypeTable, WireType};

macro_rules! marshaled {
    ($(#[$doc:meta])* $name:ident, $kind:expr, $key:literal) => {
        $(#[$doc])*
        ///
        /// The bytes are kept as the Go side produced them. Zero-ness cannot be known without the
        /// Go type, so empty bytes count as the zero value a struct field omits — true of a zero
        /// `url.URL`, but check any other type before relying on it.
        #[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
        pub struct $name(pub Vec<u8>);

        impl GobType for $name {
            fn describe(d: &mut Describer<'_>) -> Result<i64> {
                Ok(d.marshaler($key, "", $kind))
            }
            fn compatible(types: &TypeTable, wire: i64) -> bool {
                matches!(types.get(wire).map(|t| &**t), Some(WireType::Marshaler { kind, .. }) if *kind == $kind)
            }
        }

        impl Encode for $name {
            fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
                Self::describe(d)
            }
            fn is_zero(&self) -> bool {
                self.0.is_empty()
            }
            fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
                e.bytes(&self.0);
                Ok(())
            }
        }

        impl Decode for $name {
            fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
                if !Self::compatible(d.types(), wire) {
                    return Err(d.mismatch::<Self>(wire));
                }
                self.0 = d.read_bytes()?.to_vec();
                Ok(())
            }
        }
    };
}

marshaled!(
    /// A value of a Go type implementing `gob.GobEncoder` (such as `*big.Int`).
    GobBytes, MarshalKind::Gob, "gobwire::GobBytes"
);
marshaled!(
    /// A value of a Go type implementing `encoding.BinaryMarshaler` (such as `url.URL`).
    BinaryBytes, MarshalKind::Binary, "gobwire::BinaryBytes"
);
