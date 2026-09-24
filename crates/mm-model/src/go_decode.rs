//! A `Deserializer` adapter that refuses a JSON array for a struct, at every depth.
//!
//! serde's derived `Deserialize` for a struct accepts a *sequence* as well as a map, reading the
//! fields positionally, so `[]` decodes into a struct with every field at its default and
//! `["u","pw"]` sets its first two fields. `encoding/json` answers `cannot unmarshal array into Go
//! value` — at the top level, and for a nested struct field, where Go records the error and the
//! caller's `Decode` still fails. [D-941] found the top-level half on the wire: a handler that
//! accepted `[]` walked on to a later validation branch and named a field where Go names the body.
//!
//! [`Strict`] wraps any deserializer and forwards every call unchanged, except that the visitor
//! it hands to `deserialize_struct` (and to a struct variant) rejects `visit_seq`. Every nested
//! value is reached through a wrapped `MapAccess`/`SeqAccess`/`EnumAccess`, so the rule holds
//! below the top level too. A `Vec`, a tuple or a map is untouched: they call `deserialize_seq`,
//! `deserialize_tuple` or `deserialize_map`, never `deserialize_struct`.
//!
//! It also makes a repeated key in a struct **last-wins**, as `encoding/json` does; serde's derive
//! refuses such a document with `duplicate field`. A repeated key that names a nested struct or
//! map is replaced whole here and merged in Go — a shape no client sends. The buffer is a
//! `serde_json::Value`, so a struct member read as a `RawValue` comes back re-serialised, and an
//! integer wider than 64 bits has already become an `f64`: no request body this server decodes
//! holds either (the `*big.Int` fields in `system.rs` are Systems rows, read without this).
//!
//! What it cannot reach: a struct under `#[serde(flatten)]` or inside an untagged enum, which serde
//! buffers into its private `Content` and decodes from that. Neither shape is in a request body
//! this server decodes.

use serde::de::{self, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess, Visitor};
use std::fmt;
use std::marker::PhantomData;

/// The text `encoding/json` uses; it never reaches the wire, since every caller maps a decode
/// failure to its own `AppError`.
const ARRAY_INTO_STRUCT: &str = "json: cannot unmarshal array into Go value";

/// See the module docs.
pub(crate) struct Strict<D>(pub(crate) D);

struct StrictSeed<S>(S);

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for StrictSeed<S> {
    type Value = S::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<S::Value, D::Error> {
        self.0.deserialize(Strict(deserializer))
    }
}

struct StrictVisitor<V> {
    inner: V,
    /// Set by `deserialize_struct` and `struct_variant`: a sequence here is Go's array-into-struct
    /// error.
    is_struct: bool,
}

impl<V> StrictVisitor<V> {
    fn any(inner: V) -> Self {
        StrictVisitor {
            inner,
            is_struct: false,
        }
    }

    fn structure(inner: V) -> Self {
        StrictVisitor {
            inner,
            is_struct: true,
        }
    }
}

macro_rules! forward_visit {
    ($($method:ident($ty:ty)),* $(,)?) => {
        $(
            fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
                self.inner.$method(value)
            }
        )*
    };
}

impl<'de, V: Visitor<'de>> Visitor<'de> for StrictVisitor<V> {
    type Value = V::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.expecting(formatter)
    }

    forward_visit!(
        visit_bool(bool),
        visit_i8(i8),
        visit_i16(i16),
        visit_i32(i32),
        visit_i64(i64),
        visit_i128(i128),
        visit_u8(u8),
        visit_u16(u16),
        visit_u32(u32),
        visit_u64(u64),
        visit_u128(u128),
        visit_f32(f32),
        visit_f64(f64),
        visit_char(char),
        visit_str(&str),
        visit_borrowed_str(&'de str),
        visit_string(String),
        visit_bytes(&[u8]),
        visit_borrowed_bytes(&'de [u8]),
        visit_byte_buf(Vec<u8>),
    );

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.inner.visit_some(Strict(deserializer))
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        self.inner.visit_newtype_struct(Strict(deserializer))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        if self.is_struct {
            return Err(de::Error::custom(ARRAY_INTO_STRUCT));
        }
        self.inner.visit_seq(StrictSeq(seq))
    }

    /// A struct's members are buffered first, so a repeated key is **last-wins** as in Go, where
    /// serde's derive refuses the document with `duplicate field`. Each value is still read
    /// through the adapter, and the buffer is handed back through it, so the array rule holds on
    /// both sides of the buffer.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        if !self.is_struct {
            return self.inner.visit_map(StrictMap(map));
        }
        let mut members = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(StrictSeed(PhantomData::<serde_json::Value>))?;
            members.insert(key, value);
        }
        let buffered = de::value::MapDeserializer::<_, serde_json::Error>::new(members.into_iter());
        self.inner
            .visit_map(StrictMap(buffered))
            .map_err(de::Error::custom)
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_enum(StrictEnum(data))
    }
}

struct StrictSeq<A>(A);

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for StrictSeq<A> {
    type Error = A::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, A::Error> {
        self.0.next_element_seed(StrictSeed(seed))
    }

    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}

struct StrictMap<A>(A);

impl<'de, A: MapAccess<'de>> MapAccess<'de> for StrictMap<A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        self.0.next_key_seed(StrictSeed(seed))
    }

    fn next_value_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<T::Value, A::Error> {
        self.0.next_value_seed(StrictSeed(seed))
    }

    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}

struct StrictEnum<A>(A);

impl<'de, A: EnumAccess<'de>> EnumAccess<'de> for StrictEnum<A> {
    type Error = A::Error;
    type Variant = StrictVariant<A::Variant>;

    fn variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<(T::Value, Self::Variant), A::Error> {
        self.0
            .variant_seed(StrictSeed(seed))
            .map(|(value, variant)| (value, StrictVariant(variant)))
    }
}

struct StrictVariant<A>(A);

impl<'de, A: de::VariantAccess<'de>> de::VariantAccess<'de> for StrictVariant<A> {
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        self.0.unit_variant()
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, A::Error> {
        self.0.newtype_variant_seed(StrictSeed(seed))
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, A::Error> {
        self.0.tuple_variant(len, StrictVisitor::any(visitor))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error> {
        self.0
            .struct_variant(fields, StrictVisitor::structure(visitor))
    }
}

macro_rules! forward_deserialize {
    ($($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
                self.0.$method(StrictVisitor::any(visitor))
            }
        )*
    };
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Strict<D> {
    type Error = D::Error;

    forward_deserialize!(
        deserialize_any,
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_option,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
        deserialize_ignored_any,
    );

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0
            .deserialize_unit_struct(name, StrictVisitor::any(visitor))
    }

    /// The name is passed through untouched: serde_json's `RawValue` is recognised by it.
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0
            .deserialize_newtype_struct(name, StrictVisitor::any(visitor))
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_tuple(len, StrictVisitor::any(visitor))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0
            .deserialize_tuple_struct(name, len, StrictVisitor::any(visitor))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0
            .deserialize_struct(name, fields, StrictVisitor::structure(visitor))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0
            .deserialize_enum(name, variants, StrictVisitor::any(visitor))
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
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
        #[serde(rename = "raw")]
        raw: Option<Box<serde_json::value::RawValue>>,
    }

    fn strict<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, serde_json::Error> {
        let mut deserializer = serde_json::Deserializer::from_str(json);
        T::deserialize(Strict(&mut deserializer))
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
            let err = strict::<Outer>(json).expect_err(json);
            assert!(err.to_string().contains(ARRAY_INTO_STRUCT), "{json}: {err}");
        }
    }

    #[test]
    fn everything_else_decodes_as_serde_alone_decodes_it() {
        let json = r#"{"name":"n","inner":{"a":"i"},"maybe":null,"list":[{"a":"l"}],
            "tags":["t"],"props":{"k":{"a":"p"}},"raw":[1, 2]}"#;
        let plain: Outer = serde_json::from_str(json).unwrap();
        let wrapped: Outer = strict(json).unwrap();
        assert_eq!(wrapped.name, plain.name);
        assert_eq!(wrapped.inner, plain.inner);
        assert_eq!(wrapped.maybe, None);
        assert_eq!(wrapped.list, plain.list);
        assert_eq!(wrapped.tags, vec!["t"]);
        assert_eq!(wrapped.props, plain.props);
        // `RawValue` is reached through `deserialize_newtype_struct`'s magic name — but inside a
        // struct it is re-serialised from the buffer, so its bytes are compact, not the body's.
        assert_eq!(
            wrapped.raw.map(|raw| raw.get().to_owned()).as_deref(),
            Some("[1,2]")
        );

        // A non-struct target takes its array as before, and `Value` takes anything.
        assert_eq!(strict::<Vec<String>>(r#"["a"]"#).unwrap(), vec!["a"]);
        assert_eq!(strict::<(i64, i64)>("[1,2]").unwrap(), (1, 2));
        assert_eq!(
            strict::<serde_json::Value>(r#"[{"a":[]}]"#).unwrap(),
            serde_json::json!([{"a":[]}])
        );
        assert_eq!(strict::<Option<Inner>>("null").unwrap(), None);
        // A wrong type is still serde's own error, not the array one.
        let err = strict::<Inner>("7").unwrap_err();
        assert!(!err.to_string().contains(ARRAY_INTO_STRUCT), "{err}");
    }

    #[test]
    fn a_repeated_key_is_last_wins_at_every_depth() {
        let json = r#"{"name":"first","name":"second","inner":{"a":"x","a":"y"}}"#;
        assert!(
            serde_json::from_str::<Outer>(json).is_err(),
            "serde alone refuses it"
        );
        let outer: Outer = strict(json).unwrap();
        assert_eq!(outer.name, "second");
        assert_eq!(outer.inner.a, "y");
        // The buffer does not launder an array past the rule.
        assert!(strict::<Outer>(r#"{"name":"n","name":"m","inner":[]}"#).is_err());
        // A field of the wrong type is still an error after buffering.
        assert!(strict::<Outer>(r#"{"name":7}"#).is_err());
    }

    #[test]
    fn an_enum_struct_variant_refuses_an_array_and_the_others_do_not() {
        #[derive(Debug, PartialEq, Deserialize)]
        enum Shape {
            Unit,
            Newtype(Inner),
            Tuple(i64, i64),
            Struct { a: String },
        }
        assert_eq!(strict::<Shape>(r#""Unit""#).unwrap(), Shape::Unit);
        assert_eq!(
            strict::<Shape>(r#"{"Tuple":[1,2]}"#).unwrap(),
            Shape::Tuple(1, 2)
        );
        assert_eq!(
            strict::<Shape>(r#"{"Struct":{"a":"x"}}"#).unwrap(),
            Shape::Struct { a: "x".into() }
        );
        assert!(strict::<Shape>(r#"{"Struct":["x"]}"#).is_err());
        assert!(strict::<Shape>(r#"{"Newtype":[]}"#).is_err());
        assert!(serde_json::from_str::<Shape>(r#"{"Struct":["x"]}"#).is_ok());
    }
}
