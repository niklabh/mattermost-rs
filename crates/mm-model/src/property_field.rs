//! Port of `model/property_field.go` — the schema half of the property system (custom profile
//! attributes, channel bookmarks' metadata, access-control attributes).
//!
//! # PSAv1 and PSAv2 are the same struct with opposite rules
//!
//! An **empty `object_type`** means PSAv1: legacy, unique by `target_id` alone, and it may not be
//! protected or carry any of the three permission levels. A non-empty `object_type` means PSAv2:
//! `object_type` and `target_type` are checked against closed sets, `target_id` must be empty for
//! a `system` target and a valid id for `team`/`channel`, and a `system` object may only live at
//! the `system` level. [`PropertyField::is_valid`] runs one branch or the other, never both.
//!
//! # Every error is the same id
//!
//! All 25 branches return `model.property_field.is_valid.app_error`; what distinguishes them is
//! the i18n **params**, `FieldName` and `Reason`. A caller matching on the id alone learns
//! nothing — which is worth knowing before writing one.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_none;
use crate::utils::{AppError, AppResult, StringInterface, get_millis, is_valid_id, new_id};

/// Port of `model.PropertyFieldType` (property_field.go:14) — a `string` newtype.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PropertyFieldType(pub String);

impl PropertyFieldType {
    pub const TEXT: &'static str = "text";
    pub const SELECT: &'static str = "select";
    pub const MULTISELECT: &'static str = "multiselect";
    pub const DATE: &'static str = "date";
    pub const USER: &'static str = "user";
    pub const MULTIUSER: &'static str = "multiuser";
    pub const RANK: &'static str = "rank";

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Port of `(PropertyFieldType).SupportsOptions` (property_field.go:93) — `select`,
    /// `multiselect` and **`rank`**. `rank` is easy to forget: it carries options too.
    pub fn supports_options(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::SELECT | Self::MULTISELECT | Self::RANK
        )
    }

    /// The closed set `IsValid` checks against.
    pub fn is_known(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::TEXT
                | Self::SELECT
                | Self::MULTISELECT
                | Self::DATE
                | Self::USER
                | Self::MULTIUSER
                | Self::RANK
        )
    }
}

impl From<&str> for PropertyFieldType {
    fn from(s: &str) -> Self {
        PropertyFieldType(s.to_string())
    }
}

/// Port of `model.PropertyFieldTargetLevel` (property_field.go:18).
pub const PROPERTY_FIELD_TARGET_LEVEL_SYSTEM: &str = "system";
pub const PROPERTY_FIELD_TARGET_LEVEL_TEAM: &str = "team";
pub const PROPERTY_FIELD_TARGET_LEVEL_CHANNEL: &str = "channel";

/// Port of `model.PermissionLevel` (property_field.go:22) — a `string` newtype.
///
/// **Not to be confused with `permission.rs`**, which is the RBAC permission table. This is a
/// four-value ladder for who may edit one property field.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionLevel(pub String);

impl PermissionLevel {
    pub const NONE: &'static str = "none";
    pub const SYSADMIN: &'static str = "sysadmin";
    pub const MEMBER: &'static str = "member";
    /// Resolves against the field's target: sysadmin for `system`, team admin for `team`,
    /// channel admin for `channel`.
    pub const ADMIN: &'static str = "admin";

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Port of `validPermissionLevels` (property_field.go:61).
    pub fn is_valid(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::NONE | Self::SYSADMIN | Self::MEMBER | Self::ADMIN
        )
    }
}

impl From<&str> for PermissionLevel {
    fn from(s: &str) -> Self {
        PermissionLevel(s.to_string())
    }
}

pub const PROPERTY_FIELD_NAME_MAX_RUNES: usize = 255;
pub const PROPERTY_FIELD_TARGET_ID_MAX_RUNES: usize = 255;
pub const PROPERTY_FIELD_TARGET_TYPE_MAX_RUNES: usize = 255;
pub const PROPERTY_FIELD_OBJECT_TYPE_MAX_RUNES: usize = 255;

pub const PROPERTY_FIELD_OBJECT_TYPE_POST: &str = "post";
pub const PROPERTY_FIELD_OBJECT_TYPE_CHANNEL: &str = "channel";
pub const PROPERTY_FIELD_OBJECT_TYPE_USER: &str = "user";
pub const PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE: &str = "template";
pub const PROPERTY_FIELD_OBJECT_TYPE_SESSION: &str = "session";
pub const PROPERTY_FIELD_OBJECT_TYPE_SYSTEM: &str = "system";

/// Port of `model.PropertyFieldAttributeOptions` (property_field.go:591) — the `Attrs` key that
/// holds a field's selectable options.
pub const PROPERTY_FIELD_ATTRIBUTE_OPTIONS: &str = "options";

/// Port of `model.IsValidPSAv2PropertyFieldTargetType` (property_field.go:449).
pub fn is_valid_psav2_property_field_target_type(target_type: &str) -> bool {
    matches!(
        target_type,
        PROPERTY_FIELD_TARGET_LEVEL_SYSTEM
            | PROPERTY_FIELD_TARGET_LEVEL_TEAM
            | PROPERTY_FIELD_TARGET_LEVEL_CHANNEL
    )
}

/// Port of `model.IsValidPropertyFieldObjectType` (property_field.go:455).
pub fn is_valid_property_field_object_type(object_type: &str) -> bool {
    matches!(
        object_type,
        PROPERTY_FIELD_OBJECT_TYPE_POST
            | PROPERTY_FIELD_OBJECT_TYPE_CHANNEL
            | PROPERTY_FIELD_OBJECT_TYPE_USER
            | PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE
            | PROPERTY_FIELD_OBJECT_TYPE_SESSION
            | PROPERTY_FIELD_OBJECT_TYPE_SYSTEM
    )
}

/// Port of `model.PropertyField` (property_field.go:99).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyField {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "group_id")]
    pub group_id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "type")]
    pub type_: PropertyFieldType,

    /// Free-form per-field metadata. No `omitempty`, so a nil map is `null` — but a row read back
    /// from Postgres never has one. Go scans this column through sqlx, which allocates a nil map
    /// before scanning, so a SQL `NULL` arrives as an **empty map** (`{}`) and only a jsonb `null`
    /// arrives as `None`. `mm-store`'s `PropertyFieldRow::into_field` carries the measurement.
    #[serde(rename = "attrs")]
    pub attrs: Option<StringInterface>,

    #[serde(rename = "target_id")]
    pub target_id: String,

    /// One of the three target levels for PSAv2; free-form for PSAv1.
    #[serde(rename = "target_type")]
    pub target_type: String,

    /// **Empty means PSAv1** — see the module docs.
    #[serde(rename = "object_type")]
    pub object_type: String,

    #[serde(rename = "protected")]
    pub protected: bool,

    #[serde(rename = "permission_field", skip_serializing_if = "is_none")]
    pub permission_field: Option<PermissionLevel>,

    #[serde(rename = "permission_values", skip_serializing_if = "is_none")]
    pub permission_values: Option<PermissionLevel>,

    #[serde(rename = "permission_options", skip_serializing_if = "is_none")]
    pub permission_options: Option<PermissionLevel>,

    /// `Some("")` is a **transient unlink signal**, not a value: [`PropertyField::patch`] turns it
    /// into `None`, and `is_valid` lets it through untouched.
    #[serde(rename = "linked_field_id", skip_serializing_if = "is_none")]
    pub linked_field_id: Option<String>,

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

impl PropertyField {
    /// Port of `(*PropertyField).IsPSAv1` (property_field.go:437).
    pub fn is_psav1(&self) -> bool {
        self.object_type.is_empty()
    }

    /// Port of `(*PropertyField).IsPSAv2` (property_field.go:444).
    pub fn is_psav2(&self) -> bool {
        !self.object_type.is_empty()
    }

    /// Port of `(*PropertyField).GetAttr` (property_field.go:587).
    ///
    /// Go returns a nil `any` for a missing key; `None` is that.
    pub fn get_attr(&self, key: &str) -> Option<&serde_json::Value> {
        self.attrs.as_ref()?.get(key)
    }

    /// Port of `(*PropertyField).PreSave` (property_field.go:143).
    ///
    /// Both timestamps are set **unconditionally** and `delete_at` is forced to zero — so calling
    /// this on an existing row resurrects it and rewrites its creation time.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.create_at = get_millis();
        self.update_at = self.create_at;
        self.delete_at = 0;
    }

    /// Port of `(*PropertyField).EnsureOptionIDs` (property_field.go:155).
    ///
    /// Fills in a fresh id for every option that has none. Go round-trips the value through JSON
    /// "to handle any slice type" — here `attrs` is already `serde_json::Value`, so the walk is
    /// direct. An option that is not an object is left alone, exactly as Go's
    /// `optMap["id"].(string)` miss leaves a non-string id to be overwritten.
    ///
    /// Returns an error only where Go does: when the stored `options` is not an array of objects.
    pub fn ensure_option_ids(&mut self) -> Result<(), PropertyFieldError> {
        if !self.type_.supports_options() {
            return Ok(());
        }

        let Some(attrs) = self.attrs.as_mut() else {
            return Ok(());
        };

        let Some(options_raw) = attrs.get_mut(PROPERTY_FIELD_ATTRIBUTE_OPTIONS) else {
            return Ok(());
        };

        let Some(options) = options_raw.as_array_mut() else {
            return Err(PropertyFieldError::InvalidOptionsFormat(self.id.clone()));
        };

        for option in options.iter_mut() {
            let Some(map) = option.as_object_mut() else {
                return Err(PropertyFieldError::InvalidOptionsFormat(self.id.clone()));
            };
            let needs_id = match map.get("id") {
                Some(serde_json::Value::String(id)) => id.is_empty(),
                _ => true,
            };
            if needs_id {
                map.insert("id".to_string(), serde_json::Value::String(new_id()));
            }
        }

        Ok(())
    }

    /// Port of `(*PropertyField).IsValid` (property_field.go:200). See the module docs for the
    /// PSAv1/PSAv2 split and for why every branch shares one error id.
    pub fn is_valid(&self) -> AppResult {
        let details = || format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            // The id branch alone passes **no** details — there is no id to report.
            return Err(field_err("id", "invalid id", String::new()));
        }

        if !is_valid_id(&self.group_id) {
            return Err(field_err("group_id", "invalid id", details()));
        }

        if self.name.is_empty() {
            return Err(field_err("name", "value cannot be empty", details()));
        }

        if self.name.chars().count() > PROPERTY_FIELD_NAME_MAX_RUNES {
            return Err(field_err("name", "value exceeds maximum length", details()));
        }

        if self.target_type.chars().count() > PROPERTY_FIELD_TARGET_TYPE_MAX_RUNES {
            return Err(field_err(
                "target_type",
                "value exceeds maximum length",
                details(),
            ));
        }

        if self.target_id.chars().count() > PROPERTY_FIELD_TARGET_ID_MAX_RUNES {
            return Err(field_err(
                "target_id",
                "value exceeds maximum length",
                details(),
            ));
        }

        if self.object_type.chars().count() > PROPERTY_FIELD_OBJECT_TYPE_MAX_RUNES {
            return Err(field_err(
                "object_type",
                "value exceeds maximum length",
                details(),
            ));
        }

        if self.is_psav2() {
            if !is_valid_property_field_object_type(&self.object_type) {
                return Err(field_err("object_type", "unknown value", details()));
            }

            if !is_valid_psav2_property_field_target_type(&self.target_type) {
                return Err(field_err("target_type", "unknown value", details()));
            }

            match self.target_type.as_str() {
                // Guards rather than nested `if`s: a false guard falls to the catch-all, which
                // is what the nested form did.
                PROPERTY_FIELD_TARGET_LEVEL_SYSTEM if !self.target_id.is_empty() => {
                    return Err(field_err(
                        "target_id",
                        "must be empty for system target type",
                        details(),
                    ));
                }
                PROPERTY_FIELD_TARGET_LEVEL_TEAM | PROPERTY_FIELD_TARGET_LEVEL_CHANNEL
                    if !is_valid_id(&self.target_id) =>
                {
                    return Err(field_err(
                        "target_id",
                        "must be a valid ID for team or channel target type",
                        details(),
                    ));
                }
                _ => {}
            }

            if self.object_type == PROPERTY_FIELD_OBJECT_TYPE_SYSTEM
                && self.target_type != PROPERTY_FIELD_TARGET_LEVEL_SYSTEM
            {
                return Err(field_err(
                    "target_type",
                    "must be system for system object type",
                    details(),
                ));
            }
        } else {
            if self.protected {
                return Err(field_err(
                    "protected",
                    "PSAv1 properties cannot be protected",
                    details(),
                ));
            }

            if self.permission_field.is_some() {
                return Err(field_err(
                    "permission_field",
                    "PSAv1 properties cannot have permissions",
                    details(),
                ));
            }

            if self.permission_values.is_some() {
                return Err(field_err(
                    "permission_values",
                    "PSAv1 properties cannot have permissions",
                    details(),
                ));
            }

            if self.permission_options.is_some() {
                return Err(field_err(
                    "permission_options",
                    "PSAv1 properties cannot have permissions",
                    details(),
                ));
            }
        }

        if !self.type_.is_known() {
            return Err(field_err("type", "unknown value", details()));
        }

        // An empty string is allowed here: it is the unlink signal.
        if let Some(linked) = &self.linked_field_id {
            if !linked.is_empty() && !is_valid_id(linked) {
                return Err(field_err("linked_field_id", "invalid id", details()));
            }
        }

        if self.object_type == PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE
            && self.linked_field_id.as_ref().is_some_and(|l| !l.is_empty())
        {
            return Err(field_err(
                "linked_field_id",
                "template fields cannot have a linked field",
                details(),
            ));
        }

        if self.create_at == 0 {
            return Err(field_err("create_at", "value cannot be zero", details()));
        }

        if self.update_at == 0 {
            return Err(field_err("update_at", "value cannot be zero", details()));
        }

        for (name, level) in [
            ("permission_field", &self.permission_field),
            ("permission_values", &self.permission_values),
            ("permission_options", &self.permission_options),
        ] {
            if let Some(level) = level {
                if !level.is_valid() {
                    return Err(field_err(name, "invalid permission level", details()));
                }
            }
        }

        if self.protected {
            match &self.permission_field {
                None => {
                    return Err(field_err(
                        "permission_field",
                        "protected fields must have explicit permissions with field set to none",
                        details(),
                    ));
                }
                Some(level) if level.as_str() != PermissionLevel::NONE => {
                    return Err(field_err(
                        "permission_field",
                        "protected fields must have field permission set to none",
                        details(),
                    ));
                }
                Some(_) => {}
            }
        }

        if !self.protected
            && self
                .permission_field
                .as_ref()
                .is_some_and(|l| l.as_str() == PermissionLevel::NONE)
        {
            return Err(field_err(
                "permission_field",
                "non-protected fields cannot have field permission set to none",
                details(),
            ));
        }

        Ok(())
    }

    /// Port of `(*PropertyField).Patch` (property_field.go:389).
    ///
    /// With `merge_attrs`, a patch key whose value is **`null` deletes** that attr; without it,
    /// `attrs` is replaced wholesale. `linked_field_id: Some("")` clears the link.
    pub fn patch(&mut self, patch: &PropertyFieldPatch, merge_attrs: bool) {
        if let Some(name) = &patch.name {
            self.name = name.clone();
        }

        if let Some(type_) = &patch.type_ {
            self.type_ = type_.clone();
        }

        if let Some(attrs) = &patch.attrs {
            if merge_attrs {
                let target = self.attrs.get_or_insert_with(StringInterface::new);
                for (key, value) in attrs.iter() {
                    if value.is_null() {
                        target.remove(key);
                    } else {
                        target.insert(key.clone(), value.clone());
                    }
                }
            } else {
                self.attrs = Some(attrs.clone());
            }
        }

        if let Some(target_id) = &patch.target_id {
            self.target_id = target_id.clone();
        }

        if let Some(target_type) = &patch.target_type {
            self.target_type = target_type.clone();
        }

        if let Some(linked) = &patch.linked_field_id {
            if linked.is_empty() {
                self.linked_field_id = None;
            } else {
                self.linked_field_id = Some(linked.clone());
            }
        }
    }
}

/// Port of `model.PropertyFieldPatch` (property_field.go:332).
///
/// Only `linked_field_id` carries `omitempty`; the other five are always written, as `null` when
/// unset. That is what lets a patch distinguish "leave alone" from "set to empty".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyFieldPatch {
    #[serde(rename = "name")]
    pub name: Option<String>,

    #[serde(rename = "type")]
    pub type_: Option<PropertyFieldType>,

    #[serde(rename = "attrs")]
    pub attrs: Option<StringInterface>,

    #[serde(rename = "target_id")]
    pub target_id: Option<String>,

    #[serde(rename = "target_type")]
    pub target_type: Option<String>,

    #[serde(rename = "linked_field_id", skip_serializing_if = "is_none")]
    pub linked_field_id: Option<String>,
}

impl PropertyFieldPatch {
    /// Port of `(*PropertyFieldPatch).IsValid` (property_field.go:353).
    ///
    /// A **subset** of the field's own rules: only the four length/emptiness checks and the type
    /// set. Nothing here validates the PSAv1/PSAv2 invariants, so a patch that would produce an
    /// invalid field passes.
    pub fn is_valid(&self) -> AppResult {
        if let Some(name) = &self.name {
            if name.is_empty() {
                return Err(patch_err("name", "value cannot be empty"));
            }
            if name.chars().count() > PROPERTY_FIELD_NAME_MAX_RUNES {
                return Err(patch_err("name", "value exceeds maximum length"));
            }
        }

        if let Some(target_type) = &self.target_type {
            if target_type.chars().count() > PROPERTY_FIELD_TARGET_TYPE_MAX_RUNES {
                return Err(patch_err("target_type", "value exceeds maximum length"));
            }
        }

        if let Some(target_id) = &self.target_id {
            if target_id.chars().count() > PROPERTY_FIELD_TARGET_ID_MAX_RUNES {
                return Err(patch_err("target_id", "value exceeds maximum length"));
            }
        }

        if let Some(type_) = &self.type_ {
            if !type_.is_known() {
                return Err(patch_err("type", "unknown value"));
            }
        }

        Ok(())
    }
}

fn property_field_app_error(
    where_: &'static str,
    field_name: &str,
    reason: &str,
    details: String,
) -> Box<AppError> {
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
        where_,
        "model.property_field.is_valid.app_error",
        Some(params),
        details,
        400,
    ))
}

fn field_err(field_name: &str, reason: &str, details: String) -> Box<AppError> {
    property_field_app_error("PropertyField.IsValid", field_name, reason, details)
}

fn patch_err(field_name: &str, reason: &str) -> Box<AppError> {
    property_field_app_error(
        "PropertyFieldPatch.IsValid",
        field_name,
        reason,
        String::new(),
    )
}

/// Port of `model.PropertyFieldSearchCursor` (property_field.go:466).
///
/// Two mutually exclusive pagination keys: directory listings page by `create_at` (stable, since
/// it never changes), delta sync pages by `update_at`. No `json:` tags — the cursor is assembled
/// from [`PropertyFieldSearch`]'s three `cursor_*` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertyFieldSearchCursor {
    pub property_field_id: String,
    pub create_at: i64,
    pub update_at: i64,
}

impl PropertyFieldSearchCursor {
    /// Port of `(PropertyFieldSearchCursor).IsEmpty` (property_field.go:472) — "start from the
    /// beginning", which is valid.
    pub fn is_empty(&self) -> bool {
        self.property_field_id.is_empty() && self.create_at == 0 && self.update_at == 0
    }

    /// Port of `(PropertyFieldSearchCursor).IsValid` (property_field.go:480).
    ///
    /// `hasCreate == hasUpdate` rejects **both** the neither case and the both case in one test.
    pub fn is_valid(&self) -> Result<(), PropertyFieldError> {
        if self.is_empty() {
            return Ok(());
        }

        if !is_valid_id(&self.property_field_id) {
            return Err(PropertyFieldError::InvalidCursorId);
        }

        if (self.create_at > 0) == (self.update_at > 0) {
            return Err(PropertyFieldError::CursorKeyAmbiguous);
        }

        Ok(())
    }
}

/// Port of `model.PropertyFieldSearch` (property_field.go:509) — the client's request body.
///
/// Scope is expressed one of two ways and they are mutually exclusive: hierarchical
/// (`channel_id`/`team_id`, which returns the named scope **and every ancestor**) or
/// single-target (`target_type` + `target_id`). `since > 0` switches to delta mode, which orders
/// by `update_at`, includes tombstones, and requires `cursor_update_at`.
///
/// **`per_page` is the only field without `omitempty`.**
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PropertyFieldSearch {
    #[serde(
        rename = "object_types",
        skip_serializing_if = "crate::serde_helpers::is_none_or_empty_vec"
    )]
    pub object_types: Option<Vec<String>>,

    #[serde(
        rename = "target_type",
        skip_serializing_if = "crate::serde_helpers::is_empty_str"
    )]
    pub target_type: String,

    #[serde(
        rename = "target_id",
        skip_serializing_if = "crate::serde_helpers::is_empty_str"
    )]
    pub target_id: String,

    #[serde(
        rename = "channel_id",
        skip_serializing_if = "crate::serde_helpers::is_empty_str"
    )]
    pub channel_id: String,

    #[serde(
        rename = "team_id",
        skip_serializing_if = "crate::serde_helpers::is_empty_str"
    )]
    pub team_id: String,

    /// Tagged **`since`**, while the field is `SinceUpdateAt`.
    #[serde(
        rename = "since",
        skip_serializing_if = "crate::serde_helpers::is_zero_i64"
    )]
    pub since_update_at: i64,

    #[serde(
        rename = "cursor_id",
        skip_serializing_if = "crate::serde_helpers::is_empty_str"
    )]
    pub cursor_id: String,

    #[serde(
        rename = "cursor_create_at",
        skip_serializing_if = "crate::serde_helpers::is_zero_i64"
    )]
    pub cursor_create_at: i64,

    #[serde(
        rename = "cursor_update_at",
        skip_serializing_if = "crate::serde_helpers::is_zero_i64"
    )]
    pub cursor_update_at: i64,

    #[serde(rename = "per_page")]
    pub per_page: i64,
}

/// Port of `model.PropertyFieldSearchOpts` (property_field.go:531) — the server-side filter set.
/// No `json:` tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertyFieldSearchOpts {
    pub group_id: String,
    /// Deprecated in Go; mutually exclusive with [`object_types`](Self::object_types).
    pub object_type: String,
    pub object_types: Vec<String>,
    pub target_type: String,
    pub target_ids: Vec<String>,
    pub channel_id: String,
    pub team_id: String,
    pub linked_field_id: String,
    /// `<= 0` means no filter.
    pub since_update_at: i64,
    pub include_deleted: bool,
    pub cursor: PropertyFieldSearchCursor,
    pub per_page: i64,
}

impl PropertyFieldSearchOpts {
    /// Port of `(PropertyFieldSearchOpts).IsValid` (property_field.go:551).
    ///
    /// The last rule is the one worth reading twice: the cursor key must match the active
    /// ordering, because a mismatch would compare against the wrong column and **silently skip
    /// rows** rather than fail.
    ///
    /// Note the documented "ChannelID requires TeamID" invariant is **not** enforced here despite
    /// the doc comment saying it is.
    pub fn is_valid(&self) -> Result<(), PropertyFieldError> {
        if !self.object_type.is_empty() && !self.object_types.is_empty() {
            return Err(PropertyFieldError::ObjectTypeMutuallyExclusive);
        }

        if !self.object_type.is_empty() && !is_valid_property_field_object_type(&self.object_type) {
            return Err(PropertyFieldError::InvalidObjectType(
                self.object_type.clone(),
            ));
        }

        for ot in &self.object_types {
            if !is_valid_property_field_object_type(ot) {
                return Err(PropertyFieldError::InvalidObjectType(ot.clone()));
            }
        }

        let scope_by_chan_team = !self.channel_id.is_empty() || !self.team_id.is_empty();
        let scope_by_target = !self.target_type.is_empty() || !self.target_ids.is_empty();
        if scope_by_chan_team && scope_by_target {
            return Err(PropertyFieldError::ScopeMutuallyExclusive);
        }

        self.cursor.is_valid()?;

        if !self.cursor.is_empty() {
            let delta_mode = self.since_update_at > 0;
            if delta_mode && self.cursor.update_at == 0 {
                return Err(PropertyFieldError::CursorUpdateAtRequired);
            }
            if !delta_mode && self.cursor.create_at == 0 {
                return Err(PropertyFieldError::CursorCreateAtRequired);
            }
        }

        Ok(())
    }
}

/// The non-`AppError` failures in `property_field.go`. Go returns bare `errors.New`/`fmt.Errorf`
/// values for these; the messages are Go's, with `%q` reproduced as [`crate::utils::go_quote`]
/// would render it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PropertyFieldError {
    #[error("invalid options format for field ID {0}")]
    InvalidOptionsFormat(String),
    #[error("property field id is invalid")]
    InvalidCursorId,
    #[error("cursor must have exactly one of create_at or update_at set")]
    CursorKeyAmbiguous,
    #[error("object_type and object_types are mutually exclusive")]
    ObjectTypeMutuallyExclusive,
    #[error("invalid object_type {}", crate::utils::go_quote(.0))]
    InvalidObjectType(String),
    #[error("channel_id/team_id cannot be combined with target_type/target_id")]
    ScopeMutuallyExclusive,
    #[error("cursor_update_at required when since is set")]
    CursorUpdateAtRequired,
    #[error("cursor_create_at required when since is not set")]
    CursorCreateAtRequired,
    #[error("options list cannot be empty")]
    EmptyOptionsList,
    #[error("invalid option at index {0}: {1}")]
    InvalidOption(usize, String),
    #[error("duplicate option name found at index {0}: {1}")]
    DuplicateOptionName(usize, String),
    #[error("data cannot be nil")]
    NilOptionData,
    #[error("id cannot be empty")]
    EmptyOptionId,
    #[error("id is not a valid ID")]
    InvalidOptionId,
    #[error("name cannot be empty")]
    EmptyOptionName,
}

/// Port of the `model.PropertyOption` interface (property_field.go:593).
pub trait PropertyOption {
    fn get_id(&self) -> String;
    fn get_name(&self) -> String;
    fn set_id(&mut self, id: String);
    fn is_valid(&self) -> Result<(), PropertyFieldError>;
}

/// Port of `model.PropertyOptions[T]` (property_field.go:600).
///
/// `#[serde(transparent)]` because Go's `[]T` newtype has no wrapper on the wire. `Default` is
/// hand-written rather than derived: the derive would demand `T: Default`, which a list type does
/// not need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PropertyOptions<T>(pub Vec<T>);

impl<T> Default for PropertyOptions<T> {
    fn default() -> Self {
        PropertyOptions(Vec::new())
    }
}

impl<T: PropertyOption> PropertyOptions<T> {
    /// Port of `(PropertyOptions[T]).IsValid` (property_field.go:622).
    ///
    /// An **empty list is invalid** — a field that supports options must offer at least one — and
    /// names must be unique. Ids are not checked for uniqueness here, only individually.
    pub fn is_valid(&self) -> Result<(), PropertyFieldError> {
        if self.0.is_empty() {
            return Err(PropertyFieldError::EmptyOptionsList);
        }

        let mut seen_names = std::collections::HashSet::new();
        for (i, option) in self.0.iter().enumerate() {
            if let Err(e) = option.is_valid() {
                return Err(PropertyFieldError::InvalidOption(i, e.to_string()));
            }

            if !seen_names.insert(option.get_name()) {
                return Err(PropertyFieldError::DuplicateOptionName(
                    i,
                    option.get_name(),
                ));
            }
        }

        Ok(())
    }

    /// Port of `model.NewPropertyOptionsFromFieldAttrs[T]` (property_field.go:602) — decode the
    /// `options` attr and fill in any missing ids.
    pub fn from_field_attrs(options_arr: &serde_json::Value) -> Result<Self, PropertyFieldError>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut options: Vec<T> = serde_json::from_value(options_arr.clone())
            .map_err(|_| PropertyFieldError::InvalidOptionsFormat(String::new()))?;

        for option in options.iter_mut() {
            if option.get_id().is_empty() {
                option.set_id(new_id());
            }
        }

        Ok(PropertyOptions(options))
    }
}

/// Port of `model.PluginPropertyOption` (property_field.go:643) — the flexible option shape
/// plugins use.
///
/// # It marshals **unwrapped**
///
/// The struct has one field tagged `data`, but hand-written `MarshalJSON`/`UnmarshalJSON` emit
/// and read the inner map **directly** — so the wire form is `{"id":…,"name":…}`, never
/// `{"data":{…}}`. A nil map marshals as `{}`, not `null`. That is reproduced here with the same
/// hand-written codec; the `data` tag is dead in Go and has no counterpart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginPropertyOption {
    pub data: Option<crate::utils::StringMap>,
}

impl Serialize for PluginPropertyOption {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.data {
            Some(data) => data.serialize(s),
            // Go marshals a nil map as `{}` here, not `null`.
            None => crate::utils::StringMap::new().serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for PluginPropertyOption {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let data = crate::utils::StringMap::deserialize(d)?;
        Ok(PluginPropertyOption { data: Some(data) })
    }
}

impl PluginPropertyOption {
    /// Port of `model.NewPluginPropertyOption` (property_field.go:647).
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        let mut data = crate::utils::StringMap::new();
        data.insert("id".to_string(), id.into());
        data.insert("name".to_string(), name.into());
        PluginPropertyOption { data: Some(data) }
    }

    /// Port of `(*PluginPropertyOption).GetValue` (property_field.go:691).
    pub fn get_value(&self, key: &str) -> String {
        self.data
            .as_ref()
            .and_then(|d| d.get(key))
            .cloned()
            .unwrap_or_default()
    }

    /// Port of `(*PluginPropertyOption).SetValue` (property_field.go:699).
    pub fn set_value(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.data
            .get_or_insert_with(crate::utils::StringMap::new)
            .insert(key.into(), value.into());
    }
}

impl PropertyOption for PluginPropertyOption {
    fn get_id(&self) -> String {
        self.get_value("id")
    }

    fn get_name(&self) -> String {
        self.get_value("name")
    }

    fn set_id(&mut self, id: String) {
        self.set_value("id", id);
    }

    /// Port of `(*PluginPropertyOption).IsValid` (property_field.go:669).
    fn is_valid(&self) -> Result<(), PropertyFieldError> {
        let Some(_) = &self.data else {
            return Err(PropertyFieldError::NilOptionData);
        };

        let id = self.get_id();
        if id.is_empty() {
            return Err(PropertyFieldError::EmptyOptionId);
        }

        if !is_valid_id(&id) {
            return Err(PropertyFieldError::InvalidOptionId);
        }

        if self.get_name().is_empty() {
            return Err(PropertyFieldError::EmptyOptionName);
        }

        Ok(())
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
    fn property_field_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyField, "property_field");
    }
    #[test]
    fn property_field_patch_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyFieldPatch, "property_field_patch");
    }
    #[test]
    fn property_field_search_round_trips_the_fixture() {
        assert_fixture_round_trips!(PropertyFieldSearch, "property_field_search");
    }
    #[test]
    fn plugin_property_option_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginPropertyOption, "plugin_property_option");
    }
}

#[cfg(test)]
mod plugin_property_option_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// [`PluginPropertyOption`] has no serialization fixture and cannot have one: its
    /// `MarshalJSON` emits the inner map unwrapped, so the JSON has no `data` key and the
    /// reflective filler's struct-keyed completeness check cannot describe it. This corpus is the
    /// oracle instead — and it is the only thing pinning `{}`-not-`null` for a nil map.
    #[test]
    fn marshal_and_accessors_match_go() {
        let oracle = oracle();
        let cases = oracle["plugin_property_option"].as_array().unwrap();
        assert_eq!(cases.len(), 4);

        let mut extra = crate::utils::StringMap::new();
        extra.insert("id".to_string(), "o99t9rfydganbi87d6ekygfory".to_string());
        extra.insert("name".to_string(), "Engineering".to_string());
        extra.insert("color".to_string(), "#112233".to_string());

        let inputs = [
            PluginPropertyOption { data: None },
            PluginPropertyOption {
                data: Some(crate::utils::StringMap::new()),
            },
            PluginPropertyOption::new("o99t9rfydganbi87d6ekygfory", "Engineering"),
            PluginPropertyOption { data: Some(extra) },
        ];
        let names = ["nil_data", "empty_data", "id_and_name", "extra_keys"];

        for ((case, input), name) in cases.iter().zip(inputs.iter()).zip(names.iter()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );

            // Text, not value graph: Go sorts map keys, and `extra_keys` is the case that shows it.
            assert_eq!(
                serde_json::to_string(input).unwrap(),
                case["out"].as_str().unwrap(),
                "marshalling {name}"
            );
            assert_eq!(input.get_id(), case["get_id"].as_str().unwrap(), "{name}");
            assert_eq!(
                input.get_name(),
                case["get_name"].as_str().unwrap(),
                "{name}"
            );

            match case.get("is_valid_error").and_then(|v| v.as_str()) {
                Some(expected) => assert_eq!(
                    input.is_valid().unwrap_err().to_string(),
                    expected,
                    "IsValid({name})"
                ),
                None => assert!(input.is_valid().is_ok(), "IsValid({name}) should pass"),
            }

            let back: PluginPropertyOption =
                serde_json::from_str(case["out"].as_str().unwrap()).unwrap();
            assert_eq!(
                back.get_id(),
                case["round_trip_id"].as_str().unwrap(),
                "{name} round trip"
            );
            assert_eq!(
                back.get_name(),
                case["round_trip_name"].as_str().unwrap(),
                "{name} round trip"
            );
        }
    }
}
