//! Spike-grade Go `encoding/gob`: a dynamic decoder that renders values into the same JSON
//! convention as `reference/dump/spike/gobgen`, and a schema-driven encoder just large enough
//! for net/rpc headers and a few hook arguments.
//!
//! Wire rules follow `encoding/gob/doc.go` and the decoder in `decoder.go`
//! (`decodeTypeSequence`, `recvType`) and `decode.go` (`decodeSingle`, `decodeInterface`).

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value as Json, json};

// Built-in type ids (encoding/gob/type.go:282-288, doc.go).
pub const T_BOOL: i64 = 1;
pub const T_INT: i64 = 2;
pub const T_UINT: i64 = 3;
pub const T_FLOAT: i64 = 4;
pub const T_BYTES: i64 = 5;
pub const T_STRING: i64 = 6;
pub const T_COMPLEX: i64 = 7;
pub const T_INTERFACE: i64 = 8;
// doc.go says user ids start at 65; the code says 64 (type.go:167, firstUserId), and ids are
// assigned process-globally by type, so the numbers a stream carries are not contiguous.
const FIRST_USER_ID: i64 = 64;

#[derive(Debug, Clone)]
pub enum WireType {
    Array {
        elem: i64,
        len: i64,
    },
    Slice {
        elem: i64,
    },
    Struct {
        name: String,
        fields: Vec<(String, i64)>,
    },
    Map {
        key: i64,
        elem: i64,
    },
    GobEncoder,
    BinaryMarshaler,
    TextMarshaler,
}

/// A cursor over one or more concatenated gob message bodies. `ends` records where each body
/// ended, because Go's decoder behaves differently at a message boundary (see `interface`).
pub struct Buf<'a> {
    all: &'a [u8],
    b: &'a [u8],
    ends: &'a [usize],
}

impl<'a> Buf<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self {
            all: b,
            b,
            ends: &[],
        }
    }
    pub fn joined(b: &'a [u8], ends: &'a [usize]) -> Self {
        Self { all: b, b, ends }
    }
    pub fn is_empty(&self) -> bool {
        self.b.is_empty()
    }
    /// True when the cursor sits exactly where a message body ended: Go's buffer would be empty.
    fn at_message_end(&self) -> bool {
        let pos = self.all.len() - self.b.len();
        self.b.is_empty() || self.ends.contains(&pos)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(
            n <= self.b.len(),
            "gob: need {n} bytes, have {}",
            self.b.len()
        );
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    pub fn uint(&mut self) -> Result<u64> {
        let first = self.take(1)?[0];
        if first < 0x80 {
            return Ok(u64::from(first));
        }
        let n = (!first).wrapping_add(1) as usize; // byte count, negated
        ensure!(n <= 8, "gob: uint of {n} bytes");
        Ok(self
            .take(n)?
            .iter()
            .fold(0u64, |acc, &x| acc << 8 | u64::from(x)))
    }
    pub fn int(&mut self) -> Result<i64> {
        let u = self.uint()?;
        Ok(if u & 1 == 1 {
            !((u >> 1) as i64)
        } else {
            (u >> 1) as i64
        })
    }
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = usize::try_from(self.uint()?)?;
        self.take(n)
    }
}

/// Stateful per-stream decoder: type definitions persist across messages, as in Go.
#[derive(Default)]
pub struct Decoder {
    pub types: HashMap<i64, WireType>,
}

#[derive(Debug)]
pub struct NeedMore;

impl std::fmt::Display for NeedMore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("gob: value continues in the next message")
    }
}

impl std::error::Error for NeedMore {}

pub enum Item {
    NeedMore,
    TypeDefined(i64),
    Value(i64, Json),
}

impl Decoder {
    /// Decode one delimited message body (the length prefix already stripped).
    ///
    /// **A value may span several messages.** When an interface carries a concrete type the
    /// stream has not defined yet, Go's `encodeInterface` calls `sendTypeDescriptor`, whose
    /// `writeMessage` flushes *everything buffered so far* — the partial outer value plus the new
    /// type definition — as one message, and the value continues in the next. Go's decoder copes
    /// because `decodeTypeSequence` calls `recvMessage` when its buffer runs dry mid-value. So
    /// this returns `Item::NeedMore`, and the caller appends the next message body and retries
    /// with [`Decoder::message_continued`].
    pub fn message(&mut self, msg: &[u8]) -> Result<Item> {
        let snapshot = self.types.clone();
        match self.message_inner(msg) {
            Err(e) if e.downcast_ref::<NeedMore>().is_some() => {
                self.types = snapshot;
                Ok(Item::NeedMore)
            }
            other => other,
        }
    }

    /// Retry after `NeedMore` with the concatenated bodies of every message since the value
    /// began, and the offsets at which each of those bodies ended.
    pub fn message_continued(&mut self, joined: &[u8], ends: &[usize]) -> Result<Item> {
        let snapshot = self.types.clone();
        match self.decode_item(Buf::joined(joined, ends), joined.len()) {
            Err(e) if e.downcast_ref::<NeedMore>().is_some() => {
                self.types = snapshot;
                Ok(Item::NeedMore)
            }
            other => other,
        }
    }

    fn message_inner(&mut self, msg: &[u8]) -> Result<Item> {
        self.decode_item(Buf::new(msg), msg.len())
    }

    fn decode_item(&mut self, mut b: Buf, len: usize) -> Result<Item> {
        let msg_len = len;
        let id = b.int()?;
        if id < 0 {
            self.recv_type(-id, &mut b)?;
            ensure!(b.is_empty(), "gob: extra data in buffer after type {}", -id);
            return Ok(Item::TypeDefined(-id));
        }
        let v = self.top_value(id, &mut b)?;
        ensure!(
            b.is_empty(),
            "gob: trailing bytes after value of type {id} ({msg_len} bytes)"
        );
        Ok(Item::Value(id, v))
    }

    fn recv_type(&mut self, id: i64, b: &mut Buf) -> Result<()> {
        ensure!(
            id >= FIRST_USER_ID && !self.types.contains_key(&id),
            "gob: duplicate type {id}"
        );
        let wt = read_wire_type(b)?;
        self.types.insert(id, wt);
        Ok(())
    }

    /// A top-level value (or an interface's concrete value): structs are framed as structs,
    /// everything else as a singleton — `uint(0)` then the value (decode.go, decodeSingle).
    fn top_value(&mut self, id: i64, b: &mut Buf) -> Result<Json> {
        if matches!(self.types.get(&id), Some(WireType::Struct { .. })) {
            return self.value(id, b);
        }
        ensure!(b.uint()? == 0, "gob: non-zero delta for singleton");
        self.value(id, b)
    }

    pub fn value(&mut self, id: i64, b: &mut Buf) -> Result<Json> {
        Ok(match id {
            T_BOOL => Json::Bool(b.uint()? != 0),
            T_INT => json!(b.int()?),
            T_UINT => json!(b.uint()?),
            T_FLOAT => json!({ "$f64": b.uint()?.swap_bytes() }),
            T_BYTES => json!({ "$bytes": B64.encode(b.bytes()?) }),
            T_STRING => Json::String(String::from_utf8(b.bytes()?.to_vec())?),
            T_COMPLEX => json!({ "$c128": [b.uint()?.swap_bytes(), b.uint()?.swap_bytes()] }),
            T_INTERFACE => self.interface(b)?,
            _ => {
                let wt = self
                    .types
                    .get(&id)
                    .cloned()
                    .with_context(|| format!("gob: unknown type {id}"))?;
                match wt {
                    WireType::Struct { fields, .. } => {
                        let mut out = Map::new();
                        let mut field: i64 = -1;
                        loop {
                            let delta = b.uint()?;
                            if delta == 0 {
                                break;
                            }
                            field += i64::try_from(delta)?;
                            let (name, fid) = fields
                                .get(usize::try_from(field)?)
                                .with_context(|| format!("gob: field {field} out of range"))?;
                            out.insert(name.clone(), self.value(*fid, b)?);
                        }
                        Json::Object(out)
                    }
                    WireType::Slice { elem } => {
                        if elem == T_UINT {
                            // []uint8 is sent as bytes, but a named/other byte slice may not be
                        }
                        let n = b.uint()?;
                        Json::Array((0..n).map(|_| self.value(elem, b)).collect::<Result<_>>()?)
                    }
                    WireType::Array { elem, len } => {
                        let n = b.uint()?;
                        ensure!(i64::try_from(n)? == len, "gob: array length {n} != {len}");
                        Json::Array((0..n).map(|_| self.value(elem, b)).collect::<Result<_>>()?)
                    }
                    WireType::Map { key, elem } => {
                        let n = b.uint()?;
                        let mut out = Map::new();
                        for _ in 0..n {
                            let k = match self.value(key, b)? {
                                Json::String(s) => s,
                                other => other.to_string(),
                            };
                            out.insert(k, self.value(elem, b)?);
                        }
                        json!({ "$map": out })
                    }
                    WireType::GobEncoder => json!({ "$gob": B64.encode(b.bytes()?) }),
                    WireType::BinaryMarshaler => json!({ "$bin": B64.encode(b.bytes()?) }),
                    WireType::TextMarshaler => json!({ "$text": B64.encode(b.bytes()?) }),
                }
            }
        })
    }

    /// decode.go, decodeInterface + decoder.go, decodeTypeSequence(isInterface = true).
    fn interface(&mut self, b: &mut Buf) -> Result<Json> {
        let name = String::from_utf8(b.bytes()?.to_vec())?;
        if name.is_empty() {
            return Ok(json!({ "$iface": "" }));
        }
        let id = loop {
            let id = b.int()?;
            if id >= 0 {
                break id;
            }
            self.recv_type(-id, b)?;
            // "after a type there may be a DelimitedValue still in the buffer. Skip its count."
            // (Alternatively, the buffer is empty and the byte count will be absorbed by
            // recvMessage.) Here the buffer is the concatenation of message bodies, so an empty
            // buffer means the value continues in a message not yet appended.
            if b.is_empty() {
                return Err(NeedMore.into());
            }
            if !b.at_message_end() {
                b.uint()?;
            }
        };
        let _len = b.uint()?;
        let v = self.top_value(id, b)?;
        Ok(json!({ "$iface": name, "value": v }))
    }
}

/// Walk a struct's (delta, value) pairs, handing each field number to `f`.
fn fields(b: &mut Buf, mut f: impl FnMut(i64, &mut Buf) -> Result<()>) -> Result<()> {
    let mut field: i64 = -1;
    loop {
        let delta = b.uint()?;
        if delta == 0 {
            return Ok(());
        }
        field += i64::try_from(delta)?;
        f(field, b)?;
    }
}

fn common(b: &mut Buf) -> Result<String> {
    let mut name = String::new();
    fields(b, |n, b| {
        match n {
            0 => name = String::from_utf8(b.bytes()?.to_vec())?,
            1 => {
                b.int()?;
            }
            _ => bail!("gob: CommonType field {n}"),
        }
        Ok(())
    })?;
    Ok(name)
}

/// wireType{ArrayT, SliceT, StructT, MapT, GobEncoderT, BinaryMarshalerT, TextMarshalerT}.
fn read_wire_type(b: &mut Buf) -> Result<WireType> {
    let mut out = None;
    fields(b, |n, b| {
        out = Some(match n {
            0 => {
                let (mut elem, mut len) = (0, 0);
                fields(b, |m, b| {
                    match m {
                        0 => {
                            common(b)?;
                        }
                        1 => elem = b.int()?,
                        2 => len = b.int()?,
                        _ => bail!("gob: arrayType field {m}"),
                    }
                    Ok(())
                })?;
                WireType::Array { elem, len }
            }
            1 => {
                let mut elem = 0;
                fields(b, |m, b| {
                    match m {
                        0 => {
                            common(b)?;
                        }
                        1 => elem = b.int()?,
                        _ => bail!("gob: sliceType field {m}"),
                    }
                    Ok(())
                })?;
                WireType::Slice { elem }
            }
            2 => {
                let (mut name, mut fs) = (String::new(), Vec::new());
                fields(b, |m, b| {
                    match m {
                        0 => name = common(b)?,
                        1 => {
                            for _ in 0..b.uint()? {
                                let (mut fname, mut fid) = (String::new(), 0);
                                fields(b, |k, b| {
                                    match k {
                                        0 => fname = String::from_utf8(b.bytes()?.to_vec())?,
                                        1 => fid = b.int()?,
                                        _ => bail!("gob: fieldType field {k}"),
                                    }
                                    Ok(())
                                })?;
                                fs.push((fname, fid));
                            }
                        }
                        _ => bail!("gob: structType field {m}"),
                    }
                    Ok(())
                })?;
                WireType::Struct { name, fields: fs }
            }
            3 => {
                let (mut key, mut elem) = (0, 0);
                fields(b, |m, b| {
                    match m {
                        0 => {
                            common(b)?;
                        }
                        1 => key = b.int()?,
                        2 => elem = b.int()?,
                        _ => bail!("gob: mapType field {m}"),
                    }
                    Ok(())
                })?;
                WireType::Map { key, elem }
            }
            4..=6 => {
                fields(b, |m, b| {
                    ensure!(m == 0, "gob: gobEncoderType field {m}");
                    common(b)?;
                    Ok(())
                })?;
                match n {
                    4 => WireType::GobEncoder,
                    5 => WireType::BinaryMarshaler,
                    _ => WireType::TextMarshaler,
                }
            }
            _ => bail!("gob: wireType field {n}"),
        });
        Ok(())
    })?;
    out.ok_or_else(|| anyhow!("gob: empty wireType"))
}

/// Split a byte stream into delimited messages.
pub fn split_messages(mut s: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    while !s.is_empty() {
        let mut b = Buf::new(s);
        let n = usize::try_from(b.uint()?)?;
        let rest = b.b;
        ensure!(n <= rest.len(), "gob: truncated message");
        out.push(&rest[..n]);
        s = &rest[n..];
    }
    Ok(out)
}

// ─── encoder ───────────────────────────────────────────────────────────────────────────────

/// The schema of a value the spike sends.
#[derive(Debug, Clone)]
pub enum Ty {
    Bool,
    Uint,
    Int,
    String,
    Struct(&'static str, Vec<(&'static str, Ty)>),
}

#[derive(Debug, Clone)]
pub enum Val {
    Bool(bool),
    Uint(u64),
    Int(i64),
    String(String),
    /// Field values in schema order; `None` for a nil pointer field.
    Struct(Vec<Option<Val>>),
}

pub fn put_uint(out: &mut Vec<u8>, u: u64) {
    if u < 0x80 {
        out.push(u as u8);
        return;
    }
    let be = u.to_be_bytes();
    let skip = be.iter().take_while(|&&x| x == 0).count();
    out.push((-((8 - skip) as i8)) as u8);
    out.extend_from_slice(&be[skip..]);
}

pub fn put_int(out: &mut Vec<u8>, i: i64) {
    put_uint(
        out,
        if i < 0 {
            (!(i as u64)) << 1 | 1
        } else {
            (i as u64) << 1
        },
    );
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uint(out, b.len() as u64);
    out.extend_from_slice(b);
}

#[derive(Default)]
pub struct Encoder {
    sent: HashMap<&'static str, i64>,
    next: i64,
}

impl Encoder {
    /// Encode one value as the sequence of delimited messages Go's Encoder.Encode would emit.
    pub fn encode(&mut self, ty: &Ty, v: &Val) -> Vec<u8> {
        let mut out = Vec::new();
        let id = self.define(ty, &mut out);
        let mut body = Vec::new();
        put_int(&mut body, id);
        match ty {
            Ty::Struct(..) => self.value(ty, v, &mut body, false),
            _ => {
                put_uint(&mut body, 0);
                self.value(ty, v, &mut body, true);
            }
        }
        put_bytes(&mut out, &body);
        out
    }

    fn define(&mut self, ty: &Ty, out: &mut Vec<u8>) -> i64 {
        match ty {
            Ty::Bool => T_BOOL,
            Ty::Uint => T_UINT,
            Ty::Int => T_INT,
            Ty::String => T_STRING,
            Ty::Struct(name, fs) => {
                if let Some(&id) = self.sent.get(name) {
                    return id;
                }
                let field_ids: Vec<i64> = fs.iter().map(|(_, t)| self.define(t, out)).collect();
                if self.next == 0 {
                    self.next = FIRST_USER_ID;
                }
                let id = self.next;
                self.next += 1;
                self.sent.insert(name, id);
                // wireType{StructT: &structType{CommonType{Name, Id}, Field: [...]}}
                let mut m = Vec::new();
                put_int(&mut m, -id);
                put_uint(&mut m, 3); // field delta to StructT (field 2)
                put_uint(&mut m, 1); // CommonType
                put_uint(&mut m, 1);
                put_bytes(&mut m, name.as_bytes());
                put_uint(&mut m, 1);
                put_int(&mut m, id);
                put_uint(&mut m, 0);
                if !fs.is_empty() {
                    put_uint(&mut m, 1); // Field
                    put_uint(&mut m, fs.len() as u64);
                    for ((fname, _), fid) in fs.iter().zip(field_ids) {
                        put_uint(&mut m, 1);
                        put_bytes(&mut m, fname.as_bytes());
                        put_uint(&mut m, 1);
                        put_int(&mut m, fid);
                        put_uint(&mut m, 0);
                    }
                }
                put_uint(&mut m, 0); // end structType
                put_uint(&mut m, 0); // end wireType
                put_bytes(out, &m);
                id
            }
        }
    }

    /// Returns without writing anything for a zero scalar unless `send_zero`.
    fn value(&mut self, ty: &Ty, v: &Val, out: &mut Vec<u8>, send_zero: bool) {
        match (ty, v) {
            (Ty::Bool, Val::Bool(x)) => put_uint(out, u64::from(*x)),
            (Ty::Uint, Val::Uint(x)) => put_uint(out, *x),
            (Ty::Int, Val::Int(x)) => put_int(out, *x),
            (Ty::String, Val::String(s)) => put_bytes(out, s.as_bytes()),
            (Ty::Struct(_, fs), Val::Struct(vals)) => {
                let _ = send_zero;
                let mut last: i64 = -1;
                for (i, ((_, fty), fv)) in fs.iter().zip(vals).enumerate() {
                    let Some(fv) = fv else { continue };
                    let zero = match fv {
                        Val::Bool(x) => !x,
                        Val::Uint(x) => *x == 0,
                        Val::Int(x) => *x == 0,
                        Val::String(s) => s.is_empty(),
                        Val::Struct(_) => false,
                    };
                    if zero {
                        continue;
                    }
                    put_uint(out, (i as i64 - last) as u64);
                    last = i as i64;
                    self.value(fty, fv, out, false);
                }
                put_uint(out, 0);
            }
            _ => panic!("spike encoder: value does not match schema"),
        }
    }
}
