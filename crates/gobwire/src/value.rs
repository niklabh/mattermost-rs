//! Dynamic values: what an interface holds when the concrete Go type is not known locally.

use std::sync::Arc;

use crate::decode::{Decode, Decoder, ValueDecoder};
use crate::encode::{Encode, Encoder, GobType, ValueEncoder};
use crate::error::{Error, Result};
use crate::types::{Describer, MarshalKind, TypeTable};
use crate::wire::ids;

/// The shape of a dynamic value, built from the remote side's type definitions.
///
/// A recursive struct refers back to itself with [`Type::Ref`]. Recursion that never passes
/// through a struct (Go's `type L []L`) has no dynamic form and fails with
/// [`Error::Unsupported`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Bool,
    Int,
    Uint,
    Float,
    Complex,
    Bytes,
    String,
    Interface,
    Slice(Arc<Type>),
    Array(Arc<Type>, usize),
    Map(Arc<Type>, Arc<Type>),
    Struct(Arc<StructType>),
    Marshaler(MarshalKind, String),
    /// The struct `n` enclosing struct levels out: `Ref(0)` is the innermost struct containing
    /// this type. A value of it is a [`Value::Struct`] of that struct type.
    Ref(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructType {
    pub name: String,
    pub fields: Vec<(String, Type)>,
}

impl Type {
    pub fn is_struct(&self) -> bool {
        matches!(self, Type::Struct(_))
    }

    /// `ctx` holds the wire ids of the enclosing dynamic structs, innermost last.
    fn describe(&self, d: &mut Describer<'_>, ctx: &[i64]) -> Result<i64> {
        Ok(match self {
            Type::Bool => ids::BOOL,
            Type::Int => ids::INT,
            Type::Uint => ids::UINT,
            Type::Float => ids::FLOAT,
            Type::Complex => ids::COMPLEX,
            Type::Bytes => ids::BYTES,
            Type::String => ids::STRING,
            Type::Interface => ids::INTERFACE,
            Type::Ref(n) => *ctx
                .len()
                .checked_sub(n + 1)
                .and_then(|i| ctx.get(i))
                .ok_or_else(|| Error::Encode(format!("dangling type reference {n}")))?,
            Type::Slice(elem) => {
                let e = elem.describe(d, ctx)?;
                d.slice(format!("dyn:[]{e}"), e)
            }
            Type::Array(elem, len) => {
                let e = elem.describe(d, ctx)?;
                d.array(format!("dyn:[{len}]{e}"), e, *len)
            }
            Type::Map(k, v) => {
                let (k, v) = (k.describe(d, ctx)?, v.describe(d, ctx)?);
                d.map(format!("dyn:map[{k}]{v}"), k, v)
            }
            Type::Marshaler(kind, name) => d.marshaler(format!("dyn:{kind:?}:{name}"), name, *kind),
            Type::Struct(st) => {
                // The shape identifies the type — plus, when it refers outward, what it refers to.
                let mut key = format!("dyn:{st:?}");
                if st.fields.iter().any(|(_, t)| t.has_ref()) {
                    key.push_str(&format!("@{ctx:?}"));
                }
                d.structure_owned(key, &st.name, |d, id| {
                    let mut inner = ctx.to_vec();
                    inner.push(id);
                    let mut out = Vec::with_capacity(st.fields.len());
                    for (n, t) in &st.fields {
                        out.push((n.clone(), t.describe(d, &inner)?));
                    }
                    Ok(out)
                })?
            }
        })
    }

    fn has_ref(&self) -> bool {
        match self {
            Type::Ref(_) => true,
            Type::Slice(t) | Type::Array(t, _) => t.has_ref(),
            Type::Map(k, v) => k.has_ref() || v.has_ref(),
            Type::Struct(st) => st.fields.iter().any(|(_, t)| t.has_ref()),
            _ => false,
        }
    }

    /// Whether a remote type is exactly this shape, as the decoder must check before decoding
    /// into a value of it.
    fn matches(&self, types: &TypeTable, wire: i64) -> bool {
        use crate::types::WireType as W;
        match self {
            Type::Bool => wire == ids::BOOL,
            Type::Int => wire == ids::INT,
            Type::Uint => wire == ids::UINT,
            Type::Float => wire == ids::FLOAT,
            Type::Complex => wire == ids::COMPLEX,
            Type::Bytes => wire == ids::BYTES,
            Type::String => wire == ids::STRING,
            Type::Interface => wire == ids::INTERFACE,
            _ => match (self, types.get(wire).map(|t| &**t)) {
                (Type::Slice(e), Some(W::Slice { elem, .. })) => e.matches(types, *elem),
                (Type::Array(e, n), Some(W::Array { elem, len, .. })) => {
                    *n as i64 == *len && e.matches(types, *elem)
                }
                (Type::Map(k, v), Some(W::Map { key, elem, .. })) => {
                    k.matches(types, *key) && v.matches(types, *elem)
                }
                (Type::Struct(_) | Type::Ref(_), Some(W::Struct { .. })) => true,
                (Type::Marshaler(k, _), Some(W::Marshaler { kind, .. })) => k == kind,
                _ => false,
            },
        }
    }
}

/// A dynamically typed gob value. Its [`Type`] travels alongside it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Complex(f64, f64),
    Bytes(Vec<u8>),
    String(String),
    /// `None` is a nil interface.
    Interface(Option<Box<Interface>>),
    Slice(Vec<Value>),
    Array(Vec<Value>),
    /// Key/value pairs in wire order (Go's map order is random).
    Map(Vec<(Value, Value)>),
    /// One entry per field of the [`StructType`]; `None` for a field the value did not carry.
    Struct(Vec<Option<Value>>),
    /// The bytes a marshaler produced.
    Marshaled(Vec<u8>),
}

/// A non-nil Go interface value: the name its concrete type was registered under
/// (`gob.Register`), the concrete type, and the value.
///
/// Go names a pointer-to-struct `*pkg.Type` with the **package name only**, not the import path
/// (type.go, `Register`, whose comment calls this a compatibility bug it cannot fix), while a
/// non-pointer named type gets the full import path. Use exactly the name the Go side registered.
#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    pub name: String,
    pub ty: Type,
    pub value: Value,
}

/// Registered names for Go's built-in types (type.go, `registerBasics`).
pub mod names {
    pub const BOOL: &str = "bool";
    pub const INT: &str = "int";
    pub const INT64: &str = "int64";
    pub const UINT: &str = "uint";
    pub const FLOAT64: &str = "float64";
    pub const STRING: &str = "string";
    pub const BYTES: &str = "[]uint8";
    pub const STRINGS: &str = "[]string";
    /// `gob.Register([]any{})` — not a basic, but registered by most RPC users.
    pub const ANY_SLICE: &str = "[]interface {}";
    /// `gob.Register(map[string]any{})`.
    pub const STRING_ANY_MAP: &str = "map[string]interface {}";
}

impl Interface {
    pub fn string(s: impl Into<String>) -> Self {
        Self {
            name: names::STRING.into(),
            ty: Type::String,
            value: Value::String(s.into()),
        }
    }

    pub fn float64(f: f64) -> Self {
        Self {
            name: names::FLOAT64.into(),
            ty: Type::Float,
            value: Value::Float(f),
        }
    }

    pub fn bool(b: bool) -> Self {
        Self {
            name: names::BOOL.into(),
            ty: Type::Bool,
            value: Value::Bool(b),
        }
    }

    pub fn int(i: i64) -> Self {
        Self {
            name: names::INT.into(),
            ty: Type::Int,
            value: Value::Int(i),
        }
    }

    /// Wrap a typed value under the name its Go type was registered with.
    pub fn new<T: Encode + ?Sized>(name: impl Into<String>, value: &T) -> Result<Self> {
        let bytes = Encoder::new().encode(value)?;
        let mut dec = Decoder::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let (width, n) = crate::wire::parse_length_prefix(rest)?.ok_or(Error::UnexpectedEof)?;
            let body = &rest[width..width + n];
            rest = &rest[width + n..];
            if dec.push_message(body)? == crate::Progress::Ready {
                let dynamic: Dynamic = dec.decode()?;
                return Ok(Self {
                    name: name.into(),
                    ty: dynamic.ty,
                    value: dynamic.value,
                });
            }
        }
        Err(Error::UnexpectedEof)
    }

    /// Decode the held value into a typed destination.
    pub fn downcast<T: Decode + Default>(&self) -> Result<T> {
        let dynamic = DynamicRef::new(&self.ty, &self.value);
        let bytes = Encoder::new().encode(&dynamic)?;
        let mut dec = Decoder::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let (width, n) = crate::wire::parse_length_prefix(rest)?.ok_or(Error::UnexpectedEof)?;
            let body = &rest[width..width + n];
            rest = &rest[width + n..];
            if dec.push_message(body)? == crate::Progress::Ready {
                return dec.decode();
            }
        }
        Err(Error::UnexpectedEof)
    }
}

/// An owned dynamic value of any type. Decodes from any remote type (it is not an interface:
/// the remote side sent a concrete value), and re-encodes as the same wire shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Dynamic {
    pub ty: Type,
    pub value: Value,
}

impl Default for Dynamic {
    fn default() -> Self {
        Self {
            ty: Type::Interface,
            value: Value::Interface(None),
        }
    }
}

impl GobType for Dynamic {
    fn describe(_: &mut Describer<'_>) -> Result<i64> {
        Err(Error::Encode(
            "a Dynamic value describes itself; encode the value, not the type".into(),
        ))
    }

    fn compatible(types: &TypeTable, wire: i64) -> bool {
        types.is_known(wire)
    }
}

impl Decode for Dynamic {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        let ty = d.dynamic_type(wire)?;
        let value = d.read_dynamic(&ty, wire)?;
        *self = Dynamic { ty, value };
        Ok(())
    }
}

impl Encode for Dynamic {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        self.ty.describe(d, &[])
    }

    fn is_zero(&self) -> bool {
        DynamicRef::new(&self.ty, &self.value).is_zero()
    }

    fn frames_as_struct(&self) -> bool {
        self.ty.is_struct()
    }

    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        DynamicRef::new(&self.ty, &self.value).encode(e)
    }
}

/// A borrowed dynamic value, for encoding.
#[derive(Debug, Clone)]
pub struct DynamicRef<'a> {
    pub ty: &'a Type,
    pub value: &'a Value,
    /// Enclosing struct types, innermost last, for resolving [`Type::Ref`].
    structs: Vec<&'a Arc<StructType>>,
    /// Their wire ids on the stream being written, once described.
    ids: Vec<i64>,
}

impl<'a> DynamicRef<'a> {
    pub fn new(ty: &'a Type, value: &'a Value) -> Self {
        Self {
            ty,
            value,
            structs: Vec::new(),
            ids: Vec::new(),
        }
    }

    fn child(&self, ty: &'a Type, value: &'a Value) -> Self {
        Self {
            ty,
            value,
            structs: self.structs.clone(),
            ids: self.ids.clone(),
        }
    }

    fn resolve(&self) -> Result<&'a Type> {
        Ok(self.ty)
    }

    fn struct_type(&self) -> Result<&'a Arc<StructType>> {
        match self.ty {
            Type::Struct(st) => Ok(st),
            Type::Ref(n) => self
                .structs
                .len()
                .checked_sub(n + 1)
                .and_then(|i| self.structs.get(i))
                .copied()
                .ok_or_else(|| Error::Encode(format!("dangling type reference {n}"))),
            _ => Err(Error::Encode("not a struct".into())),
        }
    }
}

impl Encode for DynamicRef<'_> {
    fn describe_value(&self, d: &mut Describer<'_>) -> Result<i64> {
        self.ty.describe(d, &self.ids)
    }

    fn is_zero(&self) -> bool {
        match self.value {
            Value::Bool(b) => !b,
            Value::Int(i) => *i == 0,
            Value::Uint(u) => *u == 0,
            Value::Float(f) => *f == 0.0,
            Value::Complex(re, im) => *re == 0.0 && *im == 0.0,
            Value::Bytes(b) => b.is_empty(),
            Value::String(s) => s.is_empty(),
            Value::Interface(i) => i.is_none(),
            Value::Slice(items) => items.is_empty(),
            // A dynamic map was sent, so it was non-nil; Go sends a non-nil map even when empty.
            Value::Map(_) => false,
            Value::Array(_) | Value::Struct(_) => false,
            // Unknowable without the Go type; a marshaled value that arrived was non-zero.
            Value::Marshaled(_) => false,
        }
    }

    fn frames_as_struct(&self) -> bool {
        matches!(self.ty, Type::Struct(_) | Type::Ref(_))
    }

    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        let mismatch = || {
            Error::Encode(format!(
                "dynamic value {:?} does not match its type {:?}",
                self.value, self.ty
            ))
        };
        match (self.resolve()?, self.value) {
            (Type::Bool, Value::Bool(b)) => e.bool(*b),
            (Type::Int, Value::Int(i)) => e.int(*i),
            (Type::Uint, Value::Uint(u)) => e.uint(*u),
            (Type::Float, Value::Float(f)) => e.float(*f),
            (Type::Complex, Value::Complex(re, im)) => e.complex(*re, *im),
            (Type::Bytes, Value::Bytes(b)) => e.bytes(b),
            (Type::String, Value::String(s)) => e.bytes(s.as_bytes()),
            (Type::Marshaler(..), Value::Marshaled(b)) => e.bytes(b),
            (Type::Interface, Value::Interface(i)) => match i {
                None => e.interface(None)?,
                // A new type tree: references inside it are relative to its own root.
                Some(i) => e.interface(Some((&i.name, &DynamicRef::new(&i.ty, &i.value))))?,
            },
            (Type::Slice(t), Value::Slice(items)) => {
                e.uint(items.len() as u64);
                for v in items {
                    self.child(t, v).encode(e)?;
                }
            }
            (Type::Array(t, n), Value::Array(items)) => {
                if items.len() != *n {
                    return Err(mismatch());
                }
                e.uint(items.len() as u64);
                for v in items {
                    self.child(t, v).encode(e)?;
                }
            }
            (Type::Map(kt, vt), Value::Map(pairs)) => {
                e.uint(pairs.len() as u64);
                for (k, v) in pairs {
                    self.child(kt, k).encode(e)?;
                    self.child(vt, v).encode(e)?;
                }
            }
            (Type::Struct(_) | Type::Ref(_), Value::Struct(fields)) => {
                let st = self.struct_type()?;
                if fields.len() != st.fields.len() {
                    return Err(mismatch());
                }
                let id = self.describe_value(&mut e.describer())?;
                let mut inner = self.clone();
                inner.structs.push(st);
                inner.ids.push(id);
                let mut s = e.structure();
                for (i, ((_, t), v)) in st.fields.iter().zip(fields).enumerate() {
                    if let Some(v) = v {
                        s.field(i as u32, &inner.child(t, v))?;
                    }
                }
                s.end();
            }
            _ => return Err(mismatch()),
        }
        Ok(())
    }
}

/// A Go interface-typed field or value (`any`, `error`, ...): `None` is nil.
impl GobType for Option<Interface> {
    fn describe(_: &mut Describer<'_>) -> Result<i64> {
        Ok(ids::INTERFACE)
    }

    fn compatible(_: &TypeTable, wire: i64) -> bool {
        wire == ids::INTERFACE
    }
}

impl Encode for Option<Interface> {
    fn describe_value(&self, _: &mut Describer<'_>) -> Result<i64> {
        Ok(ids::INTERFACE)
    }

    fn is_zero(&self) -> bool {
        self.is_none()
    }

    fn encode(&self, e: &mut ValueEncoder<'_>) -> Result<()> {
        match self {
            None => e.interface(None),
            Some(i) => e.interface(Some((&i.name, &DynamicRef::new(&i.ty, &i.value)))),
        }
    }
}

impl Decode for Option<Interface> {
    /// Always replaces the destination: Go allocates a fresh concrete value (decode.go,
    /// `decodeInterface`, `allocValue`).
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()> {
        if wire != ids::INTERFACE {
            return Err(d.mismatch::<Self>(wire));
        }
        *self = d.read_interface()?;
        Ok(())
    }
}

impl Type {
    /// Whether values of remote type `wire` can be decoded into this dynamic shape.
    pub fn accepts(&self, types: &TypeTable, wire: i64) -> bool {
        self.matches(types, wire)
    }
}
