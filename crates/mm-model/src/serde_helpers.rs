//! `skip_serializing_if` predicates for Go's `omitempty`.
//!
//! Go's `omitempty` drops a field when it holds the type's zero value: `false`, `0`, `""`, and a
//! nil *or empty* slice/map. Per CLAUDE.md a non-pointer Go field with `omitempty` stays a
//! concrete type here plus one of these predicates — turning it into `Option` would change what a
//! decoder can round-trip.
//!
//! Every predicate takes `&T` because that is the signature serde's derive calls with. Only the
//! shapes actually in use live here: a helper with no call site is speculative code, and the
//! `omitempty` variant a future field needs is one line to add.

use std::collections::BTreeMap;

pub fn is_false(b: &bool) -> bool {
    !*b
}

pub fn is_empty_str(s: &str) -> bool {
    s.is_empty()
}

pub fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

pub fn is_empty_map<K, V>(m: &BTreeMap<K, V>) -> bool {
    m.is_empty()
}

/// The ordinary `*T` + `omitempty` case: dropped only when the pointer is nil.
pub fn is_none<T>(v: &Option<T>) -> bool {
    v.is_none()
}

/// Go's `omitempty` on a **nil** slice behind a pointer: `None` is dropped, but so is
/// `Some(empty)` — because Go's check is `len() == 0`, not `== nil`.
pub fn is_none_or_empty_vec<T>(v: &Option<Vec<T>>) -> bool {
    match v {
        None => true,
        Some(inner) => inner.is_empty(),
    }
}

/// The same for a `map[string]any` behind an `Option`.
pub fn is_none_or_empty_map(m: &Option<serde_json::Map<String, serde_json::Value>>) -> bool {
    match m {
        None => true,
        Some(inner) => inner.is_empty(),
    }
}

/// The same for a `StringMap` behind an `Option`.
pub fn is_none_or_empty_string_map(m: &Option<BTreeMap<String, String>>) -> bool {
    match m {
        None => true,
        Some(inner) => inner.is_empty(),
    }
}

/// A `float64` field rendered the way Go's `encoding/json` renders one.
///
/// serde_json writes `65.0` where Go writes `65`, and the difference reaches the wire. The crate
/// already had this problem once, in `analytics_row.rs`, and solved it by hand-writing
/// `Serialize`; this is the same solution packaged as a field codec, for the several `float64`s
/// the 2026-08-24 sweep added.
///
/// Use as `#[serde(rename = "…", with = "crate::serde_helpers::go_float")]`.
pub mod go_float {
    use serde::de::{Deserialize, Deserializer};
    use serde::ser::{Error as SerError, Serializer};

    pub fn serialize<S: Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
        let rendered = crate::utils::go_json_format_float(*value).ok_or_else(|| {
            // Go's own text for a value `encoding/json` refuses: NaN and the two infinities.
            S::Error::custom(format!(
                "json: unsupported value: {}",
                crate::utils::go_format_float(*value)
            ))
        })?;
        // There is no serializer method for "a numeric token I have already formatted", and
        // `serialize_f64` would hand the value back to the encoder being replaced.
        let number = serde_json::value::RawValue::from_string(rendered)
            .map_err(|e| S::Error::custom(format!("json: unsupported value: {e}")))?;
        serde::Serialize::serialize(&number, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        f64::deserialize(d)
    }
}
