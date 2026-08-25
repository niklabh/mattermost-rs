//! Port of `model/property_group.go` — the namespace a set of property fields lives in.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, is_valid_id, new_id};

/// Port of `model.AccessControlPropertyGroupName` (property_group.go:11).
pub const ACCESS_CONTROL_PROPERTY_GROUP_NAME: &str = "access_control";

/// Port of `model.DeprecatedCPAPropertyGroupName` (property_group.go:17) — the old name for the
/// same group. The plugin API still accepts it.
pub const DEPRECATED_CPA_PROPERTY_GROUP_NAME: &str = "custom_profile_attributes";

/// Port of `model.AccessControlGroupFieldLimit` (property_group.go:26).
///
/// **This is load-bearing beyond being a cap.** Call sites read every field and value in one page
/// of `AccessControlGroupFieldLimit + 5` instead of paginating, on the assumption that the result
/// set is bounded by it. Raising it without converting those call sites silently truncates.
pub const ACCESS_CONTROL_GROUP_FIELD_LIMIT: i64 = 200;

pub const PROPERTY_GROUP_VERSION_V1: i64 = 1;
pub const PROPERTY_GROUP_VERSION_V2: i64 = 2;

/// Port of `model.AccessControlPropertyGroupSchemaVersion` (property_group.go:36) — bump when the
/// shape of `access_control` fields changes in a way consumers must detect.
pub const ACCESS_CONTROL_PROPERTY_GROUP_SCHEMA_VERSION: i64 = 1;

/// Port of `model.IsValidPropertyGroupName` (property_group.go:84) — `^[a-z0-9][a-z0-9_]*$`.
///
/// A leading `_` is **reserved**, which is what the two-part pattern encodes; uppercase and
/// hyphens are rejected outright.
pub fn is_valid_property_group_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Port of `model.PropertyGroup` (property_group.go:39).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyGroup {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    /// [`PROPERTY_GROUP_VERSION_V1`] or [`PROPERTY_GROUP_VERSION_V2`].
    #[serde(rename = "version")]
    pub version: i64,

    #[serde(rename = "schema_version")]
    pub schema_version: i64,
}

impl PropertyGroup {
    /// Port of `(*PropertyGroup).IsPSAv1` (property_group.go:46).
    pub fn is_psav1(&self) -> bool {
        self.version == PROPERTY_GROUP_VERSION_V1
    }

    /// Port of `(*PropertyGroup).IsPSAv2` (property_group.go:50).
    pub fn is_psav2(&self) -> bool {
        self.version == PROPERTY_GROUP_VERSION_V2
    }

    /// Port of `(*PropertyGroup).PreSave` (property_group.go:54).
    ///
    /// Note the two defaults differ in shape: `version` is filled only when **exactly zero**,
    /// while `schema_version` is filled when **`<= 0`** — a negative version survives, a negative
    /// schema version does not.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.version == 0 {
            self.version = PROPERTY_GROUP_VERSION_V1;
        }

        if self.schema_version <= 0 {
            self.schema_version = 1;
        }
    }

    /// Port of `(*PropertyGroup).IsValid` (property_group.go:68).
    ///
    /// As with `property_field.go`, every branch shares one error id and differs only in the
    /// `FieldName`/`Reason` params. `schema_version` is **not** validated.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id", "invalid id", String::new()));
        }

        if !is_valid_property_group_name(&self.name) {
            return Err(err("name", "invalid name", format!("id={}", self.id)));
        }

        if self.version != PROPERTY_GROUP_VERSION_V1 && self.version != PROPERTY_GROUP_VERSION_V2 {
            return Err(err("version", "unknown value", format!("id={}", self.id)));
        }

        Ok(())
    }
}

fn err(field_name: &str, reason: &str, details: String) -> Box<AppError> {
    let mut params = std::collections::HashMap::new();
    params.insert(
        "FieldName".to_string(),
        serde_json::Value::String(field_name.to_string()),
    );
    params.insert(
        "Reason".to_string(),
        serde_json::Value::String(reason.to_string()),
    );
    Box::new(AppError::new(
        "PropertyGroup.IsValid",
        "model.property_group.is_valid.app_error",
        Some(params),
        details,
        400,
    ))
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
    fn property_group_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyGroup, "property_group");
    }
}
