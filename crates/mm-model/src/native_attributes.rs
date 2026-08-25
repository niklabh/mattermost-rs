//! Port of `model/native_attributes.go` — synthetic `PropertyField` descriptors for the User
//! columns that ABAC policies can reference.
//!
//! Native attributes are referenced as `user.<name>` in CEL, in contrast to custom profile
//! attributes, which are `user.attributes.<name>`. **The SQL/CEL source of truth lives in the
//! enterprise access-control package** — these descriptors only drive editor autocomplete, so
//! adding one here does not make it queryable.
//!
//! # The ids are synthetic but must be stable
//!
//! These fields are never persisted, yet the ABAC editors resolve the selected attribute **by
//! id** — a CPA and a session attribute can share a name, so only the id disambiguates the
//! namespace. Hence [`NATIVE_ATTRIBUTE_ID_PREFIX`] plus the name, which is a fixed unique set.
//!
//! # Go's gob comment does not apply here
//!
//! Go builds the select options as `[]any` of `map[string]any` rather than a concrete slice type,
//! because these cross the plugin RPC boundary inside `Attrs` and an unregistered gob type kills
//! the shared plugin connection. There is no gob here — but the **JSON is identical either way**,
//! which is what this port has to match, so the shape is reproduced as written.

use crate::property_field::{
    PROPERTY_FIELD_ATTRIBUTE_OPTIONS, PROPERTY_FIELD_OBJECT_TYPE_USER,
    PROPERTY_FIELD_TARGET_LEVEL_SYSTEM, PermissionLevel, PropertyField, PropertyFieldType,
};
use crate::utils::StringInterface;

pub const NATIVE_ATTRIBUTE_PROPERTY_FIELD_EMAIL: &str = "email";
pub const NATIVE_ATTRIBUTE_PROPERTY_FIELD_VERIFIED: &str = "verified";
/// One word, lower-case: **`isbot`**, not `is_bot`.
pub const NATIVE_ATTRIBUTE_PROPERTY_FIELD_IS_BOT: &str = "isbot";
/// **`createat`**, not `create_at`.
pub const NATIVE_ATTRIBUTE_PROPERTY_FIELD_CREATE_AT: &str = "createat";

pub const NATIVE_ATTRIBUTE_DISPLAY_NAME_EMAIL: &str = "Email";
pub const NATIVE_ATTRIBUTE_DISPLAY_NAME_VERIFIED: &str = "Email verified";
pub const NATIVE_ATTRIBUTE_DISPLAY_NAME_IS_BOT: &str = "Bot account";
pub const NATIVE_ATTRIBUTE_DISPLAY_NAME_CREATE_AT: &str = "Account created";

/// Port of `model.NativeAttributeIDPrefix` (native_attributes.go:29).
pub const NATIVE_ATTRIBUTE_ID_PREFIX: &str = "native_user_attribute_";

/// `Attrs` key marking a field as Mattermost-native (`user.<name>`) rather than a custom profile
/// attribute (`user.attributes.<name>`).
pub const NATIVE_ATTRIBUTE_ATTR_MARKER: &str = "native";
/// `Attrs` key carrying the human-readable label.
pub const NATIVE_ATTRIBUTE_ATTR_DISPLAY_NAME: &str = "display_name";
/// `Attrs` key listing the visual operators an editor may offer, e.g. `==`, `youngerThanDays`.
pub const NATIVE_ATTRIBUTE_ATTR_OPERATORS: &str = "operators";

/// Port of `nativeAttributeField` (native_attributes.go:46).
///
/// Every native field is system-scoped and sysadmin-gated on all three permission levels.
fn native_attribute_field(
    group_id: &str,
    name: &str,
    display_name: &str,
    field_type: PropertyFieldType,
    operators: &[&str],
    extra_attrs: Option<StringInterface>,
) -> PropertyField {
    let mut attrs = StringInterface::new();
    attrs.insert(
        NATIVE_ATTRIBUTE_ATTR_MARKER.to_string(),
        serde_json::Value::Bool(true),
    );
    attrs.insert(
        NATIVE_ATTRIBUTE_ATTR_DISPLAY_NAME.to_string(),
        serde_json::Value::String(display_name.to_string()),
    );
    attrs.insert(
        NATIVE_ATTRIBUTE_ATTR_OPERATORS.to_string(),
        serde_json::Value::Array(
            operators
                .iter()
                .map(|o| serde_json::Value::String((*o).to_string()))
                .collect(),
        ),
    );

    // `maps.Copy(attrs, extraAttrs)` — the extras win on a key collision.
    if let Some(extra) = extra_attrs {
        for (key, value) in extra {
            attrs.insert(key, value);
        }
    }

    PropertyField {
        id: format!("{NATIVE_ATTRIBUTE_ID_PREFIX}{name}"),
        group_id: group_id.to_string(),
        name: name.to_string(),
        type_: field_type,
        object_type: PROPERTY_FIELD_OBJECT_TYPE_USER.to_string(),
        target_type: PROPERTY_FIELD_TARGET_LEVEL_SYSTEM.to_string(),
        permission_field: Some(PermissionLevel::SYSADMIN.into()),
        permission_values: Some(PermissionLevel::SYSADMIN.into()),
        permission_options: Some(PermissionLevel::SYSADMIN.into()),
        attrs: Some(attrs),
        ..Default::default()
    }
}

/// Port of `model.NativeUserAttributeFields` (native_attributes.go:75) — the four descriptors,
/// appended to the access-control autocomplete beside the custom profile attributes.
///
/// Note the operator sets differ per field: `email` gets six text operators, the two booleans get
/// equality only, and `createat` gets exactly one — `youngerThanDays`.
pub fn native_user_attribute_fields(group_id: &str) -> Vec<PropertyField> {
    // The two boolean-ish fields are modelled as `select` with a literal true/false option list.
    let bool_select_options = || {
        let mut options = StringInterface::new();
        options.insert(
            PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_string(),
            serde_json::json!([{"name": "true"}, {"name": "false"}]),
        );
        options
    };

    vec![
        native_attribute_field(
            group_id,
            NATIVE_ATTRIBUTE_PROPERTY_FIELD_EMAIL,
            NATIVE_ATTRIBUTE_DISPLAY_NAME_EMAIL,
            PropertyFieldType::TEXT.into(),
            &["==", "!=", "in", "contains", "startsWith", "endsWith"],
            None,
        ),
        native_attribute_field(
            group_id,
            NATIVE_ATTRIBUTE_PROPERTY_FIELD_VERIFIED,
            NATIVE_ATTRIBUTE_DISPLAY_NAME_VERIFIED,
            PropertyFieldType::SELECT.into(),
            &["==", "!="],
            Some(bool_select_options()),
        ),
        native_attribute_field(
            group_id,
            NATIVE_ATTRIBUTE_PROPERTY_FIELD_IS_BOT,
            NATIVE_ATTRIBUTE_DISPLAY_NAME_IS_BOT,
            PropertyFieldType::SELECT.into(),
            &["==", "!="],
            Some(bool_select_options()),
        ),
        native_attribute_field(
            group_id,
            NATIVE_ATTRIBUTE_PROPERTY_FIELD_CREATE_AT,
            NATIVE_ATTRIBUTE_DISPLAY_NAME_CREATE_AT,
            PropertyFieldType::TEXT.into(),
            &["youngerThanDays"],
            None,
        ),
    ]
}
