//! Go's `encoding/json` treatment of `[]byte`, as a reusable serde codec.
//!
//! Go special-cases `[]byte` and emits **base64** (standard alphabet, padded); serde_json would
//! emit an array of numbers, so `[1,2,3]` where Go writes `"AQID"`. A nil slice — and a nil
//! `*[]byte` — writes `null`; an empty non-nil slice writes `""`. Because Go cannot tell "no
//! pointer" from "pointer to nil slice" on the wire, both decode back to `None` here, and only
//! `""` round-trips as `Some(vec![])`.
//!
//! This is the same codec `file_info.rs` carries privately for `mini_preview`; it lives here so
//! the plugin KV, cluster-message and remote-cluster ports do not each grow a copy.
//!
//! Use it as `#[serde(rename = "…", with = "crate::go_bytes")]` on an `Option<Vec<u8>>`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serializer};

pub fn serialize<S: Serializer>(value: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(bytes) => s.serialize_str(&STANDARD.encode(bytes)),
        None => s.serialize_none(),
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
    let Some(encoded) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    STANDARD
        .decode(encoded.as_bytes())
        .map(Some)
        .map_err(serde::de::Error::custom)
}
