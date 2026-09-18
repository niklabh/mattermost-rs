//! `GobType`, `Encode` and `Decode` for the standard types.
//!
//! Go ↔ Rust correspondence:
//!
//! | Go | Rust |
//! |---|---|
//! | `bool` | `bool` |
//! | `int`, `int8`…`int64` | `i8`…`i64`, `isize` (range-checked on decode) |
//! | `uint`, `uint8`…`uint64`, `uintptr` | `u8`…`u64`, `usize` |
//! | `float32`, `float64` | `f32`, `f64` |
//! | `complex128` | `(f64, f64)` is *not* used; see [`Complex`] |
//! | `string` | `String` (`&str` to encode) |
//! | `[]byte` | `Vec<u8>` |
//! | `[]T` | `Vec<T>` |
//! | `[N]T` | `[T; N]` |
//! | `map[K]V` | `HashMap<K, V>`, `BTreeMap<K, V>` |
//! | `*T` | `Option<T>` (or `Box<T>` when never nil) |
//! | `any`, `error`, other interfaces | `Option<Interface>` |

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

use crate::decode::{Decode, ValueDecoder};
use crate::encode::{Encode, GobType, ValueEncoder};
use crate::error::{Error, Result};
use crate::types::{Describer, TypeTable, WireType};
use crate::wire::ids;

macro_rules! simple_type {
    ($t:ty, $id:expr) => {
        impl GobType for $t {
            fn describe(_: &mut Describer<'_>) -> Result<i64> {
                Ok($id)
            }
            fn compatible(_: &TypeTable, wire: i64) -> bool {
                wire == $id
            }
        }
    };
}

impl GobType for bool {
    fn describe(_: &mut Describer<'_>) -> Result<i64> {
        Ok(ids::BOOL)
    }
    fn compatible(_: &TypeTable, wire: i64) -> bool {
        wire == ids::BOOL
    }
}

impl Encode for bool {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    fn is_zero(&self) -> bool {
        !*self
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.bool(*self);
        Ok(())
    }
}

impl Decode for bool {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::BOOL {
            return Err(d.mismatch::<Self>(wire));
        }
        *self = d.read_uint()? != 0;
        Ok(())
    }
}

macro_rules! signed {
    ($($t:ty),*) => {$(
        simple_type!($t, ids::INT);
        impl Encode for $t {
            fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> { Self::describe(d) }
            fn is_zero(&self) -> bool { *self == 0 }
            fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> { e.int(*self as i64); Ok(()) }
        }
        impl Decode for $t {
            fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
                if wire != ids::INT {
                    return Err(d.mismatch::<Self>(wire));
                }
                let v = d.read_int()?;
                *self = <$t>::try_from(v).map_err(|_| Error::Overflow(stringify!($t)))?;
                Ok(())
            }
        }
    )*};
}

macro_rules! unsigned {
    ($($t:ty),*) => {$(
        simple_type!($t, ids::UINT);
        impl Encode for $t {
            fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> { Self::describe(d) }
            fn is_zero(&self) -> bool { *self == 0 }
            fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> { e.uint(*self as u64); Ok(()) }
        }
        impl Decode for $t {
            fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
                if wire != ids::UINT {
                    return Err(d.mismatch::<Self>(wire));
                }
                let v = d.read_uint()?;
                *self = <$t>::try_from(v).map_err(|_| Error::Overflow(stringify!($t)))?;
                Ok(())
            }
        }
    )*};
}

signed!(i8, i16, i32, i64, isize);
unsigned!(u16, u32, u64, usize);

// `u8` separately: its slices are Go's built-in `[]byte`.

impl GobType for u8 {
    fn describe(_: &mut Describer<'_>) -> Result<i64> {
        Ok(ids::UINT)
    }
    fn compatible(_: &TypeTable, wire: i64) -> bool {
        wire == ids::UINT
    }
    fn describe_slice(_: &mut Describer<'_>) -> Result<i64> {
        Ok(ids::BYTES)
    }
    fn compatible_slice(_: &TypeTable, wire: i64) -> bool {
        wire == ids::BYTES
    }
}

impl Encode for u8 {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    fn is_zero(&self) -> bool {
        *self == 0
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.uint(u64::from(*self));
        Ok(())
    }
    fn encode_slice(items: &[Self], e: &mut ValueEncoder<'_>) -> Result<()> {
        e.bytes(items);
        Ok(())
    }
}

impl Decode for u8 {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::UINT {
            return Err(d.mismatch::<Self>(wire));
        }
        *self = u8::try_from(d.read_uint()?).map_err(|_| Error::Overflow("u8"))?;
        Ok(())
    }
    fn decode_vec(vec: &mut Vec<Self>, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::BYTES {
            return Err(d.mismatch::<Vec<u8>>(wire));
        }
        let b = d.read_bytes()?;
        vec.clear();
        vec.extend_from_slice(b);
        Ok(())
    }
}

simple_type!(f64, ids::FLOAT);
simple_type!(f32, ids::FLOAT);

impl Encode for f64 {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    /// `f != 0` in Go, so `-0.0` is omitted too.
    fn is_zero(&self) -> bool {
        *self == 0.0
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.float(*self);
        Ok(())
    }
}

impl Decode for f64 {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::FLOAT {
            return Err(d.mismatch::<Self>(wire));
        }
        *self = d.read_float()?;
        Ok(())
    }
}

impl Encode for f32 {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    fn is_zero(&self) -> bool {
        *self == 0.0
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.float(f64::from(*self));
        Ok(())
    }
}

/// decode.go, `float32FromBits`: a finite value beyond `MaxFloat32` overflows; infinities and
/// underflow do not.
fn to_f32(v: f64) -> Result<f32> {
    let av = v.abs();
    if f64::from(f32::MAX) < av && av <= f64::MAX {
        return Err(Error::Overflow("f32"));
    }
    Ok(v as f32)
}

impl Decode for f32 {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::FLOAT {
            return Err(d.mismatch::<Self>(wire));
        }
        *self = to_f32(d.read_float()?)?;
        Ok(())
    }
}

/// Go's `complex128`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

simple_type!(Complex, ids::COMPLEX);

impl Encode for Complex {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    fn is_zero(&self) -> bool {
        self.re == 0.0 && self.im == 0.0
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.complex(self.re, self.im);
        Ok(())
    }
}

impl Decode for Complex {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::COMPLEX {
            return Err(d.mismatch::<Self>(wire));
        }
        self.re = d.read_float()?;
        self.im = d.read_float()?;
        Ok(())
    }
}

simple_type!(String, ids::STRING);
simple_type!(str, ids::STRING);

impl Encode for String {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    fn is_zero(&self) -> bool {
        self.is_empty()
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.bytes(self.as_bytes());
        Ok(())
    }
}

impl Encode for str {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    fn is_zero(&self) -> bool {
        self.is_empty()
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.bytes(self.as_bytes());
        Ok(())
    }
}

impl Decode for String {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::STRING {
            return Err(d.mismatch::<Self>(wire));
        }
        *self = d.read_string()?;
        Ok(())
    }
}

// ─── containers ────────────────────────────────────────────────────────────────────────────

impl<T: GobType> GobType for Vec<T> {
    fn describe(d: &mut Describer<'_>) -> Result<i64> {
        T::describe_slice(d)
    }
    fn compatible(types: &TypeTable, wire: i64) -> bool {
        T::compatible_slice(types, wire)
    }
}

impl<T: GobType + Encode> Encode for Vec<T> {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    /// Go omits an empty slice whether or not it is nil.
    fn is_zero(&self) -> bool {
        self.is_empty()
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        T::encode_slice(self, e)
    }
}

impl<T: Decode + Default> Decode for Vec<T> {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if !Self::compatible(d.types(), wire) {
            return Err(d.mismatch::<Self>(wire));
        }
        T::decode_vec(self, d, wire)
    }
}

impl<T: GobType> GobType for [T] {
    fn describe(d: &mut Describer<'_>) -> Result<i64> {
        T::describe_slice(d)
    }
    fn compatible(types: &TypeTable, wire: i64) -> bool {
        T::compatible_slice(types, wire)
    }
}

impl<T: GobType + Encode> Encode for [T] {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    fn is_zero(&self) -> bool {
        self.is_empty()
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        T::encode_slice(self, e)
    }
}

impl<T: GobType, const N: usize> GobType for [T; N] {
    fn describe(d: &mut Describer<'_>) -> Result<i64> {
        let elem = T::describe(d)?;
        Ok(d.array(std::any::type_name::<Self>(), elem, N))
    }
    fn compatible(types: &TypeTable, wire: i64) -> bool {
        match types.get(wire).map(|t| &**t) {
            Some(WireType::Array { elem, len, .. }) => {
                *len == N as i64 && T::compatible(types, *elem)
            }
            _ => false,
        }
    }
}

impl<T: GobType + Encode, const N: usize> Encode for [T; N] {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        Self::describe(d)
    }
    /// Arrays are always sent (encode.go: the array op has no zero check).
    fn is_zero(&self) -> bool {
        false
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        e.uint(N as u64);
        for item in self {
            item.encode(e)?;
        }
        Ok(())
    }
}

impl<T: Decode, const N: usize> Decode for [T; N] {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if !Self::compatible(d.types(), wire) {
            return Err(d.mismatch::<Self>(wire));
        }
        let elem = match d.types().get(wire).map(|t| &**t) {
            Some(WireType::Array { elem, .. }) => *elem,
            _ => return Err(d.mismatch::<Self>(wire)),
        };
        if d.read_uint()? != N as u64 {
            return Err(Error::Corrupt("length mismatch in decodeArray".into()));
        }
        for item in self.iter_mut() {
            d.expect_element(N as u64)?;
            item.decode_into(d, elem)?;
        }
        Ok(())
    }
}

macro_rules! map_impl {
    ($map:ident, $($bound:path),*) => {
        impl<K: GobType, V: GobType> GobType for $map<K, V> {
            fn describe(d: &mut Describer<'_>) -> Result<i64> {
                let k = K::describe(d)?;
                let v = V::describe(d)?;
                Ok(d.map(std::any::type_name::<Self>(), k, v))
            }
            fn compatible(types: &TypeTable, wire: i64) -> bool {
                match types.get(wire).map(|t| &**t) {
                    Some(WireType::Map { key, elem, .. }) => K::compatible(types, *key) && V::compatible(types, *elem),
                    _ => false,
                }
            }
        }

        impl<K: GobType + Encode, V: GobType + Encode> Encode for $map<K, V> {
            fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
                Self::describe(d)
            }
            /// Go omits only a **nil** map and sends an empty non-nil one. A Rust map cannot be
            /// nil, so an empty map is treated as the nil a zero-valued Go struct holds: an empty
            /// non-nil Go map decodes to an empty Rust map and re-encodes as nil. Code that
            /// must tell the two apart has to keep the distinction itself.
            fn is_zero(&self) -> bool {
                self.is_empty()
            }
            fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
                e.uint(self.len() as u64);
                for (k, v) in self {
                    k.encode(e)?;
                    v.encode(e)?;
                }
                Ok(())
            }
        }

        impl<K: Decode + Default $(+ $bound)*, V: Decode + Default> Decode for $map<K, V> {
            /// decode.go, `decodeMap`: each key and element decodes into a fresh zero value and
            /// is then stored, replacing any existing entry — entries are not merged.
            fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
                if !Self::compatible(d.types(), wire) {
                    return Err(d.mismatch::<Self>(wire));
                }
                let (key, elem) = match d.types().get(wire).map(|t| &**t) {
                    Some(WireType::Map { key, elem, .. }) => (*key, *elem),
                    _ => return Err(d.mismatch::<Self>(wire)),
                };
                let n = d.read_uint()?;
                for _ in 0..n {
                    d.expect_element(n)?;
                    let mut k = K::default();
                    k.decode_into(d, key)?;
                    let mut v = V::default();
                    v.decode_into(d, elem)?;
                    self.insert(k, v);
                }
                Ok(())
            }
        }
    };
}

map_impl!(HashMap, Hash, Eq);
map_impl!(BTreeMap, Ord);

/// `*T`. (`Option<Interface>` is an interface, not a pointer: see `value.rs`.)
impl<T: GobType> GobType for Option<T> {
    fn describe(d: &mut Describer<'_>) -> Result<i64> {
        T::describe(d)
    }
    fn compatible(types: &TypeTable, wire: i64) -> bool {
        T::compatible(types, wire)
    }
}

impl<T: GobType + Encode> Encode for Option<T> {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        T::describe(d)
    }
    /// A nil pointer is omitted, and so is a pointer to a value Go would omit.
    fn is_zero(&self) -> bool {
        match self {
            None => true,
            Some(v) => v.is_zero(),
        }
    }
    fn frames_as_struct(&self) -> bool {
        self.as_ref().is_some_and(Encode::frames_as_struct)
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        match self {
            Some(v) => v.encode(e),
            None => Err(Error::Encode(format!(
                "cannot encode nil pointer of type {}",
                std::any::type_name::<T>()
            ))),
        }
    }
}

impl<T: Decode + Default> Decode for Option<T> {
    /// decode.go, `decAlloc`: allocate a nil pointer, then decode through it — into the
    /// existing pointee when there is one.
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        self.get_or_insert_with(T::default).decode_into(d, wire)
    }
}

impl<T: GobType + ?Sized> GobType for Box<T> {
    fn describe(d: &mut Describer<'_>) -> Result<i64> {
        T::describe(d)
    }
    fn compatible(types: &TypeTable, wire: i64) -> bool {
        T::compatible(types, wire)
    }
}

impl<T: Encode + ?Sized> Encode for Box<T> {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        (**self).describe_value(d)
    }
    fn is_zero(&self) -> bool {
        (**self).is_zero()
    }
    fn frames_as_struct(&self) -> bool {
        (**self).frames_as_struct()
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        (**self).encode(e)
    }
}

impl<T: Decode + ?Sized> Decode for Box<T> {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        (**self).decode_into(d, wire)
    }
}

impl<T: GobType + ?Sized> GobType for &T {
    fn describe(d: &mut Describer<'_>) -> Result<i64> {
        T::describe(d)
    }
    fn compatible(types: &TypeTable, wire: i64) -> bool {
        T::compatible(types, wire)
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        (**self).describe_value(d)
    }
    fn is_zero(&self) -> bool {
        (**self).is_zero()
    }
    fn frames_as_struct(&self) -> bool {
        (**self).frames_as_struct()
    }
    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        (**self).encode(e)
    }
}
