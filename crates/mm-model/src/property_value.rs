//! Port of `model/property_value.go` — one stored value of one property field.
//!
//! # `Value` is a `json.RawMessage`
//!
//! Go stores the value's **bytes**, untouched: whatever the client sent, byte for byte, is what
//! goes in the column and what comes back out. Rust's nearest equivalent that still supports
//! `PartialEq`, `Default` and `Clone` is [`serde_json::Value`], which re-serialises canonically —
//! so `1.0`, `1.00` and `1e0` all read back as `1.0`, and object key order is normalised to
//! sorted. **That is a real divergence** and it matters exactly where Go's comment says it does:
//! `SanitizePropertyValue` returns the *original bytes* when nothing changed so callers can skip
//! a write by identity, and that identity comparison no longer means "unchanged bytes" here.
//! Every other use — validation, comparison of decoded shapes — is unaffected.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{is_empty_str, is_zero_i64};
use crate::utils::{AppError, AppResult, get_millis, is_valid_id, new_id};

pub const PROPERTY_VALUE_TARGET_ID_MAX_RUNES: usize = 255;
pub const PROPERTY_VALUE_TARGET_TYPE_MAX_RUNES: usize = 255;

pub const PROPERTY_VALUE_TARGET_TYPE_POST: &str = "post";
pub const PROPERTY_VALUE_TARGET_TYPE_USER: &str = "user";
pub const PROPERTY_VALUE_TARGET_TYPE_CHANNEL: &str = "channel";
pub const PROPERTY_VALUE_TARGET_TYPE_SYSTEM: &str = "system";

/// Port of `model.PropertyValueSystemTargetID` (property_value.go:25).
///
/// A system-scoped value attaches to the instance, so there is no 26-character entity id to point
/// at; this sentinel — the literal string `system`, **not** an id — stands in for one.
pub const PROPERTY_VALUE_SYSTEM_TARGET_ID: &str = "system";

/// Port of `isValidPropertyValueTargetID` (property_value.go:45).
pub fn is_valid_property_value_target_id(target_type: &str, target_id: &str) -> bool {
    if target_type == PROPERTY_VALUE_TARGET_TYPE_SYSTEM {
        return target_id == PROPERTY_VALUE_SYSTEM_TARGET_ID;
    }
    is_valid_id(target_id)
}

/// Port of `model.PropertyValue` (property_value.go:28).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyValue {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "target_id")]
    pub target_id: String,

    #[serde(rename = "target_type")]
    pub target_type: String,

    #[serde(rename = "group_id")]
    pub group_id: String,

    #[serde(rename = "field_id")]
    pub field_id: String,

    /// See the module docs — Go keeps the raw bytes.
    #[serde(rename = "value")]
    pub value: serde_json::Value,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "created_by")]
    pub created_by: String,

    #[serde(rename = "updated_by")]
    pub updated_by: String,
}

impl PropertyValue {
    /// Port of `(*PropertyValue).PreSave` (property_value.go:52).
    ///
    /// `create_at` is filled only when zero, but `update_at` is then set to **`create_at`**, not
    /// to now — so re-saving an existing row rewinds `update_at` to the creation time.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.update_at = self.create_at;
    }

    /// Port of `(*PropertyValue).IsValid` (property_value.go:64).
    ///
    /// Note the order: `target_id` is checked for **validity before** `target_type` is checked for
    /// emptiness, so a value with an empty target type and a non-id target id reports `target_id`.
    pub fn is_valid(&self) -> AppResult {
        let details = || format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            return Err(err("id", "invalid id", String::new()));
        }

        if !is_valid_property_value_target_id(&self.target_type, &self.target_id) {
            return Err(err("target_id", "invalid id", details()));
        }

        if self.target_type.is_empty() {
            return Err(err("target_type", "value cannot be empty", details()));
        }

        if self.target_type.chars().count() > PROPERTY_VALUE_TARGET_TYPE_MAX_RUNES {
            return Err(err(
                "target_type",
                "value exceeds maximum length",
                details(),
            ));
        }

        if self.target_id.chars().count() > PROPERTY_VALUE_TARGET_ID_MAX_RUNES {
            return Err(err("target_id", "value exceeds maximum length", details()));
        }

        if !is_valid_id(&self.group_id) {
            return Err(err("group_id", "invalid id", details()));
        }

        if !is_valid_id(&self.field_id) {
            return Err(err("field_id", "invalid id", details()));
        }

        if self.create_at == 0 {
            return Err(err("create_at", "value cannot be zero", details()));
        }

        if self.update_at == 0 {
            return Err(err("update_at", "value cannot be zero", details()));
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
        "PropertyValue.IsValid",
        "model.property_value.is_valid.app_error",
        Some(params),
        details,
        400,
    ))
}

/// Port of `model.PropertyValueSearchCursor` (property_value.go:117). Identical in shape and
/// rules to `PropertyFieldSearchCursor`; the two are separate types in Go and stay separate here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertyValueSearchCursor {
    pub property_value_id: String,
    pub create_at: i64,
    pub update_at: i64,
}

impl PropertyValueSearchCursor {
    /// Port of `(PropertyValueSearchCursor).IsEmpty` (property_value.go:123).
    pub fn is_empty(&self) -> bool {
        self.property_value_id.is_empty() && self.create_at == 0 && self.update_at == 0
    }

    /// Port of `(PropertyValueSearchCursor).IsValid` (property_value.go:127).
    pub fn is_valid(&self) -> Result<(), PropertyValueError> {
        if self.is_empty() {
            return Ok(());
        }

        if !is_valid_id(&self.property_value_id) {
            return Err(PropertyValueError::InvalidCursorId);
        }

        if (self.create_at > 0) == (self.update_at > 0) {
            return Err(PropertyValueError::CursorKeyAmbiguous);
        }

        Ok(())
    }
}

/// Port of `model.PropertyValueSearchOpts` (property_value.go:149). No `json:` tags.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PropertyValueSearchOpts {
    pub group_id: String,
    pub target_type: String,
    pub target_ids: Vec<String>,
    pub field_id: String,
    /// `> 0` switches to delta mode.
    pub since_update_at: i64,
    pub include_deleted: bool,
    pub cursor: PropertyValueSearchCursor,
    pub per_page: i64,
    /// A value filter. `json.RawMessage` in Go — see the module docs.
    pub value: Option<serde_json::Value>,
}

impl PropertyValueSearchOpts {
    /// Port of `(PropertyValueSearchOpts).IsValid` (property_value.go:161).
    pub fn is_valid(&self) -> Result<(), PropertyValueError> {
        self.cursor.is_valid()?;

        if !self.cursor.is_empty() {
            let delta_mode = self.since_update_at > 0;
            if delta_mode && self.cursor.update_at == 0 {
                return Err(PropertyValueError::CursorUpdateAtRequired);
            }
            if !delta_mode && self.cursor.create_at == 0 {
                return Err(PropertyValueError::CursorCreateAtRequired);
            }
        }

        Ok(())
    }
}

/// Port of `model.PropertyValueSearch` (property_value.go:185) — the client's request body.
/// `per_page` is the only key without `omitempty`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyValueSearch {
    #[serde(rename = "cursor_id", skip_serializing_if = "is_empty_str")]
    pub cursor_id: String,

    #[serde(rename = "cursor_create_at", skip_serializing_if = "is_zero_i64")]
    pub cursor_create_at: i64,

    #[serde(rename = "cursor_update_at", skip_serializing_if = "is_zero_i64")]
    pub cursor_update_at: i64,

    /// Tagged **`since`**.
    #[serde(rename = "since", skip_serializing_if = "is_zero_i64")]
    pub since_update_at: i64,

    #[serde(rename = "per_page")]
    pub per_page: i64,
}

/// Port of `model.PropertyValuePatchItem` (property_value.go:195) — one entry of a batch PATCH.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyValuePatchItem {
    #[serde(rename = "field_id")]
    pub field_id: String,

    #[serde(rename = "value")]
    pub value: serde_json::Value,
}

/// The non-`AppError` failures in `property_value.go`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PropertyValueError {
    #[error("property value id is invalid")]
    InvalidCursorId,
    #[error("cursor must have exactly one of create_at or update_at set")]
    CursorKeyAmbiguous,
    #[error("cursor_update_at required when since is set")]
    CursorUpdateAtRequired,
    #[error("cursor_create_at required when since is not set")]
    CursorCreateAtRequired,
}

/// Port of `model.SanitizePropertyValue` (property_value.go:209).
///
/// Trims a top-level string; trims each element of a top-level array of strings and **drops the
/// empty ones**; leaves every other shape alone. An array containing a non-string is left
/// untouched entirely, because Go's `[]string` unmarshal fails for the whole array.
///
/// Go returns the original bytes when nothing changed. Here the value is returned unchanged
/// instead — the identity comparison Go's callers can make is not available; see the module docs.
pub fn sanitize_property_value(raw: &serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::String(s) = raw {
        let trimmed = s.trim();
        if trimmed == s {
            return raw.clone();
        }
        return serde_json::Value::String(trimmed.to_string());
    }

    if let serde_json::Value::Array(items) = raw {
        // Go decodes into `[]string`, which fails if any element is not a string — and on
        // failure the whole value passes through unchanged.
        let mut strings = Vec::with_capacity(items.len());
        for item in items {
            match item.as_str() {
                Some(s) => strings.push(s),
                None => return raw.clone(),
            }
        }

        let mut filtered: Vec<&str> = Vec::with_capacity(strings.len());
        let mut changed = false;
        for v in strings.iter() {
            let t = v.trim();
            if t != *v {
                changed = true;
            }
            if t.is_empty() {
                if !v.is_empty() {
                    changed = true;
                }
                continue;
            }
            filtered.push(t);
        }

        if !changed && filtered.len() == strings.len() {
            return raw.clone();
        }

        return serde_json::Value::Array(
            filtered
                .into_iter()
                .map(|s| serde_json::Value::String(s.to_string()))
                .collect(),
        );
    }

    raw.clone()
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
    fn property_value_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyValue, "property_value");
    }
    #[test]
    fn property_value_search_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyValueSearch, "property_value_search");
    }
    #[test]
    fn property_value_patch_item_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyValuePatchItem, "property_value_patch_item");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// The sanitizer trims strings and drops empty entries from **string arrays only** — a mixed
    /// array, an array of numbers and an object all pass through untouched, which is the branch a
    /// reader is most likely to over-generalise.
    ///
    /// **Divergence:** Go returns the caller's original `json.RawMessage` when nothing changed,
    /// so `[1, 2]` keeps its space. The port works on a parsed [`serde_json::Value`] and
    /// re-encodes, so the comparison here is on the value graph. Callers using Go's
    /// bytes-unchanged as a "skip the write" signal have no counterpart; nothing in this tree
    /// does.
    #[test]
    fn sanitize_property_value_matches_go() {
        let oracle = oracle();
        let cases = oracle["sanitize_property_value"].as_array().unwrap();
        assert!(cases.len() >= 15);

        for case in cases {
            let input = case["in"].as_str().unwrap();
            let parsed: serde_json::Value = serde_json::from_str(input).unwrap();
            let expected: serde_json::Value =
                serde_json::from_str(case["out"].as_str().unwrap()).unwrap();
            assert_eq!(
                sanitize_property_value(&parsed),
                expected,
                "SanitizePropertyValue({input})"
            );
        }
    }
}
