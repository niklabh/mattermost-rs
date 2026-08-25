//! Port of `model/property_field_attrs_validation.go` — the canonical `Attrs` keys and the
//! validators that read them.

use crate::go_url::parse_request_uri;
use crate::property_field::PropertyField;
use crate::utils::is_valid_email;

pub const PROPERTY_FIELD_ATTR_VISIBILITY: &str = "visibility";
pub const PROPERTY_FIELD_ATTR_SORT_ORDER: &str = "sort_order";
pub const PROPERTY_FIELD_ATTR_VALUE_TYPE: &str = "value_type";
pub const PROPERTY_FIELD_ATTR_LDAP: &str = "ldap";
pub const PROPERTY_FIELD_ATTR_SAML: &str = "saml";
pub const PROPERTY_FIELD_ATTR_MANAGED: &str = "managed";
pub const PROPERTY_FIELD_ATTR_DISPLAY_NAME: &str = "display_name";

pub const PROPERTY_FIELD_VISIBILITY_HIDDEN: &str = "hidden";
pub const PROPERTY_FIELD_VISIBILITY_WHEN_SET: &str = "when_set";
pub const PROPERTY_FIELD_VISIBILITY_ALWAYS: &str = "always";

pub const PROPERTY_FIELD_VALUE_TYPE_EMAIL: &str = "email";
pub const PROPERTY_FIELD_VALUE_TYPE_URL: &str = "url";
pub const PROPERTY_FIELD_VALUE_TYPE_PHONE: &str = "phone";

/// Port of `model.PropertyFieldValueTypeTextMaxLength` (property_field_attrs_validation.go:37).
///
/// **Declared and unused inside this file** — no validator here enforces it; the API layer does.
pub const PROPERTY_FIELD_VALUE_TYPE_TEXT_MAX_LENGTH: usize = 64;

/// Port of `model.IsValidPropertyFieldVisibility` (property_field_attrs_validation.go:40).
pub fn is_valid_property_field_visibility(v: &str) -> bool {
    matches!(
        v,
        PROPERTY_FIELD_VISIBILITY_HIDDEN
            | PROPERTY_FIELD_VISIBILITY_WHEN_SET
            | PROPERTY_FIELD_VISIBILITY_ALWAYS
    )
}

/// Port of `model.IsValidPropertyFieldValueType` (property_field_attrs_validation.go:51).
pub fn is_valid_property_field_value_type(v: &str) -> bool {
    matches!(
        v,
        PROPERTY_FIELD_VALUE_TYPE_EMAIL
            | PROPERTY_FIELD_VALUE_TYPE_URL
            | PROPERTY_FIELD_VALUE_TYPE_PHONE
    )
}

/// Port of `model.ValidatePropertyFieldVisibility` (property_field_attrs_validation.go:64).
///
/// A **non-string** visibility is an error, but a missing key and a whitespace-only value are
/// both fine — the value is trimmed and an empty result passes.
pub fn validate_property_field_visibility(field: &PropertyField) -> Result<(), PropertyAttrsError> {
    let Some(attrs) = field.attrs.as_ref() else {
        return Ok(());
    };

    let Some(raw) = attrs.get(PROPERTY_FIELD_ATTR_VISIBILITY) else {
        return Ok(());
    };

    let Some(v) = raw.as_str() else {
        return Err(PropertyAttrsError::VisibilityNotAString);
    };

    let v = v.trim();
    if v.is_empty() {
        return Ok(());
    }

    if !is_valid_property_field_visibility(v) {
        return Err(PropertyAttrsError::InvalidVisibility(v.to_string()));
    }

    Ok(())
}

/// Port of `model.ValidatePropertyFieldSortOrder` (property_field_attrs_validation.go:91).
///
/// Any JSON number passes; anything else — including a numeric **string** — does not. Go's error
/// interpolates the Go type with `%T`, which has no Rust equivalent; the JSON type name is used
/// instead, and that is the only difference in the message.
pub fn validate_property_field_sort_order(field: &PropertyField) -> Result<(), PropertyAttrsError> {
    let Some(attrs) = field.attrs.as_ref() else {
        return Ok(());
    };

    let Some(raw) = attrs.get(PROPERTY_FIELD_ATTR_SORT_ORDER) else {
        return Ok(());
    };

    if raw.is_number() {
        return Ok(());
    }

    Err(PropertyAttrsError::SortOrderNotNumeric(
        json_type_name(raw).to_string(),
    ))
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Port of `model.ValidatePropertyValueForValueType` (property_field_attrs_validation.go:110).
///
/// - an empty `value_type` skips validation entirely;
/// - the value must decode as a **string**, whatever the type is;
/// - a whitespace-only value passes — the field is simply considered unset;
/// - `phone` is accepted as-is: there is deliberately no structural check;
/// - `url` uses `ParseRequestURI`, which rejects relative references, **and additionally**
///   requires a non-empty host, so `http:` and `file:///x` are rejected.
pub fn validate_property_value_for_value_type(
    value_type: &str,
    value: &serde_json::Value,
) -> Result<(), PropertyAttrsError> {
    if value_type.is_empty() {
        return Ok(());
    }

    let Some(str_value) = value.as_str() else {
        return Err(PropertyAttrsError::ExpectedStringValue(
            value_type.to_string(),
        ));
    };

    let str_value = str_value.trim();
    if str_value.is_empty() {
        return Ok(());
    }

    match value_type {
        PROPERTY_FIELD_VALUE_TYPE_EMAIL => {
            if !is_valid_email(str_value) {
                return Err(PropertyAttrsError::InvalidEmail(str_value.to_string()));
            }
        }
        PROPERTY_FIELD_VALUE_TYPE_URL => match parse_request_uri(str_value) {
            Err(_) => return Err(PropertyAttrsError::InvalidUrl(str_value.to_string())),
            Ok(u) => {
                if u.scheme.is_empty() || u.host.is_empty() {
                    return Err(PropertyAttrsError::InvalidUrl(str_value.to_string()));
                }
            }
        },
        // Phone values are accepted as-is.
        PROPERTY_FIELD_VALUE_TYPE_PHONE => {}
        other => return Err(PropertyAttrsError::UnknownValueType(other.to_string())),
    }

    Ok(())
}

/// Port of `model.GetPropertyFieldValueType` (property_field_attrs_validation.go:153) — trimmed,
/// and `""` when absent or not a string.
pub fn get_property_field_value_type(field: &PropertyField) -> &str {
    field
        .attrs
        .as_ref()
        .and_then(|a| a.get(PROPERTY_FIELD_ATTR_VALUE_TYPE))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
}

/// Port of `model.IsPropertyFieldSynced` (property_field_attrs_validation.go:163) — the field's
/// values are owned by an external sync service.
///
/// Note both attrs are read as **strings**: a boolean `true` under `ldap` does not count.
pub fn is_property_field_synced(field: &PropertyField) -> bool {
    !get_property_field_sync_source(field).is_empty()
}

/// Port of `model.GetPropertyFieldSyncSource` (property_field_attrs_validation.go:174).
///
/// **`ldap` wins** when both are set.
pub fn get_property_field_sync_source(field: &PropertyField) -> &'static str {
    let Some(attrs) = field.attrs.as_ref() else {
        return "";
    };

    let read = |key: &str| -> bool {
        attrs
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    };

    if read(PROPERTY_FIELD_ATTR_LDAP) {
        return "ldap";
    }
    if read(PROPERTY_FIELD_ATTR_SAML) {
        return "saml";
    }
    ""
}

/// The errors this file returns. Go builds them with `fmt.Errorf`; `%q` is reproduced as
/// [`crate::utils::go_quote`] renders it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PropertyAttrsError {
    #[error("visibility must be a string")]
    VisibilityNotAString,
    #[error(
        "invalid visibility {}: must be one of hidden, when_set, always",
        crate::utils::go_quote(.0)
    )]
    InvalidVisibility(String),
    #[error("sort_order must be numeric, got {0}")]
    SortOrderNotNumeric(String),
    #[error("expected string value for value_type {}", crate::utils::go_quote(.0))]
    ExpectedStringValue(String),
    #[error("invalid email: {}", crate::utils::go_quote(.0))]
    InvalidEmail(String),
    #[error("invalid url: {}", crate::utils::go_quote(.0))]
    InvalidUrl(String),
    #[error("unknown value_type {}", crate::utils::go_quote(.0))]
    UnknownValueType(String),
}
