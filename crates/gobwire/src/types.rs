//! Wire type definitions: what `(-id, wireType)` messages carry, how the encoder assigns them,
//! and the decoder's table of what the remote side has defined.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use crate::decode::ValueDecoder;
use crate::error::{Error, Result};
use crate::wire::{frame, ids, put_bytes, put_int, put_uint};

/// Which marshaling interface a type's bytes came from (type.go, `wireType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarshalKind {
    /// `GobEncoder` / `GobDecoder`.
    Gob,
    /// `encoding.BinaryMarshaler` / `BinaryUnmarshaler`.
    Binary,
    /// `encoding.TextMarshaler` / `TextUnmarshaler`. Defined by the wire format but never produced
    /// by Go's encoder, whose text-marshaler support is commented out (type.go:85-89).
    Text,
}

/// One field of a remote struct type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireField {
    pub name: String,
    pub id: i64,
}

/// A type definition as received on the wire (encoding/gob/type.go, `wireType`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireType {
    Array {
        name: String,
        elem: i64,
        len: i64,
    },
    Slice {
        name: String,
        elem: i64,
    },
    Struct {
        name: String,
        fields: Vec<WireField>,
    },
    Map {
        name: String,
        key: i64,
        elem: i64,
    },
    Marshaler {
        name: String,
        kind: MarshalKind,
    },
}

impl WireType {
    pub fn name(&self) -> &str {
        match self {
            WireType::Array { name, .. }
            | WireType::Slice { name, .. }
            | WireType::Struct { name, .. }
            | WireType::Map { name, .. }
            | WireType::Marshaler { name, .. } => name,
        }
    }

    /// Encode as the value of Go's `wireType` struct.
    ///
    /// Field numbers: `wireType{ArrayT 0, SliceT 1, StructT 2, MapT 3, GobEncoderT 4,
    /// BinaryMarshalerT 5, TextMarshalerT 6}`; every inner type opens with an embedded
    /// `CommonType{Name 0, Id 1}`. Zero fields are omitted as for any struct.
    pub(crate) fn encode(&self, id: i64, out: &mut Vec<u8>) {
        fn common(out: &mut Vec<u8>, name: &str, id: i64) {
            put_uint(out, 1); // field 0: CommonType (a struct: always sent)
            if !name.is_empty() {
                put_uint(out, 1); // Name
                put_bytes(out, name.as_bytes());
                put_uint(out, 1); // Id
            } else {
                put_uint(out, 2); // Id
            }
            put_int(out, id);
            put_uint(out, 0);
        }
        let outer_field = match self {
            WireType::Array { .. } => 0,
            WireType::Slice { .. } => 1,
            WireType::Struct { .. } => 2,
            WireType::Map { .. } => 3,
            WireType::Marshaler {
                kind: MarshalKind::Gob,
                ..
            } => 4,
            WireType::Marshaler {
                kind: MarshalKind::Binary,
                ..
            } => 5,
            WireType::Marshaler {
                kind: MarshalKind::Text,
                ..
            } => 6,
        };
        put_uint(out, outer_field + 1);
        match self {
            WireType::Array { name, elem, len } => {
                common(out, name, id);
                put_uint(out, 1);
                put_int(out, *elem);
                if *len != 0 {
                    put_uint(out, 1);
                    put_int(out, *len);
                }
            }
            WireType::Slice { name, elem } => {
                common(out, name, id);
                put_uint(out, 1);
                put_int(out, *elem);
            }
            WireType::Struct { name, fields } => {
                common(out, name, id);
                if !fields.is_empty() {
                    put_uint(out, 1);
                    put_uint(out, fields.len() as u64);
                    for f in fields {
                        let mut delta = 1;
                        if !f.name.is_empty() {
                            put_uint(out, 1);
                            put_bytes(out, f.name.as_bytes());
                        } else {
                            delta = 2;
                        }
                        if f.id != 0 {
                            put_uint(out, delta);
                            put_int(out, f.id);
                        }
                        put_uint(out, 0);
                    }
                }
            }
            WireType::Map { name, key, elem } => {
                common(out, name, id);
                put_uint(out, 1);
                put_int(out, *key);
                put_uint(out, 1);
                put_int(out, *elem);
            }
            WireType::Marshaler { name, .. } => common(out, name, id),
        }
        put_uint(out, 0); // end of the inner type
        put_uint(out, 0); // end of wireType
    }

    /// Decode a `wireType` value. Unknown field numbers are corrupt data: the struct is built in
    /// to both sides of every connection.
    pub(crate) fn decode(d: &mut ValueDecoder<'_>) -> Result<(WireType, i64)> {
        let mut out: Option<WireType> = None;
        let mut id = 0;
        fields(d, |d, n| {
            let mut name = String::new();
            let (mut a, mut b) = (0i64, 0i64);
            let mut fs = Vec::new();
            let common = |d: &mut ValueDecoder<'_>, name: &mut String, id: &mut i64| {
                fields(d, |d, k| {
                    match k {
                        0 => *name = d.read_string()?,
                        1 => *id = d.read_int()?,
                        _ => return Err(corrupt("CommonType")),
                    }
                    Ok(())
                })
            };
            let wt = match n {
                0 => {
                    fields(d, |d, k| {
                        match k {
                            0 => common(d, &mut name, &mut id)?,
                            1 => a = d.read_int()?,
                            2 => b = d.read_int()?,
                            _ => return Err(corrupt("arrayType")),
                        }
                        Ok(())
                    })?;
                    WireType::Array {
                        name,
                        elem: a,
                        len: b,
                    }
                }
                1 => {
                    fields(d, |d, k| {
                        match k {
                            0 => common(d, &mut name, &mut id)?,
                            1 => a = d.read_int()?,
                            _ => return Err(corrupt("sliceType")),
                        }
                        Ok(())
                    })?;
                    WireType::Slice { name, elem: a }
                }
                2 => {
                    fields(d, |d, k| {
                        match k {
                            0 => common(d, &mut name, &mut id)?,
                            1 => {
                                let count = d.read_uint()?;
                                for _ in 0..count {
                                    d.expect_element(count)?;
                                    let mut f = WireField {
                                        name: String::new(),
                                        id: 0,
                                    };
                                    fields(d, |d, j| {
                                        match j {
                                            0 => f.name = d.read_string()?,
                                            1 => f.id = d.read_int()?,
                                            _ => return Err(corrupt("fieldType")),
                                        }
                                        Ok(())
                                    })?;
                                    fs.push(f);
                                }
                            }
                            _ => return Err(corrupt("structType")),
                        }
                        Ok(())
                    })?;
                    WireType::Struct { name, fields: fs }
                }
                3 => {
                    fields(d, |d, k| {
                        match k {
                            0 => common(d, &mut name, &mut id)?,
                            1 => a = d.read_int()?,
                            2 => b = d.read_int()?,
                            _ => return Err(corrupt("mapType")),
                        }
                        Ok(())
                    })?;
                    WireType::Map {
                        name,
                        key: a,
                        elem: b,
                    }
                }
                4..=6 => {
                    fields(d, |d, k| {
                        match k {
                            0 => common(d, &mut name, &mut id)?,
                            _ => return Err(corrupt("gobEncoderType")),
                        }
                        Ok(())
                    })?;
                    let kind = match n {
                        4 => MarshalKind::Gob,
                        5 => MarshalKind::Binary,
                        _ => MarshalKind::Text,
                    };
                    WireType::Marshaler { name, kind }
                }
                _ => return Err(corrupt("wireType")),
            };
            // Go's decoder keeps every pointer it was sent and consults them in a fixed order
            // (decode.go, decIgnoreOpFor: array, map, slice, struct, marshalers). A well-formed
            // stream sets exactly one; keep the first.
            if out.is_none() {
                out = Some(wt);
            }
            Ok(())
        })?;
        let wt = out.ok_or_else(|| corrupt("empty wireType"))?;
        Ok((wt, id))
    }
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("bad data: unknown field in {what}"))
}

/// Walk a struct's `(delta, value)` pairs, handing each field number to `f`.
fn fields(
    d: &mut ValueDecoder<'_>,
    mut f: impl FnMut(&mut ValueDecoder<'_>, u64) -> Result<()>,
) -> Result<()> {
    let mut field: i64 = -1;
    while !d.at_end() {
        let delta = d.read_uint()?;
        if delta == 0 {
            break;
        }
        field = field
            .checked_add(
                i64::try_from(delta)
                    .map_err(|_| Error::Corrupt("bad data: field numbers out of bounds".into()))?,
            )
            .ok_or_else(|| Error::Corrupt("bad data: field numbers out of bounds".into()))?;
        f(d, field as u64)?;
    }
    Ok(())
}

/// The decoder's record of every type the remote side has defined on this stream.
///
/// While a value is being *scanned* for completeness (see [`crate::Decoder`]), definitions met
/// inside it are staged rather than committed, so a scan that has to wait for another message
/// can be restarted from the top without tripping the duplicate-type check.
#[derive(Debug, Default, Clone)]
pub struct TypeTable {
    main: HashMap<i64, Arc<WireType>>,
    staged: HashMap<i64, Arc<WireType>>,
    staging: bool,
}

impl TypeTable {
    pub fn get(&self, id: i64) -> Option<&Arc<WireType>> {
        self.main.get(&id).or_else(|| self.staged.get(&id))
    }

    /// True for a built-in id or one this stream has defined.
    pub fn is_known(&self, id: i64) -> bool {
        (ids::BOOL..=ids::INTERFACE).contains(&id) || self.get(id).is_some()
    }

    pub fn is_struct(&self, id: i64) -> bool {
        matches!(self.get(id).map(|t| &**t), Some(WireType::Struct { .. }))
    }

    pub(crate) fn define(&mut self, id: i64, wt: WireType) -> Result<()> {
        // decoder.go, recvType: `if id < firstUserId || dec.wireType[id] != nil`.
        let taken = self.main.contains_key(&id) || (self.staging && self.staged.contains_key(&id));
        if id < ids::FIRST_USER || taken {
            return Err(Error::DuplicateType(id));
        }
        if self.staging {
            self.staged.insert(id, Arc::new(wt));
        } else {
            self.main.insert(id, Arc::new(wt));
        }
        Ok(())
    }

    pub(crate) fn begin_staging(&mut self) {
        self.staged.clear();
        self.staging = true;
    }

    pub(crate) fn end_staging(&mut self) {
        self.staging = false;
    }

    pub(crate) fn clear_staged(&mut self) {
        self.staged.clear();
        self.staging = false;
    }

    /// A readable name for a type id, for error messages (decode.go, `typeString`).
    pub fn describe(&self, id: i64) -> String {
        match id {
            ids::BOOL => "bool".into(),
            ids::INT => "int".into(),
            ids::UINT => "uint".into(),
            ids::FLOAT => "float".into(),
            ids::BYTES => "[]byte".into(),
            ids::STRING => "string".into(),
            ids::COMPLEX => "complex".into(),
            ids::INTERFACE => "interface".into(),
            _ => match self.get(id) {
                Some(t) if !t.name().is_empty() => t.name().to_owned(),
                Some(t) => format!("{t:?}"),
                None => format!("type id {id}"),
            },
        }
    }
}

/// The encoder's record of which local types it has already defined on this stream.
#[derive(Debug)]
pub(crate) struct SentTypes {
    sent: HashMap<Cow<'static, str>, i64>,
    next: i64,
    /// Keys added since the current top-level `encode` began, removed again if it fails.
    journal: Vec<Cow<'static, str>>,
}

impl Default for SentTypes {
    fn default() -> Self {
        Self {
            sent: HashMap::new(),
            next: ids::FIRST_USER + 1,
            journal: Vec::new(),
        }
    }
}

impl SentTypes {
    pub(crate) fn begin(&mut self) -> i64 {
        self.journal.clear();
        self.next
    }

    pub(crate) fn rollback(&mut self, next: i64) {
        for key in self.journal.drain(..) {
            self.sent.remove(&key);
        }
        self.next = next;
    }
}

/// Assigns wire type ids for local types and queues their definitions.
///
/// Implementations of [`GobType::describe`](crate::GobType::describe) call one of the
/// constructors here; a type already defined on this stream returns its existing id and queues
/// nothing. Definitions are queued as complete messages to be written **before** the value that
/// needs them, which Go's decoder accepts even for types first met inside an interface.
pub struct Describer<'a> {
    pub(crate) sent: &'a mut SentTypes,
    pub(crate) defs: &'a mut Vec<u8>,
}

impl Describer<'_> {
    fn lookup(&self, key: &str) -> Option<i64> {
        self.sent.sent.get(key).copied()
    }

    fn reserve(&mut self, key: Cow<'static, str>) -> i64 {
        let id = self.sent.next;
        self.sent.next += 1;
        self.sent.journal.push(key.clone());
        self.sent.sent.insert(key, id);
        id
    }

    fn emit(&mut self, id: i64, wt: &WireType) {
        let mut body = Vec::new();
        put_int(&mut body, -id);
        wt.encode(id, &mut body);
        frame(self.defs, &body);
    }

    /// A slice type (`[]elem`). `key` identifies the local type; `elem` is the element's id.
    pub fn slice(&mut self, key: impl Into<Cow<'static, str>>, elem: i64) -> i64 {
        let key = key.into();
        if let Some(id) = self.lookup(&key) {
            return id;
        }
        let id = self.reserve(key);
        self.emit(
            id,
            &WireType::Slice {
                name: String::new(),
                elem,
            },
        );
        id
    }

    /// An array type (`[len]elem`).
    pub fn array(&mut self, key: impl Into<Cow<'static, str>>, elem: i64, len: usize) -> i64 {
        let key = key.into();
        if let Some(id) = self.lookup(&key) {
            return id;
        }
        let id = self.reserve(key);
        self.emit(
            id,
            &WireType::Array {
                name: String::new(),
                elem,
                len: len as i64,
            },
        );
        id
    }

    /// A map type (`map[key]elem`).
    pub fn map(&mut self, key: impl Into<Cow<'static, str>>, key_id: i64, elem: i64) -> i64 {
        let key = key.into();
        if let Some(id) = self.lookup(&key) {
            return id;
        }
        let id = self.reserve(key);
        self.emit(
            id,
            &WireType::Map {
                name: String::new(),
                key: key_id,
                elem,
            },
        );
        id
    }

    /// A type encoded by a marshaler (`GobEncoder`, `BinaryMarshaler` or `TextMarshaler`).
    pub fn marshaler(
        &mut self,
        key: impl Into<Cow<'static, str>>,
        name: &str,
        kind: MarshalKind,
    ) -> i64 {
        let key = key.into();
        if let Some(id) = self.lookup(&key) {
            return id;
        }
        let id = self.reserve(key);
        self.emit(
            id,
            &WireType::Marshaler {
                name: name.to_owned(),
                kind,
            },
        );
        id
    }

    /// A struct type. The id is reserved before `fields` runs, so a field may refer back to the
    /// struct itself (a recursive type).
    pub fn structure(
        &mut self,
        key: impl Into<Cow<'static, str>>,
        name: &str,
        fields: impl FnOnce(&mut Describer<'_>) -> Result<Vec<(&'static str, i64)>>,
    ) -> Result<i64> {
        let key = key.into();
        if let Some(id) = self.lookup(&key) {
            return Ok(id);
        }
        let id = self.reserve(key);
        let fs = fields(self)?;
        let wt = WireType::Struct {
            name: name.to_owned(),
            fields: fs
                .into_iter()
                .map(|(n, id)| WireField {
                    name: n.to_owned(),
                    id,
                })
                .collect(),
        };
        self.emit(id, &wt);
        Ok(id)
    }

    /// A struct type whose field names are only known at run time (dynamic values).
    pub(crate) fn structure_owned(
        &mut self,
        key: String,
        name: &str,
        fields: impl FnOnce(&mut Describer<'_>, i64) -> Result<Vec<(String, i64)>>,
    ) -> Result<i64> {
        if let Some(id) = self.lookup(&key) {
            return Ok(id);
        }
        let id = self.reserve(Cow::Owned(key));
        let fs = fields(self, id)?;
        let wt = WireType::Struct {
            name: name.to_owned(),
            fields: fs
                .into_iter()
                .map(|(name, id)| WireField { name, id })
                .collect(),
        };
        self.emit(id, &wt);
        Ok(id)
    }
}
