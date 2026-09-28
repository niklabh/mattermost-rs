//! `encoding/json`'s decoding rules, as a `Deserializer` every body decoder in `utils` goes through.
//!
//! serde's derive and Go's `encoding/json` agree on well-formed, exactly-spelt documents and part
//! on five things a client can send. Each is reproduced here, and each is held to Go's own answers
//! by `fixtures/behaviour_body_decode.json`:
//!
//! 1. **An array is never a struct** ([D-941]). serde's derive reads a sequence positionally; Go
//!    answers `cannot unmarshal array into Go value`, at any depth.
//! 2. **`null` is not an error** ([D-057], [D-075]). Go's `literalStore` sets an interface,
//!    pointer, map or slice to nil and *ignores* `null` for everything else (decode.go:904) — a
//!    string, a number, a bool or a struct keeps the value it had: its zero value, or what an
//!    earlier occurrence of the same key wrote. A slice element or a map value is decoded into a
//!    fresh zero, so `[null]` into a `[]string` is `[""]`. In Rust terms: `Option`, `Vec`, maps
//!    and `serde_json::Value` take the nil; every other target takes its zero or keeps its value.
//! 3. **A key matches a field case-insensitively** ([D-040], [D-460]). Go tries the exact name,
//!    then `foldName(key)` against the folded names, the first declared field winning a collision
//!    (decode.go:699, encode.go:1306). The field names are the `fields` serde hands
//!    `deserialize_struct` — the `rename`s — and [`crate::go_json::fold_name`] is Go's fold, exact
//!    for the ASCII names every struct here has (`serde_default_guard` checks they are ASCII).
//! 4. **A repeated key is assigned again, in document order** ([D-071]). serde's derive refuses
//!    it (`duplicate field`). What "again" means is Go's per kind: a scalar is overwritten, `null`
//!    is ignored by a scalar and nils a pointer, a struct and a pointed-to struct are *merged* (the
//!    second object decodes into the first), a map is merged key by key, and a slice is decoded
//!    into element by element — `array` (decode.go:507) reuses the backing array and truncates, so
//!    `"t":["a","b"],"t":[null]` leaves `["a"]`, and a later, longer array exposes the elements a
//!    shorter one truncated away. [`At`] carries every occurrence of a field to the point where the
//!    target's kind is known and applies the rule there. Keys that fold together are the same
//!    field, so the last *spelling* wins as well.
//! 5. **Trailing bytes and lone surrogates** are the callers' (`decode_one_from_json` reads one
//!    value; `replace_lone_surrogates` runs first).
//!
//! The document is parsed once into [`GoJson`], which keeps object members **in order and with
//! duplicates** — `serde_json::Value` cannot, since its map is a `BTreeMap` — and then deserialized
//! from that.
//!
//! # What it does not reach
//!
//! - A struct with a `#[serde(flatten)]` member is decoded by serde as a *map*, so its fields are
//!   not known here: exact duplicates are last-wins and a top-level `null` member is dropped, but a
//!   folded key is not matched, a duplicate is not merged, and below the top level serde's
//!   buffered `Content` decides. The body types Go embeds into therefore write their
//!   `Deserialize` over [`embedded_document`] instead ([D-1240]); what still arrives here as a
//!   flattened map is a response type no handler decodes.
//! - A type with a hand-written `Deserialize` that goes through `serde_json::Value` sees the
//!   document after `Value` has collapsed it; the rules apply only down to that point.
//! - Of the earlier occurrences a repeated scalar key overwrites, only the type is checked, not
//!   every conversion Go attempts; Go reports the first mismatch after decoding the rest, and the
//!   `Decode` fails either way.
//! - `null` into a Rust enum is `""`, which only an enum with an empty variant accepts. No
//!   `Deserialize` enum is in a request body.
//! - A `RawValue` member comes back as compact JSON rather than the body's own bytes, and only in
//!   its owned (`Box<RawValue>`) form; an integer wider than 64 bits has become an `f64`, as it
//!   does in `serde_json::Value`.

use std::fmt;

use serde::de::value::{MapAccessDeserializer, MapDeserializer, SeqDeserializer};
use serde::de::{self, Deserialize, Deserializer, IntoDeserializer, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

use crate::go_json::fold_name;

/// The text `encoding/json` uses; it never reaches the wire, since every caller maps a decode
/// failure to its own `AppError`.
const ARRAY_INTO_STRUCT: &str = "json: cannot unmarshal array into Go value";

/// `serde_json::value::RawValue`'s private newtype name, which its `Deserialize` asks for.
const RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";

/// A parsed JSON document that keeps object members in order, duplicates included.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GoJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<GoJson>),
    Object(Vec<(String, GoJson)>),
}

impl GoJson {
    fn kind(&self) -> de::Unexpected<'_> {
        match self {
            GoJson::Null => de::Unexpected::Unit,
            GoJson::Bool(b) => de::Unexpected::Bool(*b),
            GoJson::Number(_) => de::Unexpected::Other("number"),
            GoJson::String(s) => de::Unexpected::Str(s),
            GoJson::Array(_) => de::Unexpected::Seq,
            GoJson::Object(_) => de::Unexpected::Map,
        }
    }
}

impl<'de> Deserialize<'de> for GoJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DocumentVisitor;

        impl<'de> Visitor<'de> for DocumentVisitor {
            type Value = GoJson;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E>(self) -> Result<GoJson, E> {
                Ok(GoJson::Null)
            }
            fn visit_none<E>(self) -> Result<GoJson, E> {
                Ok(GoJson::Null)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<GoJson, D::Error> {
                GoJson::deserialize(d)
            }
            fn visit_bool<E>(self, v: bool) -> Result<GoJson, E> {
                Ok(GoJson::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<GoJson, E> {
                Ok(GoJson::Number(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<GoJson, E> {
                Ok(GoJson::Number(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<GoJson, E> {
                serde_json::Number::from_f64(v)
                    .map(GoJson::Number)
                    .ok_or_else(|| E::custom("a JSON number is finite"))
            }
            fn visit_str<E>(self, v: &str) -> Result<GoJson, E> {
                Ok(GoJson::String(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<GoJson, E> {
                Ok(GoJson::String(v))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<GoJson, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(GoJson::Array(items))
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<GoJson, A::Error> {
                let mut members = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, GoJson>()? {
                    members.push((key, value));
                }
                Ok(GoJson::Object(members))
            }
        }

        deserializer.deserialize_any(DocumentVisitor)
    }
}

/// Compact JSON, members in their document order — what a `RawValue` member is handed.
impl Serialize for GoJson {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            GoJson::Null => s.serialize_unit(),
            GoJson::Bool(b) => s.serialize_bool(*b),
            GoJson::Number(n) => n.serialize(s),
            GoJson::String(v) => s.serialize_str(v),
            GoJson::Array(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            GoJson::Object(members) => {
                let mut map = s.serialize_map(Some(members.len()))?;
                for (key, value) in members {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

/// Every occurrence of one value, in document order — one for most values, several for a key an
/// object repeats (or folds together). The target's `deserialize_*` call says which kind it is,
/// and each method below applies Go's rule for repeating that kind. Never empty.
#[derive(Clone)]
pub(crate) struct At<'a>(Vec<&'a GoJson>);

impl<'a> At<'a> {
    pub(crate) fn one(value: &'a GoJson) -> Self {
        At(vec![value])
    }

    fn last(&self) -> &'a GoJson {
        self.0.last().copied().unwrap_or(&GoJson::Null)
    }

    /// The occurrences after the last `null`: a pointer, a map or a slice is nilled by `null`
    /// and starts again after it.
    fn since_last_null(&self) -> Vec<&'a GoJson> {
        let start = self
            .0
            .iter()
            .rposition(|value| matches!(value, GoJson::Null))
            .map_or(0, |i| i + 1);
        self.0[start..].to_vec()
    }

    /// The non-`null` occurrences: a scalar or a struct ignores `null`.
    fn non_null(&self) -> Vec<&'a GoJson> {
        self.0
            .iter()
            .copied()
            .filter(|value| !matches!(value, GoJson::Null))
            .collect()
    }

    /// One JSON value, as `serde_json::Value`'s own deserializer would hand it over.
    fn any<V: Visitor<'a>>(value: &'a GoJson, visitor: V) -> Result<V::Value, serde_json::Error> {
        match value {
            GoJson::Null => visitor.visit_unit(),
            GoJson::Bool(b) => visitor.visit_bool(*b),
            GoJson::Number(n) => {
                if let Some(u) = n.as_u64() {
                    visitor.visit_u64(u)
                } else if let Some(i) = n.as_i64() {
                    visitor.visit_i64(i)
                } else {
                    visitor.visit_f64(n.as_f64().unwrap_or_default())
                }
            }
            GoJson::String(s) => visitor.visit_borrowed_str(s),
            GoJson::Array(items) => {
                let mut seq = SeqDeserializer::new(items.iter().map(At::one));
                let out = visitor.visit_seq(&mut seq)?;
                seq.end()?;
                Ok(out)
            }
            GoJson::Object(members) => {
                let mut map = MapDeserializer::new(
                    members
                        .iter()
                        .map(|(key, value)| (key.as_str(), At::one(value))),
                );
                let out = visitor.visit_map(&mut map)?;
                map.end()?;
                Ok(out)
            }
        }
    }

    /// A scalar target: every non-`null` occurrence must fit `T` — Go reports a mismatch even when
    /// a later occurrence overwrites it — and the last one is the value. All `null` is `zero`.
    fn scalar<T, V>(
        self,
        visitor: V,
        zero: impl FnOnce(V) -> Result<V::Value, serde_json::Error>,
    ) -> Result<V::Value, serde_json::Error>
    where
        T: Deserialize<'a>,
        V: Visitor<'a>,
    {
        let present = self.non_null();
        let Some((last, earlier)) = present.split_last() else {
            return zero(visitor);
        };
        for value in earlier {
            T::deserialize(At::one(value))?;
        }
        Self::any(last, visitor)
    }
}

impl<'a> IntoDeserializer<'a, serde_json::Error> for At<'a> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

macro_rules! scalar_methods {
    ($($method:ident: $ty:ty => |$v:ident| $zero:expr),* $(,)?) => {
        $(
            fn $method<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Self::Error> {
                self.scalar::<$ty, V>(visitor, |$v| $zero)
            }
        )*
    };
}

impl<'a> Deserializer<'a> for At<'a> {
    type Error = serde_json::Error;

    /// An interface: each occurrence replaces the last, `null` included.
    fn deserialize_any<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        Self::any(self.last(), visitor)
    }

    scalar_methods!(
        deserialize_bool: bool => |v| v.visit_bool(false),
        deserialize_i8: i8 => |v| v.visit_i8(0),
        deserialize_i16: i16 => |v| v.visit_i16(0),
        deserialize_i32: i32 => |v| v.visit_i32(0),
        deserialize_i64: i64 => |v| v.visit_i64(0),
        deserialize_i128: i128 => |v| v.visit_i128(0),
        deserialize_u8: u8 => |v| v.visit_u8(0),
        deserialize_u16: u16 => |v| v.visit_u16(0),
        deserialize_u32: u32 => |v| v.visit_u32(0),
        deserialize_u64: u64 => |v| v.visit_u64(0),
        deserialize_u128: u128 => |v| v.visit_u128(0),
        deserialize_f32: f32 => |v| v.visit_f32(0.0),
        deserialize_f64: f64 => |v| v.visit_f64(0.0),
        deserialize_char: char => |v| v.visit_char('\0'),
        deserialize_str: String => |v| v.visit_borrowed_str(""),
        deserialize_string: String => |v| v.visit_borrowed_str(""),
        deserialize_bytes: serde_json::Value => |v| v.visit_borrowed_bytes(&[]),
        deserialize_byte_buf: serde_json::Value => |v| v.visit_borrowed_bytes(&[]),
        deserialize_identifier: String => |v| v.visit_borrowed_str(""),
    );

    /// A pointer: `null` nils it, and what follows decodes into a fresh pointee, merging as the
    /// pointee's own kind does.
    fn deserialize_option<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let rest = self.since_last_null();
        if rest.is_empty() {
            visitor.visit_none()
        } else {
            visitor.visit_some(At(rest))
        }
    }

    fn deserialize_unit<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        Self::any(self.last(), visitor)
    }

    fn deserialize_unit_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Self::any(self.last(), visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'a>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if name == RAW_VALUE_TOKEN {
            let text = serde_json::to_string(self.last())?;
            let entry = std::iter::once((RAW_VALUE_TOKEN, text));
            let mut map = MapDeserializer::<_, serde_json::Error>::new(entry);
            let out = visitor.visit_map(&mut map)?;
            map.end()?;
            return Ok(out);
        }
        visitor.visit_newtype_struct(self)
    }

    /// A slice: `null` nils it, `[]` makes a new empty one, and a non-empty array decodes into the
    /// existing backing array element by element and then truncates — so an element keeps what
    /// an earlier occurrence wrote there, including past a truncation.
    fn deserialize_seq<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let mut backing: Vec<Vec<&'a GoJson>> = Vec::new();
        let mut len = 0;
        for value in &self.0 {
            match value {
                GoJson::Null => {
                    backing.clear();
                    len = 0;
                }
                GoJson::Array(items) if items.is_empty() => {
                    backing.clear();
                    len = 0;
                }
                GoJson::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        match backing.get_mut(i) {
                            Some(slot) => slot.push(item),
                            None => backing.push(vec![item]),
                        }
                    }
                    len = items.len();
                }
                other => {
                    return Err(de::Error::invalid_type(other.kind(), &"an array"));
                }
            }
        }
        backing.truncate(len);
        let mut seq = SeqDeserializer::new(backing.into_iter().map(At));
        let out = visitor.visit_seq(&mut seq)?;
        seq.end()?;
        Ok(out)
    }

    fn deserialize_tuple<V: Visitor<'a>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    /// A map: `null` nils it, and each object after that adds its keys to the same map; a key
    /// is decoded into a fresh element each time, so within a map the last occurrence wins
    /// outright. Keys are exact — Go folds struct fields, never map keys.
    ///
    /// A struct with a `#[serde(flatten)]` member arrives here too, recognisable only by its
    /// visitor's `expecting` ("struct X"). Its fields are not known, so a folded key cannot be
    /// matched ([D-1240]); what can be done is Go's `null` rule one level down — a member whose
    /// last occurrence is `null` is dropped, which for a struct field is the same as `null`
    /// ignored (a scalar or a struct keeps its zero) or nilled (a pointer, map or slice stays
    /// nil). Deeper levels reach serde's buffered `Content`, where the rules do not apply.
    fn deserialize_map<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let flattened_struct = Expecting(&visitor).to_string().starts_with("struct ");
        let mut entries: Vec<(&'a str, &'a GoJson)> = Vec::new();
        for value in self.since_last_null() {
            let GoJson::Object(members) = value else {
                return Err(de::Error::invalid_type(value.kind(), &"an object"));
            };
            for (key, member) in members {
                match entries.iter_mut().find(|(seen, _)| *seen == key.as_str()) {
                    Some(entry) => entry.1 = member,
                    None => entries.push((key.as_str(), member)),
                }
            }
        }
        if flattened_struct {
            entries.retain(|(_, value)| !matches!(value, GoJson::Null));
        }
        let mut map = MapDeserializer::new(entries.into_iter().map(|(k, v)| (k, At::one(v))));
        let out = visitor.visit_map(&mut map)?;
        map.end()?;
        Ok(out)
    }

    /// A struct: `null` is ignored, an array is Go's array-into-struct error, and every object
    /// decodes into the same struct — each key resolved to a field exactly, then by fold, and all
    /// the occurrences that land on one field handed to it together.
    fn deserialize_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let folded: Vec<String> = fields.iter().map(|field| fold_name(field)).collect();
        let mut resolved: Vec<(&'a str, At<'a>)> = Vec::new();
        for value in self.non_null() {
            let members = match value {
                GoJson::Object(members) => members,
                GoJson::Array(_) => return Err(de::Error::custom(ARRAY_INTO_STRUCT)),
                other => return Err(de::Error::invalid_type(other.kind(), &"an object")),
            };
            for (key, member) in members {
                let target: &'a str = match fields.iter().find(|field| **field == key.as_str()) {
                    Some(field) => field,
                    None => {
                        let key_fold = fold_name(key);
                        folded
                            .iter()
                            .position(|fold| *fold == key_fold)
                            .map_or(key.as_str(), |i| fields[i])
                    }
                };
                match resolved.iter_mut().find(|(seen, _)| *seen == target) {
                    Some((_, at)) => at.0.push(member),
                    None => resolved.push((target, At::one(member))),
                }
            }
        }
        let mut map = MapDeserializer::new(resolved.into_iter());
        let out = visitor.visit_map(&mut map)?;
        map.end()?;
        Ok(out)
    }

    fn deserialize_enum<V: Visitor<'a>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.non_null().last().copied().unwrap_or(&GoJson::Null) {
            GoJson::Null => visitor.visit_enum("".into_deserializer()),
            GoJson::String(s) => visitor.visit_enum(s.as_str().into_deserializer()),
            GoJson::Object(members) if members.len() == 1 => {
                let map = MapDeserializer::new(
                    members
                        .iter()
                        .map(|(key, value)| (key.as_str(), At::one(value))),
                );
                visitor.visit_enum(MapAccessDeserializer::new(map))
            }
            other => Err(de::Error::invalid_type(other.kind(), &"an enum")),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }
}

/// A visitor's `expecting` text, which is how a derived struct with a flattened member — decoded
/// through `deserialize_map` — is told apart from a map.
struct Expecting<'v, V>(&'v V);

impl<'a, V: Visitor<'a>> fmt::Display for Expecting<'_, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.expecting(f)
    }
}

/// Deserialize `T` from a parsed document under Go's rules.
pub(crate) fn from_go_json<'a, T: Deserialize<'a>>(
    document: &'a GoJson,
) -> Result<T, serde_json::Error> {
    T::deserialize(At::one(document))
}

/// The document a type Go **embeds** another in is decoded from, once per part ([D-1240]).
///
/// `#[serde(flatten)]` makes serde decode the outer struct as a map and hand the embedded one
/// its members through a private buffer, so neither the field names (for the key fold) nor the
/// target kinds (for `null` and repeated keys) ever reach [`At`]. A type Go embeds into instead
/// writes its `Deserialize` as: buffer the object once with this, then decode **each part** — the
/// embedded struct, and a derived struct of the outer type's own fields — from the same document
/// with [`embedded_part`]. Each part ignores the other's keys as unknown, and gets every rule.
///
/// This is Go's answer as long as no key folds onto a field of two parts; Go would give it to the
/// shallower one. Each type's tests assert its parts' folded names are disjoint.
pub(crate) fn embedded_document<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<GoJson, D::Error> {
    GoJson::deserialize(deserializer)
}

/// One part of an [`embedded_document`], its error in the caller's error type.
pub(crate) fn embedded_part<T: serde::de::DeserializeOwned, E: de::Error>(
    document: &GoJson,
) -> Result<T, E> {
    from_go_json(document).map_err(E::custom)
}

/// The `fields` a derived struct hands `deserialize_struct` — its wire names, for the tests that
/// check an embedded type's parts do not share a folded name.
#[cfg(test)]
pub(crate) fn struct_fields<T: serde::de::DeserializeOwned>() -> &'static [&'static str] {
    struct Probe<'c>(&'c std::cell::Cell<&'static [&'static str]>);

    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = serde_json::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not a struct"))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            self.0.set(fields);
            Err(de::Error::custom("probed"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
            option unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier
            ignored_any
        }
    }

    let cell = std::cell::Cell::new(&[][..]);
    let _ = T::deserialize(Probe(&cell));
    cell.get()
}

/// Asserts no two parts of an embedded type have fields whose names fold together.
#[cfg(test)]
pub(crate) fn assert_disjoint_folds(parts: &[&[&str]]) {
    let mut seen: Vec<String> = Vec::new();
    for part in parts {
        assert!(
            !part.is_empty(),
            "a part has no fields — not a derived struct?"
        );
        for field in *part {
            let folded = fold_name(field);
            assert!(
                !seen.contains(&folded),
                "{field} folds onto a field of another part"
            );
            seen.push(folded);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Default, PartialEq, Deserialize)]
    #[serde(default)]
    struct Inner {
        a: String,
        n: i64,
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(default)]
    struct Outer {
        name: String,
        inner: Inner,
        maybe: Option<Inner>,
        list: Vec<Inner>,
        tags: Vec<String>,
        props: BTreeMap<String, Inner>,
        raw: Option<Box<serde_json::value::RawValue>>,
    }

    fn go<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, serde_json::Error> {
        let document: GoJson = serde_json::from_str(json)?;
        from_go_json::<T>(&document)
    }

    #[test]
    fn serde_alone_takes_an_array_for_a_struct() {
        // The premise: without the adapter, every one of these decodes.
        assert!(serde_json::from_str::<Outer>("[]").is_ok());
        assert!(serde_json::from_str::<Outer>(r#"{"inner":[]}"#).is_ok());
        assert!(serde_json::from_str::<Outer>(r#"["n"]"#).is_ok());
    }

    #[test]
    fn an_array_is_refused_for_a_struct_at_every_depth() {
        for json in [
            "[]",
            r#"["n"]"#,
            r#"{"inner":[]}"#,
            r#"{"maybe":["x"]}"#,
            r#"{"list":[{"a":"x"},[]]}"#,
            r#"{"props":{"k":[]}}"#,
        ] {
            let err = go::<Outer>(json).expect_err(json);
            assert!(err.to_string().contains(ARRAY_INTO_STRUCT), "{json}: {err}");
        }
    }

    #[test]
    fn everything_else_decodes_as_serde_alone_decodes_it() {
        let json = r#"{"name":"n","inner":{"a":"i"},"maybe":null,"list":[{"a":"l"}],
            "tags":["t"],"props":{"k":{"a":"p"}},"raw":[1, 2]}"#;
        let plain: Outer = serde_json::from_str(json).unwrap();
        let ours: Outer = go(json).unwrap();
        assert_eq!(ours.name, plain.name);
        assert_eq!(ours.inner, plain.inner);
        assert_eq!(ours.maybe, None);
        assert_eq!(ours.list, plain.list);
        assert_eq!(ours.tags, vec!["t"]);
        assert_eq!(ours.props, plain.props);
        // `RawValue` is re-serialised from the parsed document, so its bytes are compact.
        assert_eq!(
            ours.raw.map(|raw| raw.get().to_owned()).as_deref(),
            Some("[1,2]")
        );

        assert_eq!(go::<Vec<String>>(r#"["a"]"#).unwrap(), vec!["a"]);
        assert_eq!(go::<(i64, i64)>("[1,2]").unwrap(), (1, 2));
        assert_eq!(
            go::<serde_json::Value>(r#"[{"a":[]}]"#).unwrap(),
            serde_json::json!([{"a":[]}])
        );
        assert_eq!(go::<Option<Inner>>("null").unwrap(), None);
        assert!(go::<Inner>("7").is_err());
        assert!(go::<Inner>(r#"{"n":"7"}"#).is_err());
        assert!(go::<Inner>(r#"{"n":1.5}"#).is_err());
        assert!(go::<Inner>(r#"{"a":7}"#).is_err());
    }

    #[test]
    fn null_is_nil_for_a_pointer_map_slice_or_interface_and_ignored_otherwise() {
        let o: Outer = go(r#"{"name":null,"inner":null,"list":null,"tags":[null,"b"]}"#).unwrap();
        assert_eq!(o.name, "");
        assert_eq!(o.inner, Inner::default());
        assert!(o.list.is_empty());
        assert_eq!(o.tags, vec!["", "b"]);
        let o: Outer = go(r#"{"list":[null,{"a":"x"}],"props":{"k":null}}"#).unwrap();
        assert_eq!(o.list[0], Inner::default());
        assert_eq!(o.props["k"], Inner::default());
        // A previously written value survives a later `null` for a scalar, not for a pointer.
        let o: Outer = go(r#"{"name":"a","name":null,"maybe":{"a":"m"},"maybe":null}"#).unwrap();
        assert_eq!(o.name, "a");
        assert_eq!(o.maybe, None);
        assert_eq!(go::<i64>("null").unwrap(), 0);
        assert!(!go::<bool>("null").unwrap());
    }

    #[test]
    fn keys_fold_exact_first_and_the_last_spelling_wins() {
        let o: Outer = go(r#"{"NAME":"a"}"#).unwrap();
        assert_eq!(o.name, "a");
        let o: Outer = go(r#"{"name":"a","NAME":"b"}"#).unwrap();
        assert_eq!(o.name, "b");
        let o: Outer = go(r#"{"NAME":"b","name":"a"}"#).unwrap();
        assert_eq!(o.name, "a");
        // U+212A KELVIN SIGN and U+017F LONG S fold into ASCII in Go.
        let o: Outer = go("{\"inner\":{\"A\":\"k\",\"\u{212A}\":1}}").unwrap();
        assert_eq!(o.inner.a, "k");
        let o: Outer = go("{\"TAG\u{017F}\":[\"s\"]}").unwrap();
        assert_eq!(o.tags, vec!["s"]);
        // Map keys are never folded.
        let o: Outer = go(r#"{"props":{"K":{"a":"x"}}}"#).unwrap();
        assert!(o.props.contains_key("K") && !o.props.contains_key("k"));
    }

    #[test]
    fn a_flattened_struct_takes_a_top_level_null_and_a_map_keeps_it() {
        #[derive(Debug, Default, Deserialize)]
        #[serde(default)]
        struct Base {
            a: String,
        }
        #[derive(Debug, Default, Deserialize)]
        #[serde(default)]
        struct Flat {
            #[serde(flatten)]
            base: Base,
            n: i64,
        }
        assert!(serde_json::from_str::<Flat>(r#"{"a":null,"n":null}"#).is_err());
        let flat: Flat = go(r#"{"a":null,"n":null}"#).unwrap();
        assert_eq!((flat.base.a.as_str(), flat.n), ("", 0));
        let flat: Flat = go(r#"{"a":"x","a":"y","n":1}"#).unwrap();
        assert_eq!(flat.base.a, "y");
        let map: BTreeMap<String, serde_json::Value> = go(r#"{"a":null}"#).unwrap();
        assert_eq!(map["a"], serde_json::Value::Null);
    }

    /// Every body type that embeds another decodes each part from the same object, which is
    /// Go's answer only while no key folds onto fields of two parts.
    #[test]
    fn the_embedding_body_types_have_disjoint_parts() {
        use crate::go_decode::{assert_disjoint_folds, struct_fields};
        assert_disjoint_folds(&[
            struct_fields::<crate::draft::Draft>(),
            struct_fields::<crate::scheduled_post::ScheduledPostOwn>(),
        ]);
        assert_disjoint_folds(&[
            struct_fields::<crate::sidebar_category::SidebarCategory>(),
            struct_fields::<crate::sidebar_category::SidebarCategoryWithChannelsOwn>(),
        ]);
        assert_disjoint_folds(&[
            struct_fields::<crate::group::Group>(),
            struct_fields::<crate::group::GroupWithUserIdsOwn>(),
        ]);
        assert_disjoint_folds(&[
            struct_fields::<crate::data_retention_policy::RetentionPolicy>(),
            struct_fields::<crate::data_retention_policy::RetentionPolicyWithTeamAndChannelIDsOwn>(
            ),
        ]);
        assert_disjoint_folds(&[
            struct_fields::<crate::post_rest::ReportPostOptions>(),
            struct_fields::<crate::post_rest::ReportPostOptionsCursor>(),
        ]);
    }

    #[test]
    fn a_repeated_key_is_assigned_again_by_its_kind() {
        // Structs and pointed-to structs merge.
        let o: Outer = go(r#"{"inner":{"a":"x"},"inner":{"n":2}}"#).unwrap();
        assert_eq!(
            o.inner,
            Inner {
                a: "x".into(),
                n: 2
            }
        );
        let o: Outer = go(r#"{"maybe":{"a":"x"},"MAYBE":{"n":2}}"#).unwrap();
        assert_eq!(
            o.maybe,
            Some(Inner {
                a: "x".into(),
                n: 2
            })
        );
        // Slices decode element-wise into the backing array.
        let o: Outer = go(r#"{"tags":["a","b"],"tags":[null]}"#).unwrap();
        assert_eq!(o.tags, vec!["a"]);
        let o: Outer = go(r#"{"tags":["a","b"],"tags":["x"],"tags":[null,null]}"#).unwrap();
        assert_eq!(o.tags, vec!["x", "b"]);
        let o: Outer = go(r#"{"tags":["a","b"],"tags":[],"tags":[null,null]}"#).unwrap();
        assert_eq!(o.tags, vec!["", ""]);
        // Maps merge by key, and a key's value is replaced, not merged.
        let o: Outer = go(r#"{"props":{"k":{"a":"x"},"j":{}},"props":{"k":{"n":1}}}"#).unwrap();
        assert_eq!(
            o.props["k"],
            Inner {
                a: String::new(),
                n: 1
            }
        );
        assert!(o.props.contains_key("j"));
        // An earlier mistyped occurrence is still an error.
        assert!(go::<Outer>(r#"{"name":7,"name":"a"}"#).is_err());
        assert!(go::<Outer>(r#"{"inner":[],"inner":{}}"#).is_err());
    }
}
