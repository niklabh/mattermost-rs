//! Port of `model/integrity.go` — the relational-integrity checker's result shape.
//!
//! # The `err` key is write-only, and lossy
//!
//! `IntegrityCheckResult.Err` is a Go `error` **interface**. `encoding/json` marshals whatever is
//! behind it, and the usual value is `errors.errorString`, whose single field is unexported — so
//! a failed check writes `"err":{}` and the message never reaches the client. The hand-written
//! `UnmarshalJSON` then reads `err` back as a **string**. The type is therefore asymmetric by
//! construction: what it writes is not what it reads.
//!
//! Both halves are reproduced here rather than harmonised, because harmonising either one is a
//! wire change.
//!
//! # `UnmarshalJSON` type-asserts its way through the payload
//!
//! Go's implementation is a chain of unchecked `.(string)` and `.([]any)` assertions on a
//! `map[string]any`: any wrong-typed field **panics**. Serde returns a typed error for the same
//! input instead — the only reachable difference is that a malformed body is an `Err` here and a
//! crash there ([D-052]-style divergence, and the safe direction).

use serde::de::Deserializer;
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

/// Port of `model.OrphanedRecord` (integrity.go:11).
///
/// Both fields are `*string` with **no** `omitempty`, so an absent id is `null` on the wire and
/// the key is always present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrphanedRecord {
    #[serde(rename = "parent_id")]
    pub parent_id: Option<String>,

    #[serde(rename = "child_id")]
    pub child_id: Option<String>,
}

/// Port of `model.RelationalIntegrityCheckData` (integrity.go:16).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelationalIntegrityCheckData {
    #[serde(rename = "parent_name")]
    pub parent_name: String,

    #[serde(rename = "child_name")]
    pub child_name: String,

    #[serde(rename = "parent_id_attr")]
    pub parent_id_attr: String,

    #[serde(rename = "child_id_attr")]
    pub child_id_attr: String,

    #[serde(rename = "records")]
    pub records: Vec<OrphanedRecord>,
}

/// Port of `model.IntegrityCheckResult` (integrity.go:24).
///
/// `Data` is a bare `any` in Go, but the only value ever placed in it — and the only shape
/// `UnmarshalJSON` can read — is a [`RelationalIntegrityCheckData`], so it is typed here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrityCheckResult {
    pub data: Option<RelationalIntegrityCheckData>,
    /// The message is **not** written to the wire; see the module docs.
    pub err: Option<String>,
}

impl Serialize for IntegrityCheckResult {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("IntegrityCheckResult", 2)?;
        st.serialize_field("data", &self.data)?;
        match &self.err {
            // `errors.errorString` has no exported fields: Go writes an empty object.
            Some(_) => st.serialize_field("err", &serde_json::Map::new())?,
            None => st.serialize_field("err", &())?,
        }
        st.end()
    }
}

/// The shape Go's `UnmarshalJSON` actually reads — `err` as a **string**, not as the object
/// [`Serialize`] writes.
#[derive(Deserialize)]
struct IntegrityCheckResultWire {
    #[serde(default)]
    data: Option<RelationalIntegrityCheckData>,
    #[serde(default)]
    err: Option<String>,
}

impl<'de> Deserialize<'de> for IntegrityCheckResult {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = IntegrityCheckResultWire::deserialize(d)?;
        Ok(IntegrityCheckResult {
            data: wire.data,
            err: wire.err,
        })
    }
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn orphaned_record_round_trips_the_fixture() {
        assert_fixture_round_trips!(OrphanedRecord, "orphaned_record");
    }
    #[test]
    fn relational_integrity_check_data_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            RelationalIntegrityCheckData,
            "relational_integrity_check_data"
        );
    }
    #[test]
    fn integrity_check_result_round_trips_the_fixture() {
        assert_fixture_round_trips!(IntegrityCheckResult, "integrity_check_result");
    }
}
