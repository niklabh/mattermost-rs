//! The encoder (encoding/gob/encoder.go, encode.go).

use crate::error::{Error, Result};
use crate::types::{Describer, SentTypes, TypeTable};
use crate::wire::{frame, put_bytes, put_float, put_int, put_uint};

/// A Rust type with a fixed gob wire type.
///
/// Implemented by `#[derive(Gob)]` for structs and by this crate for the standard types. The
/// decoder half, [`GobType::compatible`], is Go's `compatibleType` (decode.go:1017): whether a
/// value the remote side sent as `wire` may be stored in this type.
pub trait GobType {
    /// The wire type id for this type on the current stream, defining it if needed.
    fn describe(d: &mut Describer<'_>) -> Result<i64>;

    /// Whether a remote value of type `wire` can be decoded into this type.
    fn compatible(types: &TypeTable, wire: i64) -> bool;

    #[doc(hidden)]
    /// `[]Self`. Overridden by `u8`, whose slices are the built-in `[]byte`.
    fn describe_slice(d: &mut Describer<'_>) -> Result<i64>
    where
        Self: Sized,
    {
        let elem = Self::describe(d)?;
        Ok(d.slice(std::any::type_name::<Vec<Self>>(), elem))
    }

    #[doc(hidden)]
    fn compatible_slice(types: &TypeTable, wire: i64) -> bool
    where
        Self: Sized,
    {
        match types.get(wire).map(|t| &**t) {
            Some(crate::WireType::Slice { elem, .. }) => Self::compatible(types, *elem),
            _ => false,
        }
    }
}

/// A value that can be written to a gob stream.
pub trait Encode {
    /// The wire type id of this value. For a [`GobType`] this is `Self::describe`; a dynamic
    /// value describes itself from its contents.
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64>;

    /// Whether Go would **omit** this value as a struct field (encode.go, `encOpFor`): a zero
    /// number or `false`, an empty string, slice or byte slice, a nil map, pointer or interface.
    /// Structs and arrays are never omitted, even when zero.
    fn is_zero(&self) -> bool;

    /// Whether this value is framed as a struct at top level and inside an interface. Everything
    /// else, including a type with a marshaler, is a "singleton": a zero delta, then the value.
    fn frames_as_struct(&self) -> bool {
        false
    }

    /// Write the value itself — no field number, no framing.
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()>;

    #[doc(hidden)]
    /// `[]Self`: a count, then every element, zero or not.
    fn encode_slice(items: &[Self], e: &mut ValueEncoder<'_>) -> Result<()>
    where
        Self: Sized,
    {
        e.uint(items.len() as u64);
        for item in items {
            item.encode(e)?;
        }
        Ok(())
    }
}

/// Writes one value's bytes. Handed to [`Encode::encode`].
pub struct ValueEncoder<'a> {
    pub(crate) out: &'a mut Vec<u8>,
    pub(crate) sent: &'a mut SentTypes,
    pub(crate) defs: &'a mut Vec<u8>,
}

impl<'a> ValueEncoder<'a> {
    pub fn uint(&mut self, u: u64) {
        put_uint(self.out, u);
    }

    pub fn int(&mut self, i: i64) {
        put_int(self.out, i);
    }

    pub fn bool(&mut self, b: bool) {
        put_uint(self.out, u64::from(b));
    }

    pub fn float(&mut self, f: f64) {
        put_float(self.out, f);
    }

    /// A complex number: real part, then imaginary.
    pub fn complex(&mut self, re: f64, im: f64) {
        put_float(self.out, re);
        put_float(self.out, im);
    }

    /// A string, `[]byte`, or a marshaler's output: a count, then the bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        put_bytes(self.out, b);
    }

    /// Define or look up types while encoding (needed for interface values).
    pub fn describer(&mut self) -> Describer<'_> {
        Describer {
            sent: self.sent,
            defs: self.defs,
        }
    }

    /// Start a struct: fields in increasing order, then [`StructEncoder::end`].
    pub fn structure(&mut self) -> StructEncoder<'_, 'a> {
        StructEncoder { e: self, last: -1 }
    }

    /// An interface value (encode.go, `encodeInterface`): the registered concrete type name,
    /// the concrete type id, then the value delimited by its byte count. `None` is a nil
    /// interface, sent as an empty name.
    pub fn interface(&mut self, value: Option<(&str, &dyn Encode)>) -> Result<()> {
        let Some((name, value)) = value else {
            put_uint(self.out, 0);
            return Ok(());
        };
        if name.is_empty() {
            return Err(Error::Encode(
                "interface value needs a registered type name".into(),
            ));
        }
        put_bytes(self.out, name.as_bytes());
        let id = value.describe_value(&mut self.describer())?;
        put_int(self.out, id);
        let mut inner = Vec::new();
        {
            let mut e = ValueEncoder {
                out: &mut inner,
                sent: self.sent,
                defs: self.defs,
            };
            if !value.frames_as_struct() {
                e.uint(0);
            }
            value.encode(&mut e)?;
        }
        put_bytes(self.out, &inner);
        Ok(())
    }
}

/// Writes a struct's fields as `(delta, value)` pairs. Created by [`ValueEncoder::structure`].
pub struct StructEncoder<'e, 'a> {
    e: &'e mut ValueEncoder<'a>,
    last: i64,
}

impl StructEncoder<'_, '_> {
    /// Write field number `index` unless Go would omit it ([`Encode::is_zero`]). Indices must
    /// increase and count only the fields that are sent at all (Go's exported fields).
    pub fn field<T: Encode + ?Sized>(&mut self, index: u32, value: &T) -> Result<()> {
        if value.is_zero() {
            return Ok(());
        }
        let index = i64::from(index);
        if index <= self.last {
            return Err(Error::Encode(format!(
                "struct field {index} written out of order"
            )));
        }
        put_uint(self.e.out, (index - self.last) as u64);
        self.last = index;
        value.encode(self.e)
    }

    /// Write the terminating zero delta.
    pub fn end(self) {
        put_uint(self.e.out, 0);
    }
}

/// Encodes values onto one gob stream.
///
/// Like Go's `gob.Encoder`, it is stateful: a type is defined once per stream, so keep one
/// `Encoder` per connection (per direction) and never share its output with another stream.
#[derive(Debug, Default)]
pub struct Encoder {
    sent: SentTypes,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode `value`, returning the bytes to write: any new type definitions, each as its own
    /// message, then the value's message.
    pub fn encode<T: Encode + ?Sized>(&mut self, value: &T) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.encode_into(value, &mut out)?;
        Ok(out)
    }

    /// Like [`Encoder::encode`], appending to `out`. On error nothing is appended and the
    /// stream's type state is unchanged.
    pub fn encode_into<T: Encode + ?Sized>(&mut self, value: &T, out: &mut Vec<u8>) -> Result<()> {
        let mark = self.sent.begin();
        let mut defs = Vec::new();
        let mut body = Vec::new();
        let result = (|| {
            let id = value.describe_value(&mut Describer {
                sent: &mut self.sent,
                defs: &mut defs,
            })?;
            put_int(&mut body, id);
            let mut e = ValueEncoder {
                out: &mut body,
                sent: &mut self.sent,
                defs: &mut defs,
            };
            if !value.frames_as_struct() {
                e.uint(0);
            }
            value.encode(&mut e)
        })();
        if let Err(e) = result {
            self.sent.rollback(mark);
            return Err(e);
        }
        out.extend_from_slice(&defs);
        frame(out, &body);
        Ok(())
    }
}
