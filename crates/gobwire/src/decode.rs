//! The decoder (encoding/gob/decoder.go, decode.go).

use std::collections::HashMap;
use std::sync::Arc;

use crate::encode::GobType;
use crate::error::{Error, Result};
use crate::types::{TypeTable, WireType};
use crate::value::{Interface, StructType, Type, Value};
use crate::wire::{ids, parse_uint, uint_to_int};

/// Deepest nesting of values the decoder follows before giving up.
///
/// Go has no such limit for values (only `maxIgnoreNestingDepth` for skipped *types*), relying on
/// growable goroutine stacks. A Rust thread's stack is fixed, so a hostile or pathological stream
/// fails with an error here rather than overflowing it.
pub const MAX_DEPTH: usize = 512;

/// A value that can be decoded from a gob stream **into an existing value**.
///
/// Go's decoder does not reset its destination: struct fields absent from the stream keep their
/// values, pointers and slices are reused, maps gain entries. Implementations must preserve
/// that, which is why this takes `&mut self` rather than returning a fresh value.
pub trait Decode: GobType {
    fn decode_into(&mut self, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()>;

    #[doc(hidden)]
    /// `[]Self` into a `Vec<Self>` (decode.go, `decodeSlice`). Overridden by `u8`.
    fn decode_vec(vec: &mut Vec<Self>, d: &mut ValueDecoder<'_>, wire: i64) -> Result<()>
    where
        Self: Sized + Default,
    {
        let elem = match d.types.get(wire).map(|t| &**t) {
            Some(WireType::Slice { elem, .. }) => *elem,
            _ => return Err(d.mismatch::<Vec<Self>>(wire)),
        };
        let n = d.read_len()?;
        // Go reuses the backing array when its capacity suffices (merging into the elements
        // already there) and otherwise allocates zeroed elements. A Rust `Vec`'s capacity is not
        // a Go slice's, so length stands in for capacity: see the crate docs.
        if vec.len() < n {
            vec.clear();
            vec.reserve(n.min(d.remaining()));
        } else {
            vec.truncate(n);
        }
        for i in 0..n {
            d.expect_element(n as u64)?;
            if i == vec.len() {
                vec.push(Self::default());
            }
            vec[i].decode_into(d, elem)?;
        }
        Ok(())
    }
}

/// What [`Decoder::push_message`] made of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The message defined a type. Push the next one.
    TypeDefinition(i64),
    /// The message began or continued a value that is not complete yet. Push the next one.
    NeedMore,
    /// A complete value is buffered: call [`Decoder::decode_into`], [`Decoder::decode`] or
    /// [`Decoder::discard`].
    Ready,
}

/// Decodes values from one gob stream, fed one message at a time.
///
/// Framing is left to the caller (see [`crate::read_message`] and [`crate::StreamDecoder`]),
/// so the same decoder serves blocking and async transports.
///
/// **A value may span several messages.** When an interface holds a type the stream has not
/// defined yet, Go's encoder flushes everything buffered so far — the partial outer value plus
/// the new definition — as one message, and the value continues in the next (encode.go,
/// `encodeInterface` → encoder.go, `sendTypeDescriptor`). Go's decoder reads the next message
/// in the middle of the value. This decoder instead *scans* each pushed message (a cheap skip
/// pass) to find out whether the value is complete, and only then decodes, so a destination is
/// never left half-written by a value that turns out to need another message.
#[derive(Debug, Default)]
pub struct Decoder {
    types: TypeTable,
    pending: Vec<u8>,
    ends: Vec<usize>,
    ready: bool,
    plans: HashMap<(&'static str, i64), Arc<StructPlan>>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The types the remote side has defined so far.
    pub fn types(&self) -> &TypeTable {
        &self.types
    }

    /// Feed one message body (without its length prefix).
    pub fn push_message(&mut self, body: &[u8]) -> Result<Progress> {
        if self.ready {
            return Err(Error::Corrupt("a decoded value is still buffered".into()));
        }
        if self.pending.is_empty() {
            let (first, _) = parse_uint(body)?.ok_or(Error::UnexpectedEof)?;
            let id = uint_to_int(first);
            if id < 0 {
                // decoder.go, decodeTypeSequence(isInterface = false).
                let mut d = ValueDecoder::new(body, &[], &mut self.types, &mut self.plans);
                d.read_int()?;
                let (wt, _) = WireType::decode(&mut d)?;
                if !d.at_end() {
                    return Err(Error::Corrupt("extra data in buffer".into()));
                }
                self.types.define(-id, wt)?;
                return Ok(Progress::TypeDefinition(-id));
            }
        }
        self.pending.extend_from_slice(body);
        self.ends.push(self.pending.len());
        self.types.begin_staging();
        let scanned = {
            let mut d =
                ValueDecoder::new(&self.pending, &self.ends, &mut self.types, &mut self.plans);
            d.scanning = true;
            d.scan_top()
        };
        self.types.clear_staged();
        match scanned {
            Ok(()) => {
                self.ready = true;
                Ok(Progress::Ready)
            }
            Err(Error::NeedMore) => Ok(Progress::NeedMore),
            Err(e) => {
                self.reset();
                Err(e)
            }
        }
    }

    /// Decode the buffered value into `dest`, merging as Go does.
    pub fn decode_into<T: Decode + ?Sized>(&mut self, dest: &mut T) -> Result<()> {
        if !self.ready {
            return Err(Error::Corrupt("no complete value buffered".into()));
        }
        let result = {
            let mut d =
                ValueDecoder::new(&self.pending, &self.ends, &mut self.types, &mut self.plans);
            d.decode_top(dest)
        };
        self.reset();
        result
    }

    /// Decode the buffered value into a fresh `T::default()`.
    pub fn decode<T: Decode + Default>(&mut self) -> Result<T> {
        let mut v = T::default();
        self.decode_into(&mut v)?;
        Ok(v)
    }

    /// Drop the buffered value, still recording any types defined inside it.
    pub fn discard(&mut self) -> Result<()> {
        if !self.ready {
            return Err(Error::Corrupt("no complete value buffered".into()));
        }
        let result = {
            let mut d =
                ValueDecoder::new(&self.pending, &self.ends, &mut self.types, &mut self.plans);
            d.scan_top()
        };
        self.reset();
        result
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.ends.clear();
        self.ready = false;
        self.types.end_staging();
    }
}

/// How a remote struct's fields map onto a local struct (decode.go, `compileDec`).
#[derive(Debug)]
pub struct StructPlan {
    /// Per remote field: its type id and the index of the local field it fills, if any.
    fields: Vec<(i64, Option<usize>)>,
    matched: usize,
}

/// Reads one value's bytes. Handed to [`Decode::decode_into`].
pub struct ValueDecoder<'a> {
    buf: &'a [u8],
    pos: usize,
    ends: &'a [usize],
    pub(crate) types: &'a mut TypeTable,
    plans: &'a mut HashMap<(&'static str, i64), Arc<StructPlan>>,
    depth: usize,
    /// Enclosing dynamic struct types, innermost last, for resolving [`Type::Ref`].
    dyn_structs: Vec<Arc<StructType>>,
    scanning: bool,
    /// Set for the value handed to `Decoder::decode` and to an interface's concrete value:
    /// Go's `decodeValue` rejects a struct with which no field matched.
    check_matched: bool,
}

impl<'a> ValueDecoder<'a> {
    fn new(
        buf: &'a [u8],
        ends: &'a [usize],
        types: &'a mut TypeTable,
        plans: &'a mut HashMap<(&'static str, i64), Arc<StructPlan>>,
    ) -> Self {
        Self {
            buf,
            pos: 0,
            ends,
            types,
            plans,
            depth: 0,
            dyn_structs: Vec::new(),
            scanning: false,
            check_matched: false,
        }
    }

    pub fn types(&self) -> &TypeTable {
        self.types
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Whether Go's per-message buffer would be empty here.
    fn at_message_end(&self) -> bool {
        self.at_end() || self.ends.contains(&self.pos)
    }

    /// Refuse the next element of an `n`-element slice, array or map when the input is exhausted
    /// (decode.go, `decodeArrayHelper`: "length exceeds input size").
    ///
    /// Every count-driven loop must call this before each element. A struct value at end of input
    /// consumes **no bytes** (its field loop is Go's `for state.b.Len() > 0`), so without it a
    /// corrupted count of `2^60` struct elements loops without reading anything — which, in a type
    /// definition's field list, grew one test process to 116 GB before the kernel killed it.
    pub fn expect_element(&self, n: u64) -> Result<()> {
        if self.at_end() {
            return Err(Error::Corrupt(format!(
                "decoding array or slice: length exceeds input size ({n} elements)"
            )));
        }
        Ok(())
    }

    pub fn read_uint(&mut self) -> Result<u64> {
        match parse_uint(&self.buf[self.pos.min(self.buf.len())..])? {
            Some((u, width)) => {
                self.pos += width;
                Ok(u)
            }
            None => Err(Error::UnexpectedEof),
        }
    }

    pub fn read_int(&mut self) -> Result<i64> {
        Ok(uint_to_int(self.read_uint()?))
    }

    pub fn read_float(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.read_uint()?.swap_bytes()))
    }

    /// A count that must fit in memory and in what remains of the input (decode.go, getLength).
    fn read_len(&mut self) -> Result<usize> {
        let n = self.read_uint()?;
        usize::try_from(n).map_err(|_| Error::Corrupt(format!("length {n} too large")))
    }

    pub fn read_bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.read_len()?;
        if n > self.remaining() {
            return Err(Error::Corrupt(format!(
                "invalid length {n}: exceeds input size {}",
                self.remaining()
            )));
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }

    pub fn read_string(&mut self) -> Result<String> {
        let b = self.read_bytes()?;
        String::from_utf8(b.to_vec()).map_err(|_| Error::InvalidUtf8)
    }

    /// The error for a remote type this local type cannot hold.
    pub fn mismatch<T: ?Sized>(&self, wire: i64) -> Error {
        Error::TypeMismatch(format!(
            "decoding into local type {}, received remote type {}",
            std::any::type_name::<T>(),
            self.types.describe(wire)
        ))
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Error::Corrupt("invalid nesting depth".into()));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn decode_top<T: Decode + ?Sized>(&mut self, dest: &mut T) -> Result<()> {
        let id = self.read_int()?;
        if id < 0 {
            return Err(Error::Corrupt(
                "expected a value, found a type definition".into(),
            ));
        }
        if !self.types.is_known(id) {
            return Err(Error::UndefinedType(id));
        }
        self.decode_framed(dest, id)
    }

    /// A top-level or interface-held value: structs framed as structs, everything else as a
    /// singleton (decode.go, `decodeValue`, `decodeSingle`).
    fn decode_framed<T: Decode + ?Sized>(&mut self, dest: &mut T, id: i64) -> Result<()> {
        if !T::compatible(self.types, id) {
            return Err(self.mismatch::<T>(id));
        }
        let is_struct = self.types.is_struct(id);
        if !is_struct && self.read_uint()? != 0 {
            return Err(Error::Corrupt(
                "decode: corrupted data: non-zero delta for singleton".into(),
            ));
        }
        self.check_matched = is_struct;
        let r = dest.decode_into(self, id);
        self.check_matched = false;
        r
    }

    fn scan_top(&mut self) -> Result<()> {
        let id = self.read_int()?;
        if id < 0 {
            return Err(Error::Corrupt(
                "expected a value, found a type definition".into(),
            ));
        }
        if !self.types.is_known(id) {
            return Err(Error::UndefinedType(id));
        }
        if !self.types.is_struct(id) && self.read_uint()? != 0 {
            return Err(Error::Corrupt(
                "decode: corrupted data: non-zero delta for singleton".into(),
            ));
        }
        self.skip(id)
    }

    /// Build (or fetch) the field mapping from remote struct `wire` onto the local struct `key`.
    ///
    /// `names` are the local fields' Go names in declaration order; `compatible(types, i, w)`
    /// answers whether local field `i` can hold remote type `w`. A remote field whose name
    /// matches but whose type is incompatible fails the whole struct, whether or not this value
    /// carries it — Go checks when it compiles the decoder, not per value.
    pub fn struct_plan(
        &mut self,
        key: &'static str,
        wire: i64,
        names: &[&str],
        compatible: impl Fn(&TypeTable, usize, i64) -> bool,
    ) -> Result<Arc<StructPlan>> {
        let check_matched = std::mem::take(&mut self.check_matched);
        let plan = match self.plans.get(&(key, wire)) {
            Some(plan) => plan.clone(),
            None => {
                let fields = match self.types.get(wire).map(|t| &**t) {
                    Some(WireType::Struct { fields, .. }) => fields.clone(),
                    _ => {
                        return Err(Error::TypeMismatch(format!(
                            "type mismatch in decoder: want struct type {key}; got non-struct"
                        )));
                    }
                };
                let mut out = Vec::with_capacity(fields.len());
                let mut matched = 0;
                for f in &fields {
                    if f.name.is_empty() {
                        return Err(Error::Corrupt(format!(
                            "empty name for remote field of type {key}"
                        )));
                    }
                    let local = names.iter().position(|n| *n == f.name);
                    if let Some(i) = local {
                        if !compatible(self.types, i, f.id) {
                            return Err(Error::TypeMismatch(format!(
                                "wrong type ({}) for received field {key}.{}",
                                self.types.describe(f.id),
                                f.name
                            )));
                        }
                        matched += 1;
                    }
                    out.push((f.id, local));
                }
                let plan = Arc::new(StructPlan {
                    fields: out,
                    matched,
                });
                // Plans are keyed by remote id, which only means something on this stream while
                // the type is committed; a plan built against a staged type is not kept.
                if self.types.get(wire).is_some() && !self.scanning {
                    self.plans.insert((key, wire), plan.clone());
                }
                plan
            }
        };
        if check_matched && plan.matched == 0 && !names.is_empty() && !plan.fields.is_empty() {
            return Err(Error::TypeMismatch(format!(
                "type mismatch: no fields matched compiling decoder for {key}"
            )));
        }
        Ok(plan)
    }

    /// Iterate a struct value's fields against `plan`.
    pub fn structure(&mut self, plan: Arc<StructPlan>) -> Result<StructDecoder<'_, 'a>> {
        self.enter()?;
        Ok(StructDecoder {
            d: self,
            plan,
            field: -1,
        })
    }

    /// Skip a value of remote type `wire` (decode.go, `decIgnoreOpFor`).
    pub fn skip(&mut self, wire: i64) -> Result<()> {
        self.enter()?;
        let r = self.skip_inner(wire);
        self.leave();
        r
    }

    fn skip_inner(&mut self, wire: i64) -> Result<()> {
        match wire {
            ids::BOOL | ids::INT | ids::UINT | ids::FLOAT => {
                self.read_uint()?;
            }
            ids::COMPLEX => {
                self.read_uint()?;
                self.read_uint()?;
            }
            ids::BYTES | ids::STRING => {
                self.read_bytes()?;
            }
            ids::INTERFACE => self.skip_interface()?,
            _ => {
                let wt = self
                    .types
                    .get(wire)
                    .cloned()
                    .ok_or(Error::UndefinedType(wire))?;
                match &*wt {
                    WireType::Array { elem, len, .. } => {
                        let n = self.read_uint()?;
                        if n != *len as u64 {
                            return Err(Error::Corrupt("length mismatch in ignoreArray".into()));
                        }
                        self.skip_elements(n, *elem)?;
                    }
                    WireType::Slice { elem, .. } => {
                        let n = self.read_uint()?;
                        self.skip_elements(n, *elem)?;
                    }
                    WireType::Map { key, elem, .. } => {
                        let n = self.read_uint()?;
                        for _ in 0..n {
                            self.expect_element(n)?;
                            self.skip(*key)?;
                            self.skip(*elem)?;
                        }
                    }
                    WireType::Struct { fields, .. } => {
                        let mut field: i64 = -1;
                        while !self.at_end() {
                            let delta = self.read_uint()?;
                            if delta == 0 {
                                break;
                            }
                            field = next_field(field, delta, fields.len())?;
                            self.skip(fields[field as usize].id)?;
                        }
                    }
                    WireType::Marshaler { .. } => {
                        self.read_bytes()?;
                    }
                }
            }
        }
        Ok(())
    }

    fn skip_elements(&mut self, n: u64, elem: i64) -> Result<()> {
        for _ in 0..n {
            self.expect_element(n)?;
            self.skip(elem)?;
        }
        Ok(())
    }

    /// Read an interface's type name and its type sequence, returning the name and concrete id,
    /// or `None` for a nil interface.
    ///
    /// decoder.go, `decodeTypeSequence(isInterface = true)`: definitions may precede the id.
    /// After each, "there may be a DelimitedValue still in the buffer. Skip its count." — unless
    /// Go's buffer is empty, in which case it reads the next message and there is no count. In
    /// the concatenated buffer that is a message boundary.
    fn interface_header(&mut self) -> Result<Option<(String, i64)>> {
        let name = self.read_bytes()?;
        if name.is_empty() {
            return Ok(None);
        }
        if name.len() > 1024 {
            return Err(Error::Corrupt(format!(
                "name too long ({} bytes)",
                name.len()
            )));
        }
        let name = String::from_utf8(name.to_vec()).map_err(|_| Error::InvalidUtf8)?;
        loop {
            if self.at_end() {
                return Err(if self.scanning {
                    Error::NeedMore
                } else {
                    Error::UnexpectedEof
                });
            }
            let id = self.read_int()?;
            if id >= 0 {
                if !self.types.is_known(id) {
                    return Err(Error::UndefinedType(id));
                }
                return Ok(Some((name, id)));
            }
            let (wt, _) = WireType::decode(self)?;
            self.types.define(-id, wt)?;
            if !self.at_message_end() {
                self.read_uint()?;
            }
        }
    }

    /// Skip an interface value (decode.go, `ignoreInterface`) by **parsing** it.
    ///
    /// Go skips by the byte count that precedes the concrete value, and that count is not the
    /// value's length whenever a nested interface defines a type inline: the nested
    /// `sendTypeDescriptor` flushes the partial value as one delimited chunk (encode.go,
    /// `encodeInterface` → encoder.go, `writeMessage`), and the rest follows under a second
    /// count that the nested type sequence skips. Go's skip therefore fails on such values
    /// ("field numbers out of bounds"), and so would one that trusted the count. It also reads a
    /// type sequence for a nil interface, which has none. Both are Go bugs this does not copy.
    fn skip_interface(&mut self) -> Result<()> {
        let Some((_, id)) = self.interface_header()? else {
            return Ok(());
        };
        self.read_uint()?; // the (unreliable) byte count
        if !self.types.is_struct(id) && self.read_uint()? != 0 {
            return Err(Error::Corrupt(
                "decode: corrupted data: non-zero delta for singleton".into(),
            ));
        }
        self.skip(id)
    }

    /// Decode an interface value into its dynamic form.
    pub fn read_interface(&mut self) -> Result<Option<Interface>> {
        let Some((name, id)) = self.interface_header()? else {
            return Ok(None);
        };
        // The byte count is not the value's length (see `skip_interface`); Go ignores it too.
        self.read_uint()?;
        let ty = self.dynamic_type(id)?;
        if !ty.is_struct() && self.read_uint()? != 0 {
            return Err(Error::Corrupt(
                "decode: corrupted data: non-zero delta for singleton".into(),
            ));
        }
        let value = self.read_dynamic(&ty, id)?;
        Ok(Some(Interface { name, ty, value }))
    }

    /// The dynamic [`Type`] for a remote type id.
    pub fn dynamic_type(&self, id: i64) -> Result<Type> {
        let mut stack = Vec::new();
        self.dynamic_type_inner(id, &mut stack)
    }

    /// `path` holds the ids being expanded, outermost first. Meeting one again is recursion: a
    /// struct becomes [`Type::Ref`] to itself, counted in enclosing structs; recursion that
    /// never passes through a struct (`type L []L`) has no dynamic form.
    fn dynamic_type_inner(&self, id: i64, path: &mut Vec<i64>) -> Result<Type> {
        Ok(match id {
            ids::BOOL => Type::Bool,
            ids::INT => Type::Int,
            ids::UINT => Type::Uint,
            ids::FLOAT => Type::Float,
            ids::BYTES => Type::Bytes,
            ids::STRING => Type::String,
            ids::COMPLEX => Type::Complex,
            ids::INTERFACE => Type::Interface,
            _ => {
                let wt = self
                    .types
                    .get(id)
                    .cloned()
                    .ok_or(Error::UndefinedType(id))?;
                if let Some(p) = path.iter().position(|x| *x == id) {
                    if !matches!(&*wt, WireType::Struct { .. }) {
                        return Err(Error::Unsupported(format!(
                            "recursive non-struct type {} cannot be held in a dynamic value",
                            self.types.describe(id)
                        )));
                    }
                    let between = path[p + 1..]
                        .iter()
                        .filter(|i| self.types.is_struct(**i))
                        .count();
                    return Ok(Type::Ref(between));
                }
                path.push(id);
                let t = match &*wt {
                    WireType::Array { elem, len, .. } => Type::Array(
                        Arc::new(self.dynamic_type_inner(*elem, path)?),
                        usize::try_from(*len)
                            .map_err(|_| Error::Corrupt("negative array length".into()))?,
                    ),
                    WireType::Slice { elem, .. } => {
                        Type::Slice(Arc::new(self.dynamic_type_inner(*elem, path)?))
                    }
                    WireType::Map { key, elem, .. } => Type::Map(
                        Arc::new(self.dynamic_type_inner(*key, path)?),
                        Arc::new(self.dynamic_type_inner(*elem, path)?),
                    ),
                    WireType::Struct { name, fields } => {
                        let mut fs = Vec::with_capacity(fields.len());
                        for f in fields {
                            fs.push((f.name.clone(), self.dynamic_type_inner(f.id, path)?));
                        }
                        Type::Struct(Arc::new(StructType {
                            name: name.clone(),
                            fields: fs,
                        }))
                    }
                    WireType::Marshaler { name, kind } => Type::Marshaler(*kind, name.clone()),
                };
                path.pop();
                t
            }
        })
    }

    /// Decode a value of remote type `id`, whose dynamic type is `ty`, without framing.
    pub fn read_dynamic(&mut self, ty: &Type, id: i64) -> Result<Value> {
        self.enter()?;
        let r = self.read_dynamic_inner(ty, id);
        self.leave();
        r
    }

    fn read_dynamic_inner(&mut self, ty: &Type, id: i64) -> Result<Value> {
        Ok(match ty {
            Type::Bool => Value::Bool(self.read_uint()? != 0),
            Type::Int => Value::Int(self.read_int()?),
            Type::Uint => Value::Uint(self.read_uint()?),
            Type::Float => Value::Float(self.read_float()?),
            Type::Complex => {
                let re = self.read_float()?;
                Value::Complex(re, self.read_float()?)
            }
            Type::Bytes => Value::Bytes(self.read_bytes()?.to_vec()),
            Type::String => Value::String(self.read_string()?),
            Type::Interface => Value::Interface(self.read_interface()?.map(Box::new)),
            Type::Marshaler(..) => Value::Marshaled(self.read_bytes()?.to_vec()),
            Type::Slice(elem) | Type::Array(elem, _) => {
                let elem_id = match self.types.get(id).map(|t| &**t) {
                    Some(WireType::Slice { elem, .. } | WireType::Array { elem, .. }) => *elem,
                    _ => return Err(Error::UndefinedType(id)),
                };
                let n = self.read_uint()?;
                if let Type::Array(_, len) = ty
                    && n != *len as u64
                {
                    return Err(Error::Corrupt("length mismatch in decodeArray".into()));
                }
                let mut items = Vec::with_capacity((n as usize).min(self.remaining()));
                for _ in 0..n {
                    self.expect_element(n)?;
                    items.push(self.read_dynamic(elem, elem_id)?);
                }
                if matches!(ty, Type::Array(..)) {
                    Value::Array(items)
                } else {
                    Value::Slice(items)
                }
            }
            Type::Map(kt, vt) => {
                let (key_id, elem_id) = match self.types.get(id).map(|t| &**t) {
                    Some(WireType::Map { key, elem, .. }) => (*key, *elem),
                    _ => return Err(Error::UndefinedType(id)),
                };
                let n = self.read_uint()?;
                let mut pairs = Vec::with_capacity((n as usize).min(self.remaining()));
                for _ in 0..n {
                    self.expect_element(n)?;
                    let k = self.read_dynamic(kt, key_id)?;
                    let v = self.read_dynamic(vt, elem_id)?;
                    pairs.push((k, v));
                }
                Value::Map(pairs)
            }
            Type::Ref(n) => {
                let st = self
                    .dyn_structs
                    .len()
                    .checked_sub(n + 1)
                    .and_then(|i| self.dyn_structs.get(i))
                    .cloned()
                    .ok_or_else(|| Error::Unsupported(format!("dangling type reference {n}")))?;
                return self.read_dynamic_inner(&Type::Struct(st), id);
            }
            Type::Struct(st) => {
                let wire_fields = match self.types.get(id).map(|t| &**t) {
                    Some(WireType::Struct { fields, .. }) => {
                        fields.iter().map(|f| f.id).collect::<Vec<_>>()
                    }
                    _ => return Err(Error::UndefinedType(id)),
                };
                let mut values: Vec<Option<Value>> = vec![None; st.fields.len()];
                self.dyn_structs.push(st.clone());
                let result: Result<()> = (|| {
                    let mut field: i64 = -1;
                    while !self.at_end() {
                        let delta = self.read_uint()?;
                        if delta == 0 {
                            break;
                        }
                        field = next_field(field, delta, st.fields.len())?;
                        let i = field as usize;
                        values[i] = Some(self.read_dynamic(&st.fields[i].1, wire_fields[i])?);
                    }
                    Ok(())
                })();
                self.dyn_structs.pop();
                result?;
                Value::Struct(values)
            }
        })
    }
}

fn next_field(field: i64, delta: u64, count: usize) -> Result<i64> {
    // decode.go, decodeStruct: `if state.fieldnum >= len(engine.instr)-delta { errRange }`.
    let delta = i64::try_from(delta).map_err(|_| out_of_bounds())?;
    let next = field.checked_add(delta).ok_or_else(out_of_bounds)?;
    if next >= count as i64 {
        return Err(out_of_bounds());
    }
    Ok(next)
}

fn out_of_bounds() -> Error {
    Error::Corrupt("internal error: field numbers out of bounds".into())
}

/// Walks one struct value's fields. Created by [`ValueDecoder::structure`].
pub struct StructDecoder<'d, 'a> {
    d: &'d mut ValueDecoder<'a>,
    plan: Arc<StructPlan>,
    field: i64,
}

/// One field present in a struct value.
#[derive(Debug, Clone, Copy)]
pub struct FieldRef {
    /// The local field it maps to, if any.
    pub local: Option<usize>,
    /// Its remote type id.
    pub wire: i64,
}

impl<'a> StructDecoder<'_, 'a> {
    /// The next field present in the value, or `None` at the terminator (or end of input, as
    /// Go's `for state.b.Len() > 0` loop allows).
    pub fn next_field(&mut self) -> Result<Option<FieldRef>> {
        if self.d.at_end() {
            return Ok(None);
        }
        let delta = self.d.read_uint()?;
        if delta == 0 {
            return Ok(None);
        }
        self.field = next_field(self.field, delta, self.plan.fields.len())?;
        let (wire, local) = self.plan.fields[self.field as usize];
        Ok(Some(FieldRef { local, wire }))
    }

    pub fn decoder(&mut self) -> &mut ValueDecoder<'a> {
        self.d
    }
}

impl Drop for StructDecoder<'_, '_> {
    fn drop(&mut self) {
        self.d.leave();
    }
}
