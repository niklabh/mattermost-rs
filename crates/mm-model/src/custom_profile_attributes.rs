//! Port of `model/custom_profile_attributes.go` — the "User Attributes" feature, still called
//! Custom Profile Attributes (CPA) everywhere an identifier is involved.
//!
//! Go's own file header is explicit about the naming: the REST paths, WebSocket events, JSON
//! shapes and the PSA group name `custom_profile_attributes` all keep the old name for backward
//! compatibility (MM-68235). Renaming any of them here would be a wire break.
//!
//! # A CPA field is a `PropertyField` with its `attrs` typed
//!
//! `CPAField` embeds `PropertyField` and then **shadows its `Attrs`** with a typed struct. Go's
//! field-shadowing rules mean the shallower `attrs` wins and the embedded one is suppressed, so
//! the wire shape is every `PropertyField` key plus one typed `attrs` object. `#[serde(flatten)]`
//! cannot express that — it would emit `attrs` twice — so the codec here is hand-written: the
//! parent is projected to an object, its `attrs` key replaced, and the result flattened.
//!
//! # Field names are CEL identifiers
//!
//! A CPA name is used verbatim in ABAC policies as `user.attributes.<name>`, so it must satisfy
//! the CEL identifier grammar and must not be a CEL keyword — otherwise the expression either
//! fails to parse or needs backtick quoting the visual builder does not emit. That is what
//! [`validate_cpa_field_name`] enforces, and it is why a leading underscore is **allowed**.

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::property_access::{
    PROPERTY_ATTRS_ACCESS_MODE, PROPERTY_ATTRS_OWNERS, PROPERTY_ATTRS_PROTECTED,
    PROPERTY_ATTRS_SOURCE_PLUGIN_ID, PropertyOwner,
};
use crate::property_field::{
    PROPERTY_FIELD_ATTRIBUTE_OPTIONS, PropertyField, PropertyFieldError, PropertyFieldPatch,
    PropertyOption, PropertyOptions,
};
use crate::property_field_attrs_validation::{
    PROPERTY_FIELD_ATTR_DISPLAY_NAME, PROPERTY_FIELD_ATTR_LDAP, PROPERTY_FIELD_ATTR_MANAGED,
    PROPERTY_FIELD_ATTR_SAML, PROPERTY_FIELD_ATTR_SORT_ORDER, PROPERTY_FIELD_ATTR_VALUE_TYPE,
    PROPERTY_FIELD_ATTR_VISIBILITY, PROPERTY_FIELD_VALUE_TYPE_EMAIL,
    PROPERTY_FIELD_VALUE_TYPE_PHONE, PROPERTY_FIELD_VALUE_TYPE_TEXT_MAX_LENGTH,
    PROPERTY_FIELD_VALUE_TYPE_URL, PROPERTY_FIELD_VISIBILITY_ALWAYS,
    PROPERTY_FIELD_VISIBILITY_HIDDEN, PROPERTY_FIELD_VISIBILITY_WHEN_SET,
};
use crate::serde_helpers::{is_empty_str, is_none, is_none_or_empty_vec};
use crate::utils::{AppError, AppResult, StringInterface, is_valid_id};

// The CPA-prefixed names are **aliases**, not redeclarations — Go's comment says so explicitly,
// so that a rename on one side cannot silently diverge from the other. Reproduced as `const … =
// <the other const>` for the same reason.
pub const CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_SORT_ORDER: &str =
    PROPERTY_FIELD_ATTR_SORT_ORDER;
pub const CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_VALUE_TYPE: &str =
    PROPERTY_FIELD_ATTR_VALUE_TYPE;
pub const CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_VISIBILITY: &str =
    PROPERTY_FIELD_ATTR_VISIBILITY;
pub const CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_LDAP: &str = PROPERTY_FIELD_ATTR_LDAP;
pub const CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_SAML: &str = PROPERTY_FIELD_ATTR_SAML;
pub const CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_MANAGED: &str = PROPERTY_FIELD_ATTR_MANAGED;
pub const CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_DISPLAY_NAME: &str =
    PROPERTY_FIELD_ATTR_DISPLAY_NAME;

pub const CUSTOM_PROFILE_ATTRIBUTES_VALUE_TYPE_EMAIL: &str = PROPERTY_FIELD_VALUE_TYPE_EMAIL;
pub const CUSTOM_PROFILE_ATTRIBUTES_VALUE_TYPE_URL: &str = PROPERTY_FIELD_VALUE_TYPE_URL;
pub const CUSTOM_PROFILE_ATTRIBUTES_VALUE_TYPE_PHONE: &str = PROPERTY_FIELD_VALUE_TYPE_PHONE;

pub const CUSTOM_PROFILE_ATTRIBUTES_VISIBILITY_HIDDEN: &str = PROPERTY_FIELD_VISIBILITY_HIDDEN;
pub const CUSTOM_PROFILE_ATTRIBUTES_VISIBILITY_WHEN_SET: &str = PROPERTY_FIELD_VISIBILITY_WHEN_SET;
pub const CUSTOM_PROFILE_ATTRIBUTES_VISIBILITY_ALWAYS: &str = PROPERTY_FIELD_VISIBILITY_ALWAYS;
/// The default is **`when_set`**, not `always`.
pub const CUSTOM_PROFILE_ATTRIBUTES_VISIBILITY_DEFAULT: &str =
    CUSTOM_PROFILE_ATTRIBUTES_VISIBILITY_WHEN_SET;

pub const CPA_OPTION_NAME_MAX_LENGTH: usize = 128;
pub const CPA_OPTION_COLOR_MAX_LENGTH: usize = 128;
pub const CPA_VALUE_TYPE_TEXT_MAX_LENGTH: usize = PROPERTY_FIELD_VALUE_TYPE_TEXT_MAX_LENGTH;

/// Port of `model.CPAFieldNamePattern` (custom_profile_attributes.go:57) —
/// `^[A-Za-z_][A-Za-z0-9_]*$`, the CEL `IDENTIFIER` grammar.
///
/// A **leading underscore is permitted**, matching both the CEL grammar and the enterprise
/// unparser.
pub fn cpa_field_name_matches_pattern(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Port of `model.CPAFieldNameReservedWords` (custom_profile_attributes.go:67) — the CEL keywords
/// from cel-go v0.27.0's lexer.
///
/// Grouped as Go groups them: the three literals, the two operator-keywords, then the alphabetical
/// reserved words. Bare use of any of them in member-select position — `user.attributes.in` —
/// either fails to parse or needs quoting the visual builder does not emit.
pub const CPA_FIELD_NAME_RESERVED_WORDS: [&str; 21] = [
    "true",
    "false",
    "null", //
    "in",
    "as", //
    "break",
    "const",
    "continue",
    "else", //
    "for",
    "function",
    "if",
    "import", //
    "let",
    "loop",
    "package",
    "namespace", //
    "return",
    "var",
    "void",
    "while",
];

/// Port of `model.ValidateCPAFieldName` (custom_profile_attributes.go:79).
///
/// Both branches are **422 Unprocessable Entity**, not 400, and both interpolate the offending
/// name into the i18n params under `Name`.
pub fn validate_cpa_field_name(name: &str) -> AppResult {
    if !cpa_field_name_matches_pattern(name) {
        return Err(cpa_name_err("invalid_charset", name));
    }

    if CPA_FIELD_NAME_RESERVED_WORDS.contains(&name) {
        return Err(cpa_name_err("reserved_word", name));
    }

    Ok(())
}

fn cpa_name_err(suffix: &str, name: &str) -> Box<AppError> {
    let mut params = std::collections::HashMap::new();
    params.insert(
        "Name".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    Box::new(AppError::new(
        "ValidateCPAFieldName",
        format!("model.cpa_field.name.{suffix}.app_error"),
        Some(params),
        "",
        422,
    ))
}

/// Port of `model.CustomProfileAttributesSelectOption` (custom_profile_attributes.go:102).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomProfileAttributesSelectOption {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    /// A hex colour. Only length-checked, never parsed.
    #[serde(rename = "color")]
    pub color: String,

    /// `omitempty`, and the only nullable field — an option with no explicit rank.
    #[serde(rename = "rank", skip_serializing_if = "is_none")]
    pub rank: Option<i64>,
}

impl PropertyOption for CustomProfileAttributesSelectOption {
    fn get_id(&self) -> String {
        self.id.clone()
    }

    fn get_name(&self) -> String {
        self.name.clone()
    }

    fn set_id(&mut self, id: String) {
        self.id = id;
    }

    /// Port of `(CustomProfileAttributesSelectOption).IsValid`
    /// (custom_profile_attributes.go:118).
    ///
    /// The two length caps are **bytes**, and `color` is checked only when non-empty.
    fn is_valid(&self) -> Result<(), PropertyFieldError> {
        if self.id.is_empty() {
            return Err(PropertyFieldError::EmptyOptionId);
        }

        if !is_valid_id(&self.id) {
            return Err(PropertyFieldError::InvalidOptionId);
        }

        if self.name.is_empty() {
            return Err(PropertyFieldError::EmptyOptionName);
        }

        if self.name.len() > CPA_OPTION_NAME_MAX_LENGTH {
            return Err(PropertyFieldError::InvalidOption(
                0,
                format!("name is too long, max length is {CPA_OPTION_NAME_MAX_LENGTH}"),
            ));
        }

        if !self.color.is_empty() && self.color.len() > CPA_OPTION_COLOR_MAX_LENGTH {
            return Err(PropertyFieldError::InvalidOption(
                0,
                format!("color is too long, max length is {CPA_OPTION_COLOR_MAX_LENGTH}"),
            ));
        }

        Ok(())
    }
}

/// Port of `model.CPAAttrs` (custom_profile_attributes.go:157) — the typed form of a CPA field's
/// `attrs` blob.
///
/// `sort_order` is a **`float64`**, not an integer: `CPAFieldsFromPropertyFields` sorts on it, and
/// a JSON round trip through `any` would have produced a float anyway.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CPAAttrs {
    #[serde(rename = "visibility")]
    pub visibility: String,

    /// Rendered by [`crate::serde_helpers::go_float`]: Go writes an integral `float64` as `65`,
    /// serde_json writes `65.0`, and the fixture round-trip is what caught it.
    #[serde(rename = "sort_order", with = "crate::serde_helpers::go_float")]
    pub sort_order: f64,

    #[serde(rename = "options")]
    pub options: PropertyOptions<CustomProfileAttributesSelectOption>,

    #[serde(rename = "value_type")]
    pub value_type: String,

    /// The LDAP attribute this field syncs from; empty means not LDAP-synced.
    #[serde(rename = "ldap")]
    pub ldap: String,

    #[serde(rename = "saml")]
    pub saml: String,

    /// `"admin"` means admin-managed; see [`CPAField::is_admin_managed`].
    #[serde(rename = "managed")]
    pub managed: String,

    #[serde(rename = "protected")]
    pub protected: bool,

    #[serde(rename = "source_plugin_id")]
    pub source_plugin_id: String,

    #[serde(rename = "access_mode")]
    pub access_mode: String,

    /// The user-facing label, kept separate from `name` (the CEL identifier).
    ///
    /// Go's comment is worth preserving: **`omitempty` applies only to a direct marshal of
    /// `CPAAttrs`** — `ToPropertyField` always writes the key into the underlying map, empty or
    /// not.
    #[serde(rename = "display_name", skip_serializing_if = "is_empty_str")]
    pub display_name: String,

    /// When non-empty, the owners list governs write access, superseding the legacy
    /// `protected`/`source_plugin_id` gating and the sync lock. Managed only by an administrator
    /// over REST.
    #[serde(rename = "owners", skip_serializing_if = "is_none_or_empty_vec")]
    pub owners: Option<Vec<PropertyOwner>>,
}

/// Port of `model.CPAField` (custom_profile_attributes.go:139).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CPAField {
    pub property_field: PropertyField,
    /// Shadows `property_field.attrs` on the wire — see the module docs.
    pub attrs: CPAAttrs,
}

impl Serialize for CPAField {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut object = match serde_json::to_value(&self.property_field) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => return Err(serde::ser::Error::custom("PropertyField is not an object")),
        };
        let typed = serde_json::to_value(&self.attrs).map_err(serde::ser::Error::custom)?;
        // The shallower field wins, exactly as Go's shadowing does.
        object.insert("attrs".to_string(), typed);
        object.serialize(s)
    }
}

impl<'de> Deserialize<'de> for CPAField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let attrs: CPAAttrs = match value.get("attrs") {
            Some(raw) => serde_json::from_value(raw.clone()).map_err(serde::de::Error::custom)?,
            None => CPAAttrs::default(),
        };
        let property_field: PropertyField =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(CPAField {
            property_field,
            attrs,
        })
    }
}

impl CPAField {
    /// Port of `(*CPAField).IsSynced` (custom_profile_attributes.go:181).
    pub fn is_synced(&self) -> bool {
        !self.attrs.ldap.is_empty() || !self.attrs.saml.is_empty()
    }

    /// Port of `(*CPAField).IsAdminManaged` (custom_profile_attributes.go:185).
    pub fn is_admin_managed(&self) -> bool {
        self.attrs.managed == "admin"
    }

    /// Port of `(*CPAField).ToPropertyField` (custom_profile_attributes.go:213).
    ///
    /// Writes **eleven** keys unconditionally — including `display_name`, which `CPAAttrs`'s own
    /// tag would have omitted — and the `owners` key only when the list is non-empty, so a legacy
    /// field's attrs blob is left byte-identical and `HasPropertyFieldOwners` stays false for it.
    pub fn to_property_field(&self) -> PropertyField {
        let mut pf = self.property_field.clone();

        let mut attrs = StringInterface::new();
        let mut put = |key: &str, value: serde_json::Value| {
            attrs.insert(key.to_string(), value);
        };

        put(
            CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_VISIBILITY,
            serde_json::Value::String(self.attrs.visibility.clone()),
        );
        put(
            CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_SORT_ORDER,
            serde_json::json!(self.attrs.sort_order),
        );
        put(
            CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_VALUE_TYPE,
            serde_json::Value::String(self.attrs.value_type.clone()),
        );
        put(
            PROPERTY_FIELD_ATTRIBUTE_OPTIONS,
            serde_json::to_value(&self.attrs.options).unwrap_or(serde_json::Value::Null),
        );
        put(
            CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_LDAP,
            serde_json::Value::String(self.attrs.ldap.clone()),
        );
        put(
            CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_SAML,
            serde_json::Value::String(self.attrs.saml.clone()),
        );
        put(
            CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_MANAGED,
            serde_json::Value::String(self.attrs.managed.clone()),
        );
        put(
            PROPERTY_ATTRS_PROTECTED,
            serde_json::Value::Bool(self.attrs.protected),
        );
        put(
            PROPERTY_ATTRS_SOURCE_PLUGIN_ID,
            serde_json::Value::String(self.attrs.source_plugin_id.clone()),
        );
        put(
            PROPERTY_ATTRS_ACCESS_MODE,
            serde_json::Value::String(self.attrs.access_mode.clone()),
        );
        put(
            CUSTOM_PROFILE_ATTRIBUTES_PROPERTY_ATTRS_DISPLAY_NAME,
            serde_json::Value::String(self.attrs.display_name.clone()),
        );

        if let Some(owners) = &self.attrs.owners {
            if !owners.is_empty() {
                put(
                    PROPERTY_ATTRS_OWNERS,
                    serde_json::to_value(owners).unwrap_or(serde_json::Value::Null),
                );
            }
        }

        pf.attrs = Some(attrs);
        pf
    }

    /// Port of `(*CPAField).Patch` (custom_profile_attributes.go:191).
    ///
    /// **Mutates the patch**: `target_id` and `target_type` are cleared before it is applied,
    /// because CPA does not use targets. The patch is then applied with `merge_attrs = false`, so
    /// a patch carrying `attrs` replaces the blob wholesale rather than merging into it.
    pub fn patch(&mut self, patch: &mut PropertyFieldPatch) -> Result<(), CPAError> {
        patch.target_id = None;
        patch.target_type = None;

        let mut pf = self.to_property_field();
        pf.patch(patch, false);

        *self = CPAField::from_property_field(&pf)?;

        Ok(())
    }

    /// Port of `model.NewCPAFieldFromPropertyField` (custom_profile_attributes.go:241).
    ///
    /// Go round-trips the untyped `Attrs` map through JSON into `CPAAttrs`; unknown keys are
    /// dropped and missing ones take their zero value. A `nil` map decodes to the zero `CPAAttrs`
    /// rather than failing.
    pub fn from_property_field(pf: &PropertyField) -> Result<Self, CPAError> {
        let attrs: CPAAttrs = match &pf.attrs {
            Some(map) => serde_json::from_value(serde_json::Value::Object(map.clone()))
                .map_err(|e| CPAError::Attrs(e.to_string()))?,
            None => CPAAttrs::default(),
        };

        Ok(CPAField {
            property_field: pf.clone(),
            attrs,
        })
    }
}

/// Port of `model.CPAFieldsFromPropertyFields` (custom_profile_attributes.go:261).
///
/// Sorted by `sort_order` ascending, **ties broken by id** — so the order is total and stable,
/// unlike the raw `sort.Slice` it is built on, which is not.
pub fn cpa_fields_from_property_fields(pfs: &[PropertyField]) -> Result<Vec<CPAField>, CPAError> {
    let mut cpa_fields = Vec::with_capacity(pfs.len());
    for pf in pfs {
        cpa_fields.push(CPAField::from_property_field(pf)?);
    }

    cpa_fields.sort_by(|a, b| {
        a.attrs
            .sort_order
            .partial_cmp(&b.attrs.sort_order)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.property_field.id.cmp(&b.property_field.id))
    });

    Ok(cpa_fields)
}

/// The one non-`AppError` failure in this file: Go's `json.Marshal`/`json.Unmarshal` error from
/// `NewCPAFieldFromPropertyField`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CPAError {
    #[error("{0}")]
    Attrs(String),
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
    fn custom_profile_attributes_select_option_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            CustomProfileAttributesSelectOption,
            "custom_profile_attributes_select_option"
        );
    }
    #[test]
    fn cpa_field_round_trips_the_fixture() {
        assert_fixture_round_trips!(CPAField, "cpa_field");
    }
    #[test]
    fn cpa_attrs_round_trips_the_fixture() {
        assert_fixture_round_trips!(CPAAttrs, "cpa_attrs");
    }
}
