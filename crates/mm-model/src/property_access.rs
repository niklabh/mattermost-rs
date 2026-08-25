//! Port of `model/property_access.go` — who may write a property field's definition and values.
//!
//! # The owners list supersedes the legacy gating
//!
//! When a field carries owners (under the `owners` key of its `Attrs`), that list — not the
//! `protected` flag or `source_plugin_id` — decides write access:
//!
//! - a machine caller must be a listed owner to write values or edit the field, and a listed
//!   owner may rewrite the **whole** definition including the owners list itself (there is no
//!   "cannot remove yourself" rule). The first owner is bootstrapped by a sysadmin over REST;
//! - a value write additionally requires the caller's acting-as scope to appear in that owner's
//!   `scopes`, unless the list is **empty**, which means unrestricted;
//! - a **human** caller is always rejected for value writes — including sysadmins — because an
//!   owned field's values are authoritative to the owning integration.
//!
//! Read access and masking are unchanged by any of this: they key off `access_mode`.

use serde::{Deserialize, Serialize};

use crate::property_field::{PermissionLevel, PropertyField};

/// Attrs key: the legacy protected flag.
pub const PROPERTY_ATTRS_PROTECTED: &str = "protected";
/// Attrs key: the plugin that created the field. Immutable once set.
pub const PROPERTY_ATTRS_SOURCE_PLUGIN_ID: &str = "source_plugin_id";
/// Attrs key: one of the three access modes.
pub const PROPERTY_ATTRS_ACCESS_MODE: &str = "access_mode";
/// Attrs key: the owners list.
pub const PROPERTY_ATTRS_OWNERS: &str = "owners";

/// The default access mode is the **empty string**, not a word — so an absent `access_mode` and
/// an explicit `""` are the same thing.
pub const PROPERTY_ACCESS_MODE_PUBLIC: &str = "";
pub const PROPERTY_ACCESS_MODE_SOURCE_ONLY: &str = "source_only";
pub const PROPERTY_ACCESS_MODE_SHARED_ONLY: &str = "shared_only";

pub const PROPERTY_OWNER_TYPE_PLUGIN: &str = "plugin";
pub const PROPERTY_OWNER_TYPE_SERVICE: &str = "service";
pub const PROPERTY_OWNER_TYPE_ROLE: &str = "role";
pub const PROPERTY_OWNER_TYPE_USER: &str = "user";

/// Defensive bounds on the owners list. Go is explicit that these are **not semantic limits** —
/// the server never interprets an owner id or scope — they only keep a buggy or hostile owner
/// from bloating the `Attrs` blob.
pub const PROPERTY_OWNERS_MAX_PER_FIELD: usize = 20;
pub const PROPERTY_OWNER_SCOPES_MAX: usize = 32;
pub const PROPERTY_OWNER_SCOPE_MAX_RUNES: usize = 64;
pub const PROPERTY_OWNER_ID_MAX_RUNES: usize = 255;

/// Port of `model.PropertyOwner` (property_access.go:69).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyOwner {
    #[serde(rename = "id")]
    pub id: String,

    /// One of the four `PROPERTY_OWNER_TYPE_*` constants.
    #[serde(rename = "type")]
    pub type_: String,

    /// **Empty means unrestricted**, not "no scopes".
    #[serde(rename = "scopes")]
    pub scopes: Option<Vec<String>>,
}

/// Port of `model.IsValidPropertyOwnerType` (property_access.go:76).
pub fn is_valid_property_owner_type(owner_type: &str) -> bool {
    matches!(
        owner_type,
        PROPERTY_OWNER_TYPE_PLUGIN
            | PROPERTY_OWNER_TYPE_SERVICE
            | PROPERTY_OWNER_TYPE_ROLE
            | PROPERTY_OWNER_TYPE_USER
    )
}

/// Port of `model.IsValidPropertyOwnerScope` (property_access.go:90) — `^[a-zA-Z0-9._:-]+$`.
///
/// A structural bound only: the server still does not interpret what a scope means. Note this is
/// `MatchString` on an anchored pattern, so an empty scope is rejected by the `+`.
pub fn is_valid_property_owner_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b':' || b == b'-')
}

/// Port of `model.GetPropertyFieldOwners` (property_access.go:98).
///
/// Go handles two shapes — a typed `[]PropertyOwner` right after a write, and the `[]any` of maps
/// a JSON round-trip produces. Here `Attrs` is always `serde_json::Value`, so there is only ever
/// the second shape and the type assertion has no counterpart. A malformed list is `None`, as in
/// Go, where both marshal and unmarshal errors return nil.
pub fn get_property_field_owners(field: &PropertyField) -> Option<Vec<PropertyOwner>> {
    let raw = field.attrs.as_ref()?.get(PROPERTY_ATTRS_OWNERS)?;
    if raw.is_null() {
        return None;
    }
    serde_json::from_value(raw.clone()).ok()
}

/// Port of `model.HasPropertyFieldOwners` (property_access.go:122).
pub fn has_property_field_owners(field: &PropertyField) -> bool {
    get_property_field_owners(field).is_some_and(|owners| !owners.is_empty())
}

/// Port of `model.IsKnownPropertyAccessMode` (property_access.go:127).
pub fn is_known_property_access_mode(access_mode: &str) -> bool {
    matches!(
        access_mode,
        PROPERTY_ACCESS_MODE_PUBLIC
            | PROPERTY_ACCESS_MODE_SOURCE_ONLY
            | PROPERTY_ACCESS_MODE_SHARED_ONLY
    )
}

/// Port of `model.IsPropertyFieldProtected` (property_access.go:138).
///
/// The attr must be a **JSON boolean** `true`; the string `"true"` does not count, because Go's
/// type assertion to `bool` fails on it.
pub fn is_property_field_protected(field: &PropertyField) -> bool {
    field
        .attrs
        .as_ref()
        .and_then(|a| a.get(PROPERTY_ATTRS_PROTECTED))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

impl PropertyField {
    /// Port of `(*PropertyField).GetAccessMode` (property_access.go:149).
    ///
    /// A non-string `access_mode` reads as public, not as an error.
    pub fn get_access_mode(&self) -> &str {
        self.attrs
            .as_ref()
            .and_then(|a| a.get(PROPERTY_ATTRS_ACCESS_MODE))
            .and_then(|v| v.as_str())
            .unwrap_or(PROPERTY_ACCESS_MODE_PUBLIC)
    }
}

/// Port of `model.ValidatePropertyFieldAccessMode` (property_access.go:161).
///
/// Three rules, and the third is the interesting one: `shared_only` filters what a caller sees to
/// the values they hold, while member-writable `permission_values` lets a user self-assign any
/// value — so the combination would let anyone see anything by first assigning it. Rejected here
/// rather than worked around later.
pub fn validate_property_field_access_mode(
    field: &PropertyField,
) -> Result<(), PropertyAccessError> {
    let Some(attrs) = field.attrs.as_ref() else {
        return Ok(());
    };

    // A missing key **and** a non-string value both mean "not set" and pass.
    let Some(access_mode) = attrs
        .get(PROPERTY_ATTRS_ACCESS_MODE)
        .and_then(|v| v.as_str())
    else {
        return Ok(());
    };

    if !is_known_property_access_mode(access_mode) {
        return Err(PropertyAccessError::UnknownAccessMode(
            access_mode.to_string(),
        ));
    }

    if (access_mode == PROPERTY_ACCESS_MODE_SOURCE_ONLY
        || access_mode == PROPERTY_ACCESS_MODE_SHARED_ONLY)
        && !is_property_field_protected(field)
    {
        return Err(PropertyAccessError::AccessModeRequiresProtected(
            access_mode.to_string(),
        ));
    }

    if access_mode == PROPERTY_ACCESS_MODE_SHARED_ONLY
        && field
            .permission_values
            .as_ref()
            .is_some_and(|p| p.as_str() == PermissionLevel::MEMBER)
    {
        return Err(PropertyAccessError::SharedOnlyWithMemberWritable);
    }

    Ok(())
}

/// The errors `property_access.go` returns. Go builds them with `fmt.Errorf`, quoting the mode
/// with **single quotes** rather than `%q` — reproduced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PropertyAccessError {
    #[error("invalid access mode '{0}'")]
    UnknownAccessMode(String),
    #[error("access mode '{0}' requires the field to be protected")]
    AccessModeRequiresProtected(String),
    #[error("access mode 'shared_only' is incompatible with member-writable permission_values")]
    SharedOnlyWithMemberWritable,
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
    fn property_owner_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyOwner, "property_owner");
    }
}
