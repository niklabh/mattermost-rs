//! Port of the property-service hook chain as it runs for the `access_control` group —
//! `app/properties/{license_check,access_control,access_control_attribute_validation,
//! field_limit,type_change_value_cleanup}.go` — and of `mapPropertyServiceError`
//! (app/property_errors.go:44), which turns what the hooks refuse with into wire errors.
//!
//! # Five hooks, in registration order, and only one group
//!
//! `app/server.go:316-380` registers, in this order: the licence check, access control, attribute
//! validation, the value audit, the field limit, the session-attributes guard (another group), and
//! last the type-change cleanup. Every one of them is constructed with `cpaGroup.ID` and passes
//! any other group straight through, so this module is the `access_control` group's chain and
//! nothing else's — the generic `api4/properties.go` routes on `boards` and `post_attributes` run
//! no hook at all ([`crate::properties`]).
//!
//! Order matters twice. A **pre**-hook that refuses stops the chain, so an unlicensed create is
//! the licence error and never the attribute error the same body would also earn. And the
//! attribute hook runs *after* access control, so `access_mode` is validated before `visibility`
//! is, and a body wrong on both reports the first.
//!
//! # The caller is a user, and that decides most of the access-control arms
//!
//! `AccessControlHook` reasons about three kinds of caller: an installed plugin, a sync service
//! (`system:ldap_sync`, `system:saml_sync`), and a human. Over the REST API the caller id is the
//! session's user id — or `system:local_admin` for the unix-socket API — and neither is ever a
//! plugin or a sync service. So on these routes: `source_plugin_id` and `protected` cannot be set
//! (403 `access_denied`); an owner-managed field's **definition** may be edited or deleted by a
//! human but its **values** may not (`checkOwnerValueWriteAccess` rejects every human, system
//! administrators included); a `protected` field's definition is untouchable; and a field with
//! an `ldap` or `saml` attr is sync-locked for values. All of those are reachable on a licensed
//! server by planting the field row, which is how they are measured.
//!
//! # What this side cannot know: which plugins are installed
//!
//! `checkFieldDeleteAccess` (access_control.go:840) lets a protected field be deleted by anyone
//! when its source plugin is **not installed** — `pluginChecker` asks the plugin host. There is no
//! plugin host here, so no plugin is ever installed from this side's point of view, and a
//! protected field whose plugin *is* installed on the Go server would be deletable here and not
//! there. That is the one arm of this chain that depends on state this server does not hold; it
//! is recorded, not guarded, because a protected field cannot be created over REST at all.
//!
//! # The audit hook is a log line
//!
//! `PropertyValueAuditHook` feeds `App.auditCPAValueChange`, which writes to the audit **log** —
//! not the `Audits` table, not the wire. It is a `tracing` event here, as every other audit record
//! in this tree is.

use std::collections::{HashMap, HashSet};

use mm_model::custom_profile_attributes::CustomProfileAttributesSelectOption;
use mm_model::permission::PERMISSION_MANAGE_SYSTEM;
use mm_model::property_access::{
    PROPERTY_ACCESS_MODE_PUBLIC, PROPERTY_ACCESS_MODE_SHARED_ONLY, PROPERTY_ATTRS_ACCESS_MODE,
    PROPERTY_ATTRS_OWNERS, PROPERTY_ATTRS_PROTECTED, PROPERTY_ATTRS_SOURCE_PLUGIN_ID,
    PROPERTY_OWNER_ID_MAX_RUNES, PROPERTY_OWNER_SCOPE_MAX_RUNES, PROPERTY_OWNER_SCOPES_MAX,
    PROPERTY_OWNER_TYPE_PLUGIN, PROPERTY_OWNER_TYPE_SERVICE, PROPERTY_OWNERS_MAX_PER_FIELD,
    PropertyOwner, get_property_field_owners, has_property_field_owners,
    is_property_field_protected, is_valid_property_owner_scope, is_valid_property_owner_type,
    validate_property_field_access_mode,
};
use mm_model::property_access_control::{
    CALLER_ID_LDAP_SYNC, CALLER_ID_LOCAL_ADMIN, CALLER_ID_SAML_SYNC,
};
use mm_model::property_field::{
    PROPERTY_FIELD_ATTRIBUTE_OPTIONS, PROPERTY_FIELD_NAME_MAX_RUNES,
    PROPERTY_FIELD_OBJECT_TYPE_SYSTEM, PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE,
    PROPERTY_FIELD_OBJECT_TYPE_USER, PermissionLevel, PropertyField, PropertyFieldType,
    PropertyOptions,
};
use mm_model::property_field_attrs_validation::{
    PROPERTY_FIELD_ATTR_DISPLAY_NAME, PROPERTY_FIELD_ATTR_LDAP, PROPERTY_FIELD_ATTR_MANAGED,
    PROPERTY_FIELD_ATTR_SAML, PROPERTY_FIELD_ATTR_VALUE_TYPE, PROPERTY_FIELD_ATTR_VISIBILITY,
    PROPERTY_FIELD_VALUE_TYPE_TEXT_MAX_LENGTH, PROPERTY_FIELD_VISIBILITY_WHEN_SET,
    get_property_field_sync_source, get_property_field_value_type,
    is_valid_property_field_value_type, validate_property_field_sort_order,
    validate_property_field_visibility, validate_property_value_for_value_type,
};
use mm_model::property_value::{PropertyValue, PropertyValueSearchCursor, PropertyValueSearchOpts};
use mm_model::session::Session;
use mm_model::utils::{AppError, StringInterface, go_quote, is_valid_id, new_id};
use mm_store::{PropertyStore, StoreError};

use crate::App;

/// `properties.FieldLimitConfig` for the CPA group (app/server.go:361): twenty `user` fields,
/// and `AccessControlGroupFieldLimit` across the whole group.
pub const CPA_USER_FIELD_LIMIT: i64 = 20;

/// `propertyAccessPaginationPageSize` and `propertyAccessMaxPaginationIterations`
/// (access_control.go:43) — the caller-value scans page at 100 and give up after ten pages.
const PAGINATION_PAGE_SIZE: i64 = 100;
const MAX_PAGINATION_ITERATIONS: usize = 10;

/// Who is asking, as the hooks see it: `RequestContextWithCallerID(sessionCallerID(c))`
/// (api4/properties.go:918).
///
/// An unrestricted (local-mode) session has an empty `UserId` and full privileges, so it is
/// tagged `system:local_admin`; everyone else is their user id. `acting_as_scope` is a plugin's
/// declaration on its own request context and is always empty over HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyCaller {
    pub id: String,
    pub acting_as_scope: String,
}

impl PropertyCaller {
    /// Port of `sessionCallerID` (api4/properties.go:918).
    pub fn from_session(session: &Session) -> Self {
        let id = if session.is_unrestricted() {
            CALLER_ID_LOCAL_ADMIN.to_owned()
        } else {
            session.user_id.clone()
        };
        Self {
            id,
            acting_as_scope: String::new(),
        }
    }

    /// `isCallerPlugin` (access_control.go:631): there is no plugin host on this side, so the
    /// answer is always no — and over REST it would be no on Go's side too, since a session's
    /// user id is never a plugin manifest id.
    fn is_plugin(&self) -> bool {
        false
    }

    /// `isMachineCaller` (access_control.go:639).
    fn is_machine(&self) -> bool {
        self.is_plugin() || self.id == CALLER_ID_LDAP_SYNC || self.id == CALLER_ID_SAML_SYNC
    }

    /// `callerOwnerIdentity` (access_control.go:650).
    fn owner_identity(&self) -> (&str, &str, &str) {
        match self.id.as_str() {
            CALLER_ID_LDAP_SYNC => (PROPERTY_FIELD_ATTR_LDAP, PROPERTY_OWNER_TYPE_SERVICE, ""),
            CALLER_ID_SAML_SYNC => (PROPERTY_FIELD_ATTR_SAML, PROPERTY_OWNER_TYPE_SERVICE, ""),
            _ => (
                self.id.as_str(),
                PROPERTY_OWNER_TYPE_PLUGIN,
                self.acting_as_scope.as_str(),
            ),
        }
    }
}

/// Everything the property service and its hooks can fail with, as `mapPropertyServiceError`
/// distinguishes them. The string payloads are Go's `err.Error()` texts, which land in
/// `detailed_error` and are wiped before the wire; they are kept for the trace.
#[derive(Debug, thiserror::Error)]
pub enum PropertyServiceError {
    #[error("{0}: access denied")]
    AccessDenied(String),
    #[error("{0}: field is managed by external sync")]
    SyncLocked(String),
    #[error("{0}: invalid access_mode")]
    InvalidAccessMode(String),
    #[error("{0}: per-object-type field limit reached")]
    FieldLimitReached(String),
    #[error("{0}: group field limit reached")]
    GroupFieldLimitReached(String),
    #[error("license_error: an Enterprise license is required")]
    LicenseRequired,
    #[error("{0}: invalid field attrs")]
    InvalidFieldAttrs(String),
    #[error("{0}: invalid property value")]
    InvalidValue(String),
    #[error("{0}: admin privileges required")]
    AdminRequired(String),
    #[error("field {0}: property field not found")]
    FieldNotFound(String),
    /// A `*model.AppError` raised inside the service, passed through untouched.
    #[error("{}", .0.id)]
    App(Box<AppError>),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<Box<AppError>> for PropertyServiceError {
    fn from(err: Box<AppError>) -> Self {
        Self::App(err)
    }
}

impl PropertyServiceError {
    /// Port of `mapPropertyServiceError` (app/property_errors.go:44) followed by the caller's
    /// own 500 for anything it does not recognise.
    ///
    /// Sentinels first, then the store's three typed errors, then a wrapped `AppError`, then the
    /// fallback. `detailed_error` is left empty on the access-control refusals "to avoid leaking
    /// field IDs, plugin IDs, and sync source names" and carries the message on the rest — all
    /// of which is invisible on the wire once `WipeDetailed` runs, and reproduced anyway.
    pub fn into_app_error(self, where_: &'static str, fallback_id: &'static str) -> Box<AppError> {
        let boxed = |id: &str, detailed: String, status: i32| {
            AppError::boxed(where_, id, None, detailed, status)
        };
        match self {
            Self::AccessDenied(_) => {
                boxed("app.property.access_denied.app_error", String::new(), 403)
            }
            Self::SyncLocked(_) => boxed("app.property.sync_lock.app_error", String::new(), 403),
            Self::InvalidAccessMode(msg) => boxed(
                "app.property.invalid_access_mode.app_error",
                format!("{msg}: invalid access_mode"),
                400,
            ),
            Self::FieldLimitReached(msg) => boxed(
                "app.property_field.create.limit_reached.app_error",
                format!("{msg}: per-object-type field limit reached"),
                422,
            ),
            Self::GroupFieldLimitReached(msg) => boxed(
                "app.property_field.create.group_limit_reached.app_error",
                format!("{msg}: group field limit reached"),
                422,
            ),
            Self::LicenseRequired => boxed("app.property.license_error", String::new(), 403),
            Self::InvalidFieldAttrs(msg) => boxed(
                "app.property_field.invalid_attrs.app_error",
                format!("{msg}: invalid field attrs"),
                400,
            ),
            Self::InvalidValue(msg) => boxed(
                "app.property_value.validate.app_error",
                format!("{msg}: invalid property value"),
                400,
            ),
            Self::AdminRequired(_) => boxed(
                "app.property_field.managed_admin.permission.app_error",
                String::new(),
                403,
            ),
            Self::FieldNotFound(_) => {
                boxed("app.property_field.not_found.app_error", String::new(), 404)
            }
            Self::App(err) => err,
            Self::Store(err) => match err {
                StoreError::Stale { .. } => boxed(
                    "app.property_field.update.conflict.app_error",
                    "concurrent modification detected; please retry".to_owned(),
                    409,
                ),
                StoreError::NotFound { .. } => {
                    boxed("app.property.not_found.app_error", String::new(), 404)
                }
                StoreError::Invalid { app_error, .. } => app_error,
                other => {
                    tracing::error!(error = ?other, where_, "a property store call failed");
                    boxed(fallback_id, String::new(), 500)
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Attribute reads shared by the hooks
// ---------------------------------------------------------------------------------------------

fn attr_str<'a>(field: &'a PropertyField, key: &str) -> &'a str {
    field
        .attrs
        .as_ref()
        .and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

/// `getSourcePluginID` (access_control.go:751).
fn source_plugin_id(field: &PropertyField) -> &str {
    attr_str(field, PROPERTY_ATTRS_SOURCE_PLUGIN_ID)
}

/// `getAccessMode` (access_control.go:760) — a missing key or a non-string is public.
fn access_mode(field: &PropertyField) -> &str {
    match field
        .attrs
        .as_ref()
        .and_then(|a| a.get(PROPERTY_ATTRS_ACCESS_MODE))
        .and_then(|v| v.as_str())
    {
        Some(mode) => mode,
        None => PROPERTY_ACCESS_MODE_PUBLIC,
    }
}

/// `hasUnrestrictedFieldReadAccess` (access_control.go:773): public, or the caller **is** the
/// source plugin.
fn has_unrestricted_field_read_access(field: &PropertyField, caller: &PropertyCaller) -> bool {
    if access_mode(field) == PROPERTY_ACCESS_MODE_PUBLIC {
        return true;
    }
    let source = source_plugin_id(field);
    !source.is_empty() && source == caller.id
}

/// `effectiveOwners` (access_control.go:667): the declared owners plus an implicit `service`
/// owner for each of `ldap`/`saml` — but only when explicit owners exist at all.
fn effective_owners(field: &PropertyField) -> Vec<PropertyOwner> {
    let mut owners = get_property_field_owners(field).unwrap_or_default();
    if owners.is_empty() {
        return owners;
    }
    for key in [PROPERTY_FIELD_ATTR_LDAP, PROPERTY_FIELD_ATTR_SAML] {
        if !attr_str(field, key).is_empty() {
            owners.push(PropertyOwner {
                id: key.to_owned(),
                type_: PROPERTY_OWNER_TYPE_SERVICE.to_owned(),
                scopes: None,
            });
        }
    }
    owners
}

/// `isListedOwner` (access_control.go:717) — id and type, scope not consulted.
fn is_listed_owner(field: &PropertyField, caller: &PropertyCaller) -> bool {
    let (owner_id, owner_type, _) = caller.owner_identity();
    get_property_field_owners(field)
        .unwrap_or_default()
        .iter()
        .any(|owner| owner.type_ == owner_type && owner.id == owner_id)
}

// ---------------------------------------------------------------------------------------------
// Access control: field pre-hooks
// ---------------------------------------------------------------------------------------------

/// `checkLegacyFieldWriteAccess` (access_control.go:823) — the protected/source-plugin rules on a
/// field that has no owners. Always the **existing** row, never the caller's copy.
fn check_legacy_field_write_access(
    field: &PropertyField,
    caller: &PropertyCaller,
) -> Result<(), PropertyServiceError> {
    if !is_property_field_protected(field) {
        return Ok(());
    }
    let source = source_plugin_id(field);
    if source.is_empty() {
        return Err(PropertyServiceError::AccessDenied(format!(
            "field {} is protected, but has no associated source plugin",
            field.id
        )));
    }
    if source != caller.id {
        return Err(PropertyServiceError::AccessDenied(format!(
            "field {} is protected and can only be modified by source plugin '{source}'",
            field.id
        )));
    }
    Ok(())
}

/// `enforceFieldUpdateAccess` (access_control.go:736).
fn enforce_field_update_access(
    existing: &PropertyField,
    updated: &PropertyField,
    caller: &PropertyCaller,
) -> Result<(), PropertyServiceError> {
    if has_property_field_owners(existing) {
        if caller.is_machine() && !is_listed_owner(existing, caller) {
            return Err(PropertyServiceError::AccessDenied(format!(
                "field {} is owner-managed and can only be modified by an administrator or a listed owner",
                existing.id
            )));
        }
        return Ok(());
    }
    if caller.is_machine() && has_property_field_owners(updated) {
        return Err(PropertyServiceError::AccessDenied(
            "owners can only be set by an administrator".to_owned(),
        ));
    }
    check_legacy_field_write_access(existing, caller)
}

/// `ensureSourcePluginIDUnchanged` (access_control.go:789).
fn ensure_source_plugin_id_unchanged(
    existing: &PropertyField,
    updated: &PropertyField,
) -> Result<(), PropertyServiceError> {
    let (before, after) = (source_plugin_id(existing), source_plugin_id(updated));
    if before != after {
        return Err(PropertyServiceError::AccessDenied(format!(
            "source_plugin_id is immutable and cannot be changed from '{before}' to '{after}'"
        )));
    }
    Ok(())
}

/// `validateProtectedFieldUpdate` (access_control.go:801).
fn validate_protected_field_update(
    updated: &PropertyField,
    caller: &PropertyCaller,
) -> Result<(), PropertyServiceError> {
    if !is_property_field_protected(updated) {
        return Ok(());
    }
    let source = source_plugin_id(updated);
    if source.is_empty() {
        return Err(PropertyServiceError::AccessDenied(
            "cannot set protected=true on a field without a source_plugin_id".to_owned(),
        ));
    }
    if source != caller.id {
        return Err(PropertyServiceError::AccessDenied(format!(
            "cannot set protected=true: only source plugin '{source}' can modify this field"
        )));
    }
    Ok(())
}

/// `checkFieldDeleteAccess` (access_control.go:842). See the module docs for the plugin-host
/// arm: with no plugin host, an installed source plugin is unknowable and the field is deletable.
fn check_field_delete_access(
    field: &PropertyField,
    caller: &PropertyCaller,
) -> Result<(), PropertyServiceError> {
    if has_property_field_owners(field) {
        if caller.is_machine() && !is_listed_owner(field, caller) {
            return Err(PropertyServiceError::AccessDenied(format!(
                "field {} is owner-managed and can only be deleted by an administrator or a listed owner",
                field.id
            )));
        }
        return Ok(());
    }
    if !is_property_field_protected(field) {
        return Ok(());
    }
    let source = source_plugin_id(field);
    if source.is_empty() {
        return Ok(());
    }
    // `h.pluginChecker != nil && !h.pluginChecker(sourcePluginID)` — nothing is installed here.
    let plugin_installed = false;
    if !plugin_installed {
        return Ok(());
    }
    if source != caller.id {
        return Err(PropertyServiceError::AccessDenied(format!(
            "field {} is protected and can only be modified by source plugin '{source}'",
            field.id
        )));
    }
    Ok(())
}

/// `checkSyncLock` (access_control.go:874).
fn check_sync_lock(
    field: &PropertyField,
    caller: &PropertyCaller,
) -> Result<(), PropertyServiceError> {
    let source = get_property_field_sync_source(field);
    if source.is_empty() {
        return Ok(());
    }
    let expected = match source {
        "ldap" => CALLER_ID_LDAP_SYNC,
        "saml" => CALLER_ID_SAML_SYNC,
        other => {
            return Err(PropertyServiceError::InvalidFieldAttrs(format!(
                "field {} has unknown sync source {}",
                field.id,
                go_quote(other)
            )));
        }
    };
    if caller.id != expected {
        return Err(PropertyServiceError::SyncLocked(format!(
            "field {} is managed by {source} sync and cannot be modified by caller {}",
            field.id,
            go_quote(&caller.id)
        )));
    }
    Ok(())
}

/// `checkOwnerValueWriteAccess` (access_control.go:698): **every human is refused**, system
/// administrators included — an owner-managed field's values belong to the owning integration.
fn check_owner_value_write_access(
    field: &PropertyField,
    caller: &PropertyCaller,
) -> Result<(), PropertyServiceError> {
    if !caller.is_machine() {
        return Err(PropertyServiceError::AccessDenied(format!(
            "field {} is owner-managed and cannot be modified by human caller {}",
            field.id,
            go_quote(&caller.id)
        )));
    }
    let (owner_id, owner_type, scope) = caller.owner_identity();
    for owner in effective_owners(field) {
        let scope_ok = owner
            .scopes
            .as_ref()
            .is_none_or(|scopes| scopes.is_empty() || scopes.iter().any(|s| s == scope));
        if owner.type_ == owner_type && owner.id == owner_id && scope_ok {
            return Ok(());
        }
    }
    Err(PropertyServiceError::AccessDenied(format!(
        "field {} is owner-managed and caller {} acting as scope {} is not an owner with a matching scope",
        field.id,
        go_quote(&caller.id),
        go_quote(scope)
    )))
}

/// `checkValueWriteAccess` (access_control.go:902): owners supersede both legacy checks.
pub fn check_value_write_access(
    field: &PropertyField,
    caller: &PropertyCaller,
) -> Result<(), PropertyServiceError> {
    if has_property_field_owners(field) {
        return check_owner_value_write_access(field, caller);
    }
    check_legacy_field_write_access(field, caller)?;
    check_sync_lock(field, caller)
}

/// The `AccessControlHook` and `AccessControlAttributeValidationHook` arms that need a store or a
/// permission lookup, as methods.
impl App {
    /// `AccessControlHook.PreCreatePropertyField` (access_control.go:99), for a human caller.
    pub async fn access_control_pre_create_field(
        &self,
        field: &mut PropertyField,
        caller: &PropertyCaller,
    ) -> Result<(), PropertyServiceError> {
        if caller.is_plugin() {
            field.attrs.get_or_insert_with(StringInterface::new).insert(
                PROPERTY_ATTRS_SOURCE_PLUGIN_ID.to_owned(),
                serde_json::Value::String(caller.id.clone()),
            );
        } else {
            if !source_plugin_id(field).is_empty() {
                return Err(PropertyServiceError::AccessDenied(
                    "source_plugin_id can only be set by a plugin".to_owned(),
                ));
            }
            if is_property_field_protected(field) {
                return Err(PropertyServiceError::AccessDenied(
                    "protected can only be set by a plugin".to_owned(),
                ));
            }
        }

        if caller.is_machine() && has_property_field_owners(field) {
            return Err(PropertyServiceError::AccessDenied(
                "owners can only be set by an administrator".to_owned(),
            ));
        }

        if let Some(linked) = field.linked_field_id.clone().filter(|id| !id.is_empty()) {
            self.validate_and_inherit_linked_field_security(&linked, field, caller)
                .await?;
        }

        validate_property_field_access_mode(field)
            .map_err(|err| PropertyServiceError::InvalidAccessMode(err.to_string()))?;
        Ok(())
    }

    /// `validateAndInheritLinkedFieldSecurity` (access_control.go:143).
    async fn validate_and_inherit_linked_field_security(
        &self,
        linked: &str,
        field: &mut PropertyField,
        caller: &PropertyCaller,
    ) -> Result<(), PropertyServiceError> {
        let source = match self.store().property().get_field_in_any_group(linked).await {
            Ok(source) => source,
            Err(err) if err.is_not_found() => {
                return Err(PropertyServiceError::App(AppError::boxed(
                    "CreatePropertyField",
                    "app.property_field.create.linked_source_not_found.app_error",
                    None,
                    format!("linked source field {} not found", go_quote(linked)),
                    400,
                )));
            }
            Err(err) => return Err(PropertyServiceError::Store(err)),
        };

        if source.attrs.is_none() || !is_property_field_protected(&source) {
            return Ok(());
        }
        let source_plugin = source_plugin_id(&source);
        if source_plugin.is_empty() || caller.id != source_plugin {
            return Err(PropertyServiceError::App(AppError::boxed(
                "CreatePropertyField",
                "app.property_field.create.linked_source_protected.app_error",
                None,
                "only the source plugin can create linked fields from a protected template"
                    .to_owned(),
                403,
            )));
        }

        let attrs = field.attrs.get_or_insert_with(StringInterface::new);
        attrs.insert(
            PROPERTY_ATTRS_PROTECTED.to_owned(),
            serde_json::Value::Bool(true),
        );
        attrs.insert(
            PROPERTY_ATTRS_SOURCE_PLUGIN_ID.to_owned(),
            serde_json::Value::String(source_plugin.to_owned()),
        );
        if let Some(mode) = source
            .attrs
            .as_ref()
            .and_then(|a| a.get(PROPERTY_ATTRS_ACCESS_MODE))
        {
            attrs.insert(PROPERTY_ATTRS_ACCESS_MODE.to_owned(), mode.clone());
        }
        Ok(())
    }

    /// `AccessControlHook.PreUpdatePropertyFields` (access_control.go:221) for one field, with
    /// `existing` the raw row (no post-get hook has touched it).
    pub fn access_control_pre_update_field(
        &self,
        existing: &PropertyField,
        field: &PropertyField,
        caller: &PropertyCaller,
    ) -> Result<(), PropertyServiceError> {
        enforce_field_update_access(existing, field, caller)?;
        ensure_source_plugin_id_unchanged(existing, field)?;
        validate_protected_field_update(field, caller)?;
        validate_property_field_access_mode(field)
            .map_err(|err| PropertyServiceError::InvalidAccessMode(err.to_string()))
    }

    /// `AccessControlHook.PreDeletePropertyField` (access_control.go:277), `existing` raw.
    pub fn access_control_pre_delete_field(
        &self,
        existing: &PropertyField,
        caller: &PropertyCaller,
    ) -> Result<(), PropertyServiceError> {
        check_field_delete_access(existing, caller)
    }

    // -----------------------------------------------------------------------------------------
    // Access control: read filtering
    // -----------------------------------------------------------------------------------------

    /// `applyFieldReadAccessControl` (access_control.go:1348): public fields and the source
    /// plugin's own see everything; `shared_only` sees the options the caller holds; anything
    /// else — `source_only` or an unknown mode — sees **empty options**, the secure default.
    pub async fn apply_field_read_access_control(
        &self,
        field: PropertyField,
        caller: &PropertyCaller,
    ) -> PropertyField {
        if has_unrestricted_field_read_access(&field, caller) {
            return field;
        }
        if access_mode(&field) == PROPERTY_ACCESS_MODE_SHARED_ONLY {
            return self.filter_shared_only_field_options(field, caller).await;
        }
        let supports_options = field.type_.supports_options();
        let mut filtered = copy_property_field(&field);
        if supports_options {
            filtered
                .attrs
                .get_or_insert_with(StringInterface::new)
                .insert(
                    PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_owned(),
                    serde_json::Value::Array(Vec::new()),
                );
        }
        filtered
    }

    /// `applyFieldReadAccessControlToList` (access_control.go:1368).
    pub async fn apply_field_read_access_control_to_list(
        &self,
        fields: Vec<PropertyField>,
        caller: &PropertyCaller,
    ) -> Vec<PropertyField> {
        let mut filtered = Vec::with_capacity(fields.len());
        for field in fields {
            filtered.push(self.apply_field_read_access_control(field, caller).await);
        }
        filtered
    }

    /// `filterSharedOnlyFieldOptions` (access_control.go:1042).
    async fn filter_shared_only_field_options(
        &self,
        field: PropertyField,
        caller: &PropertyCaller,
    ) -> PropertyField {
        if !field.type_.supports_options() {
            return field;
        }
        if field.type_.as_str() == PropertyFieldType::RANK {
            return self
                .filter_shared_only_rank_field_options(field, caller)
                .await;
        }

        let caller_option_ids = self.caller_option_ids_for_field(&field, caller).await;
        let caller_option_ids = match caller_option_ids {
            Ok(ids) if !ids.is_empty() => ids,
            _ => {
                let mut filtered = copy_property_field(&field);
                filtered
                    .attrs
                    .get_or_insert_with(StringInterface::new)
                    .insert(
                        PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_owned(),
                        serde_json::Value::Array(Vec::new()),
                    );
                return filtered;
            }
        };

        let Some(options) = field
            .attrs
            .as_ref()
            .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS))
            .and_then(|o| o.as_array())
        else {
            return field;
        };
        let filtered_options: Vec<serde_json::Value> = options
            .iter()
            .filter(|opt| {
                opt.as_object()
                    .and_then(|m| m.get("id"))
                    .and_then(|id| id.as_str())
                    .is_some_and(|id| caller_option_ids.contains(id))
            })
            .cloned()
            .collect();
        let mut filtered = copy_property_field(&field);
        filtered
            .attrs
            .get_or_insert_with(StringInterface::new)
            .insert(
                PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_owned(),
                serde_json::Value::Array(filtered_options),
            );
        filtered
    }

    /// `filterSharedOnlyRankFieldOptions` (access_control.go:1095): every option at or below the
    /// caller's own rank; a caller with no rank sees none.
    async fn filter_shared_only_rank_field_options(
        &self,
        field: PropertyField,
        caller: &PropertyCaller,
    ) -> PropertyField {
        let Some(options) = field
            .attrs
            .as_ref()
            .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS))
            .and_then(|o| o.as_array())
            .cloned()
        else {
            return field;
        };

        let rank_by_id = build_option_rank_map(&field);
        let Some(caller_rank) = self
            .caller_rank_for_field(&field, caller, &rank_by_id)
            .await
        else {
            let mut filtered = copy_property_field(&field);
            filtered
                .attrs
                .get_or_insert_with(StringInterface::new)
                .insert(
                    PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_owned(),
                    serde_json::Value::Array(Vec::new()),
                );
            return filtered;
        };

        let filtered_options: Vec<serde_json::Value> = options
            .into_iter()
            .filter(|opt| {
                opt.as_object()
                    .and_then(|m| m.get("id"))
                    .and_then(|id| id.as_str())
                    .and_then(|id| rank_by_id.get(id))
                    .is_some_and(|rank| *rank <= caller_rank)
            })
            .collect();
        let mut filtered = copy_property_field(&field);
        filtered
            .attrs
            .get_or_insert_with(StringInterface::new)
            .insert(
                PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_owned(),
                serde_json::Value::Array(filtered_options),
            );
        filtered
    }

    /// `callerRankForField` (access_control.go:1148).
    async fn caller_rank_for_field(
        &self,
        field: &PropertyField,
        caller: &PropertyCaller,
        rank_by_id: &HashMap<String, i64>,
    ) -> Option<i64> {
        let ids = self.caller_option_ids_for_field(field, caller).await.ok()?;
        // A rank field is select-shaped, so the caller holds at most one option.
        let caller_option_id = ids.into_iter().next()?;
        rank_by_id.get(&caller_option_id).copied()
    }

    /// `getCallerValuesForField` (access_control.go:914): the caller's own values on this
    /// field, paged at 100 and capped at ten pages. An empty caller id has no values.
    async fn caller_values_for_field(
        &self,
        field: &PropertyField,
        caller: &PropertyCaller,
    ) -> Result<Vec<PropertyValue>, PropertyServiceError> {
        if caller.id.is_empty() {
            return Ok(Vec::new());
        }
        let mut all = Vec::new();
        let mut cursor = PropertyValueSearchCursor::default();
        for iteration in 0..=MAX_PAGINATION_ITERATIONS {
            if iteration == MAX_PAGINATION_ITERATIONS {
                return Err(PropertyServiceError::App(AppError::boxed(
                    "getCallerValuesForField",
                    "app.property_value.search.app_error",
                    None,
                    format!("exceeded maximum pagination iterations ({MAX_PAGINATION_ITERATIONS})"),
                    500,
                )));
            }
            let opts = PropertyValueSearchOpts {
                group_id: field.group_id.clone(),
                field_id: field.id.clone(),
                target_ids: vec![caller.id.clone()],
                per_page: PAGINATION_PAGE_SIZE,
                cursor: cursor.clone(),
                ..PropertyValueSearchOpts::default()
            };
            let page = self.store().property().search_values(&opts).await?;
            let page_len = page.len() as i64;
            let last = page.last().map(|v| (v.id.clone(), v.create_at));
            all.extend(page);
            if page_len < PAGINATION_PAGE_SIZE {
                break;
            }
            let Some((id, create_at)) = last else { break };
            cursor = PropertyValueSearchCursor {
                property_value_id: id,
                create_at,
                ..PropertyValueSearchCursor::default()
            };
        }
        Ok(all)
    }

    /// `getCallerOptionIDsForField` (access_control.go:1013).
    async fn caller_option_ids_for_field(
        &self,
        field: &PropertyField,
        caller: &PropertyCaller,
    ) -> Result<HashSet<String>, PropertyServiceError> {
        let values = self.caller_values_for_field(field, caller).await?;
        let mut ids = HashSet::new();
        for value in values {
            if let Ok(Some(option_ids)) = extract_option_ids_from_value(&field.type_, &value.value)
            {
                ids.extend(option_ids);
            }
        }
        Ok(ids)
    }

    /// `applyValueReadAccessControl` (access_control.go:1416): a value on a public field passes;
    /// on a `shared_only` field it is the intersection with the caller's own value; on a
    /// `source_only` field it is **dropped**, silently.
    pub async fn apply_value_read_access_control(
        &self,
        values: Vec<PropertyValue>,
        caller: &PropertyCaller,
    ) -> Result<Vec<PropertyValue>, PropertyServiceError> {
        if values.is_empty() {
            return Ok(values);
        }
        let field_map = self.fields_for_values(&values).await?;
        let mut filtered = Vec::with_capacity(values.len());
        for value in values {
            let Some(field) = field_map.get(&value.field_id) else {
                return Err(PropertyServiceError::App(AppError::boxed(
                    "applyValueReadAccessControl",
                    "app.property_value.search.app_error",
                    None,
                    format!("field not found for value {}", value.id),
                    500,
                )));
            };
            if has_unrestricted_field_read_access(field, caller) {
                filtered.push(value);
            } else if access_mode(field) == PROPERTY_ACCESS_MODE_SHARED_ONLY {
                if let Some(shared) = self.filter_shared_only_value(field, value, caller).await {
                    filtered.push(shared);
                }
            }
        }
        Ok(filtered)
    }

    /// `getFieldsForValues` (access_control.go:1382), keyed by field id. The raw rows — no
    /// post-get hook runs inside a hook.
    pub async fn fields_for_values(
        &self,
        values: &[PropertyValue],
    ) -> Result<HashMap<String, PropertyField>, PropertyServiceError> {
        let mut by_group: HashMap<&str, Vec<String>> = HashMap::new();
        for value in values {
            let ids = by_group.entry(value.group_id.as_str()).or_default();
            if !ids.contains(&value.field_id) {
                ids.push(value.field_id.clone());
            }
        }
        let mut map = HashMap::new();
        for (group_id, ids) in by_group {
            let fields = self.get_property_fields_raw(group_id, &ids).await?;
            for field in fields {
                map.insert(field.id.clone(), field);
            }
        }
        Ok(map)
    }

    /// `PropertyService.getPropertyFields` (property_field.go:260): the store read plus the
    /// cardinality check the store leaves to its caller — fewer rows than ids is
    /// `ErrFieldNotFound`.
    pub async fn get_property_fields_raw(
        &self,
        group_id: &str,
        ids: &[String],
    ) -> Result<Vec<PropertyField>, PropertyServiceError> {
        let fields = self
            .store()
            .property()
            .get_many_fields(group_id, ids)
            .await?;
        if fields.len() < ids.len() {
            return Err(PropertyServiceError::FieldNotFound(String::new()));
        }
        Ok(fields)
    }

    /// `filterSharedOnlyValue` (access_control.go:1200).
    async fn filter_shared_only_value(
        &self,
        field: &PropertyField,
        value: PropertyValue,
        caller: &PropertyCaller,
    ) -> Option<PropertyValue> {
        match field.type_.as_str() {
            PropertyFieldType::RANK => {
                self.filter_shared_only_rank_value(field, value, caller)
                    .await
            }
            PropertyFieldType::SELECT | PropertyFieldType::MULTISELECT => {
                let caller_ids = self.caller_option_ids_for_field(field, caller).await.ok()?;
                if caller_ids.is_empty() {
                    return None;
                }
                let target_ids = extract_option_ids_from_value(&field.type_, &value.value)
                    .ok()
                    .flatten()?;
                if target_ids.is_empty() {
                    return None;
                }
                // Go iterates a map, so the intersection's order is arbitrary; a select value
                // takes "the first" of it. With one target option there is nothing to choose.
                let intersection: Vec<String> = target_ids
                    .into_iter()
                    .filter(|id| caller_ids.contains(id))
                    .collect();
                if intersection.is_empty() {
                    return None;
                }
                let mut filtered = value;
                filtered.value = if field.type_.as_str() == PropertyFieldType::SELECT {
                    serde_json::Value::String(intersection.into_iter().next()?)
                } else {
                    serde_json::Value::Array(
                        intersection
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    )
                };
                Some(filtered)
            }
            _ => {
                self.filter_shared_only_scalar_value(field, value, caller)
                    .await
            }
        }
    }

    /// `filterSharedOnlyRankValue` (access_control.go:1264): the target's value when it is at or
    /// below the caller's rank, otherwise clamped down to the option at the caller's rank.
    async fn filter_shared_only_rank_value(
        &self,
        field: &PropertyField,
        value: PropertyValue,
        caller: &PropertyCaller,
    ) -> Option<PropertyValue> {
        let rank_by_id = build_option_rank_map(field);
        let caller_rank = self
            .caller_rank_for_field(field, caller, &rank_by_id)
            .await?;
        let target_ids = extract_option_ids_from_value(&field.type_, &value.value)
            .ok()
            .flatten()?;
        for target_id in target_ids {
            let Some(target_rank) = rank_by_id.get(&target_id) else {
                continue;
            };
            if *target_rank <= caller_rank {
                return Some(value);
            }
            return clamp_rank_value_to_rank(value, &rank_by_id, caller_rank);
        }
        None
    }

    /// `filterSharedOnlyScalarValue` (access_control.go:1322): visible only when the caller's
    /// own value for the field is **exactly** the target's.
    async fn filter_shared_only_scalar_value(
        &self,
        field: &PropertyField,
        value: PropertyValue,
        caller: &PropertyCaller,
    ) -> Option<PropertyValue> {
        if value.value.is_null() {
            return None;
        }
        let caller_values = self.caller_values_for_field(field, caller).await.ok()?;
        if caller_values.is_empty() {
            return None;
        }
        caller_values
            .iter()
            .any(|cv| cv.value == value.value)
            .then_some(value)
    }

    // -----------------------------------------------------------------------------------------
    // Attribute validation
    // -----------------------------------------------------------------------------------------

    /// `AccessControlAttributeValidationHook.PreCreatePropertyField` (attribute_validation.go:447).
    pub async fn attribute_validation_pre_create_field(
        &self,
        field: &mut PropertyField,
        caller: &PropertyCaller,
    ) -> Result<(), PropertyServiceError> {
        mm_model::custom_profile_attributes::validate_cpa_field_name(&field.name)?;
        sanitize_and_validate_field_attrs(field, "")?;
        self.enforce_group_permissions(field, caller).await
    }

    /// `AccessControlAttributeValidationHook.PreUpdatePropertyFields` (attribute_validation.go:495)
    /// for one field, `existing` the raw row. The name is re-validated **only when it changes**,
    /// so a field whose name predates the CEL rules stays editable on its other attrs.
    pub async fn attribute_validation_pre_update_field(
        &self,
        existing: &PropertyField,
        field: &mut PropertyField,
        caller: &PropertyCaller,
    ) -> Result<(), PropertyServiceError> {
        if existing.name != field.name {
            mm_model::custom_profile_attributes::validate_cpa_field_name(&field.name)?;
        }
        sanitize_and_validate_field_attrs(field, existing.type_.as_str())?;
        self.enforce_group_permissions(field, caller).await
    }

    /// `enforceGroupPermissions` (attribute_validation.go:405).
    ///
    /// `managed=admin` needs `manage_system` on the **caller id**, resolved without a user lookup
    /// for the local admin; an unidentifiable caller is refused rather than promoted. Owners pin
    /// `PermissionValues` to sysadmin; otherwise a nil `PermissionValues` defaults by object type.
    /// `PermissionField` and `PermissionOptions` are always sysadmin in this group.
    async fn enforce_group_permissions(
        &self,
        field: &mut PropertyField,
        caller: &PropertyCaller,
    ) -> Result<(), PropertyServiceError> {
        let sysadmin = PermissionLevel(PermissionLevel::SYSADMIN.to_owned());
        if attr_str(field, PROPERTY_FIELD_ATTR_MANAGED) == "admin" {
            let allowed = !caller.id.is_empty()
                && (caller.id == CALLER_ID_LOCAL_ADMIN
                    || self
                        .has_permission_to(&caller.id, &PERMISSION_MANAGE_SYSTEM)
                        .await);
            if !allowed {
                return Err(PropertyServiceError::AdminRequired(
                    "missing permission to set managed=admin: only system admins can set managed=admin"
                        .to_owned(),
                ));
            }
            field.permission_values = Some(sysadmin.clone());
        } else if has_property_field_owners(field) {
            field.permission_values = Some(sysadmin.clone());
        } else if field.permission_values.is_none() {
            field.permission_values = Some(default_permission_values_for_object_type(
                &field.object_type,
            ));
        }
        field.permission_field = Some(sysadmin.clone());
        field.permission_options = Some(sysadmin);
        Ok(())
    }

    /// `validateValues` (attribute_validation.go:657) over raw fields already fetched.
    pub fn attribute_validation_values(
        &self,
        values: &[PropertyValue],
        fields: &HashMap<String, PropertyField>,
    ) -> Result<(), PropertyServiceError> {
        for value in values {
            let Some(field) = fields.get(&value.field_id) else {
                return Err(PropertyServiceError::FieldNotFound(value.field_id.clone()));
            };
            if let Err(reason) = validate_value_against_field(field, value) {
                return Err(PropertyServiceError::InvalidValue(format!(
                    "field {}: {reason}",
                    value.field_id
                )));
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Field limit
    // -----------------------------------------------------------------------------------------

    /// `FieldLimitHook.PreCreatePropertyField` (field_limit.go:70) with the CPA group's limits:
    /// the per-object-type cap first, then the whole-group cap. Both are `>=`, so the twentieth
    /// `user` field is the last one accepted.
    pub async fn field_limit_pre_create_field(
        &self,
        field: &PropertyField,
    ) -> Result<(), PropertyServiceError> {
        if field.object_type == PROPERTY_FIELD_OBJECT_TYPE_USER {
            let count = self
                .store()
                .property()
                .count_fields_for_group_object_type(&field.group_id, &field.object_type, false)
                .await?;
            if count >= CPA_USER_FIELD_LIMIT {
                return Err(PropertyServiceError::FieldLimitReached(format!(
                    "limit_reached: field limit of {CPA_USER_FIELD_LIMIT} reached for object type {}",
                    go_quote(&field.object_type)
                )));
            }
        }
        let limit = mm_model::property_group::ACCESS_CONTROL_GROUP_FIELD_LIMIT;
        let count = self
            .store()
            .property()
            .count_fields_for_group(&field.group_id, false)
            .await?;
        if count >= limit {
            return Err(PropertyServiceError::GroupFieldLimitReached(format!(
                "group_limit_reached: global field limit of {limit} reached for group"
            )));
        }
        Ok(())
    }
}

/// `copyPropertyField` (access_control.go:1003): a shallow copy with its own attrs map.
fn copy_property_field(field: &PropertyField) -> PropertyField {
    let mut copied = field.clone();
    copied.attrs = Some(field.attrs.clone().unwrap_or_default());
    copied
}

/// `extractOptionIDsFromValue` (access_control.go:961). `None` for an empty value; an error for
/// a type that has no options or a value of the wrong shape.
fn extract_option_ids_from_value(
    field_type: &PropertyFieldType,
    value: &serde_json::Value,
) -> Result<Option<HashSet<String>>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let mut ids = HashSet::new();
    match field_type.as_str() {
        PropertyFieldType::SELECT | PropertyFieldType::RANK => {
            let id = value
                .as_str()
                .ok_or_else(|| "expected a string".to_owned())?;
            if !id.is_empty() {
                ids.insert(id.to_owned());
            }
        }
        PropertyFieldType::MULTISELECT => {
            let list: Vec<String> =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            ids.extend(list.into_iter().filter(|id| !id.is_empty()));
        }
        other => {
            return Err(format!(
                "extractOptionIDsFromValue only supports select, multiselect and rank field types, got: {other}"
            ));
        }
    }
    Ok(Some(ids))
}

/// `buildOptionRankMap` (access_control.go:1166): option id → rank, options without a rank
/// skipped, and an options blob that does not decode contributes nothing.
fn build_option_rank_map(field: &PropertyField) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    let Some(raw) = field
        .attrs
        .as_ref()
        .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS))
    else {
        return out;
    };
    let Ok(options) = PropertyOptions::<CustomProfileAttributesSelectOption>::from_field_attrs(raw)
    else {
        return out;
    };
    for option in options.0 {
        if let Some(rank) = option.rank {
            out.insert(option.id, rank);
        }
    }
    out
}

/// `clampRankValueToRank` (access_control.go:1296).
fn clamp_rank_value_to_rank(
    value: PropertyValue,
    rank_by_id: &HashMap<String, i64>,
    rank: i64,
) -> Option<PropertyValue> {
    let option_id = rank_by_id
        .iter()
        .find(|(_, r)| **r == rank)
        .map(|(id, _)| id.clone())?;
    let mut clamped = value;
    clamped.value = serde_json::Value::String(option_id);
    Some(clamped)
}

/// `defaultPermissionValuesForObjectType` (attribute_validation.go:438).
fn default_permission_values_for_object_type(object_type: &str) -> PermissionLevel {
    match object_type {
        PROPERTY_FIELD_OBJECT_TYPE_SYSTEM | PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE => {
            PermissionLevel(PermissionLevel::SYSADMIN.to_owned())
        }
        _ => PermissionLevel(PermissionLevel::MEMBER.to_owned()),
    }
}

/// `trimmedFieldAttrKeys` (attribute_validation.go:145).
const TRIMMED_FIELD_ATTR_KEYS: [&str; 6] = [
    PROPERTY_FIELD_ATTR_VISIBILITY,
    PROPERTY_FIELD_ATTR_VALUE_TYPE,
    PROPERTY_FIELD_ATTR_MANAGED,
    PROPERTY_FIELD_ATTR_LDAP,
    PROPERTY_FIELD_ATTR_SAML,
    PROPERTY_FIELD_ATTR_DISPLAY_NAME,
];

/// `sanitizeAndValidateFieldAttrs` (attribute_validation.go:84). Mutates `field.attrs` in place;
/// `prev_type` is `""` on create.
///
/// The order is the contract: trim, default the visibility, clear the attrs the type cannot
/// carry (options off a non-select field, `ldap`/`saml` off a non-text one), then validate
/// visibility, `value_type` (text only), `managed`, owners, `display_name`, options (select-shaped
/// only) and `sort_order` — so a body wrong in two places reports the earlier one.
pub fn sanitize_and_validate_field_attrs(
    field: &mut PropertyField,
    prev_type: &str,
) -> Result<(), PropertyServiceError> {
    let is_select = field.type_.supports_options();
    let is_text = field.type_.as_str() == PropertyFieldType::TEXT;
    let is_rank = field.type_.as_str() == PropertyFieldType::RANK;

    let attrs = field.attrs.get_or_insert_with(StringInterface::new);
    for key in TRIMMED_FIELD_ATTR_KEYS {
        if let Some(serde_json::Value::String(s)) = attrs.get(key) {
            let trimmed = s.trim().to_owned();
            attrs.insert(key.to_owned(), serde_json::Value::String(trimmed));
        }
    }
    let visibility_unset = attrs
        .get(PROPERTY_FIELD_ATTR_VISIBILITY)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .is_empty();
    if visibility_unset {
        attrs.insert(
            PROPERTY_FIELD_ATTR_VISIBILITY.to_owned(),
            serde_json::Value::String(PROPERTY_FIELD_VISIBILITY_WHEN_SET.to_owned()),
        );
    }
    let managed = attrs
        .get(PROPERTY_FIELD_ATTR_MANAGED)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    if !is_select {
        attrs.remove(PROPERTY_FIELD_ATTRIBUTE_OPTIONS);
    }
    if !is_text {
        attrs.remove(PROPERTY_FIELD_ATTR_LDAP);
        attrs.remove(PROPERTY_FIELD_ATTR_SAML);
    }

    validate_property_field_visibility(field)
        .map_err(|err| PropertyServiceError::InvalidFieldAttrs(err.to_string()))?;
    if is_text {
        let value_type = attr_str(field, PROPERTY_FIELD_ATTR_VALUE_TYPE);
        if !value_type.is_empty() && !is_valid_property_field_value_type(value_type) {
            return Err(PropertyServiceError::InvalidFieldAttrs(format!(
                "invalid value_type {}",
                go_quote(value_type)
            )));
        }
    }
    if !managed.is_empty() && managed != "admin" {
        return Err(PropertyServiceError::InvalidFieldAttrs(format!(
            "invalid managed {} (must be empty or {})",
            go_quote(&managed),
            go_quote("admin")
        )));
    }
    sanitize_and_validate_owners(field)?;
    let display_name_runes = attr_str(field, PROPERTY_FIELD_ATTR_DISPLAY_NAME)
        .chars()
        .count();
    if display_name_runes > PROPERTY_FIELD_NAME_MAX_RUNES {
        return Err(PropertyServiceError::InvalidFieldAttrs(format!(
            "display_name exceeds max length of {PROPERTY_FIELD_NAME_MAX_RUNES} runes"
        )));
    }
    if is_select {
        sanitize_and_validate_options(field, prev_type, is_rank)?;
    }
    validate_property_field_sort_order(field)
        .map_err(|err| PropertyServiceError::InvalidFieldAttrs(err.to_string()))
}

/// `sanitizeAndValidateOptions` (attribute_validation.go:160): decode the options as CPA select
/// options, repair or validate ranks, mint missing ids, validate, and write the canonical shape
/// back — `[]any` of `map[string]any` — so every reader of `attrs.options` sees one form.
fn sanitize_and_validate_options(
    field: &mut PropertyField,
    prev_type: &str,
    is_rank: bool,
) -> Result<(), PropertyServiceError> {
    let Some(raw) = field
        .attrs
        .as_ref()
        .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS))
        .filter(|v| !v.is_null())
        .cloned()
    else {
        return Ok(());
    };

    let mut options: PropertyOptions<CustomProfileAttributesSelectOption> =
        serde_json::from_value(raw).map_err(|err| {
            PropertyServiceError::InvalidFieldAttrs(format!("invalid options: {err}"))
        })?;

    if is_rank {
        if !prev_type.is_empty() && prev_type != PropertyFieldType::RANK {
            normalize_option_ranks(&mut options.0);
        }
        validate_rank_options(&options.0)?;
    }

    for option in options.0.iter_mut() {
        if option.id.is_empty() {
            option.id = new_id();
        }
    }
    options.is_valid().map_err(|err| {
        PropertyServiceError::InvalidFieldAttrs(format!("invalid options: {err}"))
    })?;

    let canonical = serde_json::to_value(&options).map_err(|err| {
        PropertyServiceError::InvalidFieldAttrs(format!("invalid options: {err}"))
    })?;
    field
        .attrs
        .get_or_insert_with(StringInterface::new)
        .insert(PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_owned(), canonical);
    Ok(())
}

/// `validateRankOptions` (attribute_validation.go:339): every option carries a positive, unique
/// rank.
fn validate_rank_options(
    options: &[CustomProfileAttributesSelectOption],
) -> Result<(), PropertyServiceError> {
    let mut seen = HashSet::new();
    for (i, option) in options.iter().enumerate() {
        let Some(rank) = option.rank else {
            return Err(PropertyServiceError::InvalidFieldAttrs(format!(
                "invalid options: option at index {i} is missing rank for rank field"
            )));
        };
        if rank <= 0 {
            return Err(PropertyServiceError::InvalidFieldAttrs(format!(
                "invalid options: option rank must be a positive integer, got {rank} at index {i}"
            )));
        }
        if !seen.insert(rank) {
            return Err(PropertyServiceError::InvalidFieldAttrs(format!(
                "invalid options: duplicate option rank {rank} at index {i}"
            )));
        }
    }
    Ok(())
}

/// `normalizeOptionRanks` (attribute_validation.go:365): renumber to a gap-free `1..N` in the
/// existing order — stable, so duplicate ranks keep array order and rank-less options sort last.
fn normalize_option_ranks(options: &mut [CustomProfileAttributesSelectOption]) {
    let mut order: Vec<usize> = (0..options.len()).collect();
    order.sort_by_key(|&i| options[i].rank.unwrap_or(i64::MAX));
    for (seq, idx) in order.into_iter().enumerate() {
        options[idx].rank = Some(seq as i64 + 1);
    }
}

/// `sanitizeAndValidateOwners` (attribute_validation.go:244).
fn sanitize_and_validate_owners(field: &mut PropertyField) -> Result<(), PropertyServiceError> {
    let Some(raw) = field
        .attrs
        .as_ref()
        .and_then(|a| a.get(PROPERTY_ATTRS_OWNERS))
        .filter(|v| !v.is_null())
        .cloned()
    else {
        return Ok(());
    };
    let invalid =
        |msg: String| PropertyServiceError::InvalidFieldAttrs(format!("invalid owners: {msg}"));

    let owners: Vec<PropertyOwner> =
        serde_json::from_value(raw).map_err(|err| invalid(err.to_string()))?;
    let attrs = field.attrs.get_or_insert_with(StringInterface::new);
    if owners.is_empty() {
        attrs.remove(PROPERTY_ATTRS_OWNERS);
        return Ok(());
    }
    if attrs
        .get(PROPERTY_FIELD_ATTR_MANAGED)
        .and_then(|v| v.as_str())
        == Some("admin")
    {
        return Err(PropertyServiceError::InvalidFieldAttrs(
            "owners cannot be combined with managed=admin".to_owned(),
        ));
    }

    let mut normalized: Vec<PropertyOwner> = Vec::with_capacity(owners.len());
    let mut index_by_key: HashMap<String, usize> = HashMap::new();
    for mut owner in owners {
        owner.id = owner.id.trim().to_owned();
        owner.type_ = owner.type_.trim().to_owned();
        if owner.id.is_empty() {
            return Err(invalid("owner id cannot be empty".to_owned()));
        }
        if owner.id.chars().count() > PROPERTY_OWNER_ID_MAX_RUNES {
            return Err(invalid(format!(
                "owner id exceeds max length of {PROPERTY_OWNER_ID_MAX_RUNES} runes"
            )));
        }
        if !is_valid_property_owner_type(&owner.type_) {
            return Err(invalid(format!(
                "unknown owner type {}",
                go_quote(&owner.type_)
            )));
        }
        let mut scopes: Vec<String> = Vec::new();
        for scope in owner.scopes.unwrap_or_default() {
            let scope = scope.trim().to_owned();
            if scope.is_empty() || scopes.contains(&scope) {
                continue;
            }
            if scope.chars().count() > PROPERTY_OWNER_SCOPE_MAX_RUNES {
                return Err(invalid(format!(
                    "scope exceeds max length of {PROPERTY_OWNER_SCOPE_MAX_RUNES} runes"
                )));
            }
            if !is_valid_property_owner_scope(&scope) {
                return Err(invalid(format!(
                    "scope {} contains invalid characters",
                    go_quote(&scope)
                )));
            }
            scopes.push(scope);
        }
        let key = format!("{}\0{}", owner.type_, owner.id);
        if let Some(&idx) = index_by_key.get(&key) {
            let existing = normalized[idx].scopes.get_or_insert_with(Vec::new);
            for scope in scopes {
                if !existing.contains(&scope) {
                    existing.push(scope);
                }
            }
            continue;
        }
        index_by_key.insert(key, normalized.len());
        normalized.push(PropertyOwner {
            id: owner.id,
            type_: owner.type_,
            scopes: Some(scopes),
        });
    }

    if normalized.len() > PROPERTY_OWNERS_MAX_PER_FIELD {
        return Err(invalid(format!(
            "too many owners ({}), max is {PROPERTY_OWNERS_MAX_PER_FIELD}",
            normalized.len()
        )));
    }
    for owner in &normalized {
        let count = owner.scopes.as_ref().map_or(0, Vec::len);
        if count > PROPERTY_OWNER_SCOPES_MAX {
            return Err(invalid(format!(
                "owner {} has too many scopes ({count}), max is {PROPERTY_OWNER_SCOPES_MAX}",
                go_quote(&owner.id)
            )));
        }
    }

    let canonical = serde_json::to_value(&normalized).map_err(|err| invalid(err.to_string()))?;
    attrs.insert(PROPERTY_ATTRS_OWNERS.to_owned(), canonical);
    Ok(())
}

/// `extractOptionIDs` (attribute_validation.go:547) — the ids a select-shaped field offers.
fn extract_option_ids(field: &PropertyField) -> Result<HashSet<String>, String> {
    let Some(raw) = field
        .attrs
        .as_ref()
        .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS))
        .filter(|v| !v.is_null())
    else {
        return Ok(HashSet::new());
    };
    #[derive(serde::Deserialize)]
    struct OnlyId {
        #[serde(default)]
        id: String,
    }
    let options: Vec<OnlyId> = serde_json::from_value(raw.clone())
        .map_err(|err| format!("invalid options format: {err}"))?;
    Ok(options
        .into_iter()
        .filter(|o| !o.id.is_empty())
        .map(|o| o.id)
        .collect())
}

/// `validateValueAgainstField` (attribute_validation.go:585). The message is Go's, because it
/// is what `ErrInvalidValue` wraps; a `date` field — or any other type — is not checked.
pub fn validate_value_against_field(
    field: &PropertyField,
    value: &PropertyValue,
) -> Result<(), String> {
    match field.type_.as_str() {
        PropertyFieldType::TEXT => {
            let text = value
                .value
                .as_str()
                .ok_or_else(|| "expected string value".to_owned())?;
            if text.trim().len() > PROPERTY_FIELD_VALUE_TYPE_TEXT_MAX_LENGTH {
                return Err(format!(
                    "text value exceeds maximum length of {PROPERTY_FIELD_VALUE_TYPE_TEXT_MAX_LENGTH} characters"
                ));
            }
            let value_type = get_property_field_value_type(field);
            if value_type.is_empty() {
                return Ok(());
            }
            validate_property_value_for_value_type(value_type, &value.value)
                .map_err(|err| err.to_string())
        }
        PropertyFieldType::SELECT | PropertyFieldType::RANK => {
            let id = value
                .value
                .as_str()
                .ok_or_else(|| "expected string value for select field".to_owned())?;
            if id.is_empty() {
                return Ok(());
            }
            let ids =
                extract_option_ids(field).map_err(|e| format!("failed to extract options: {e}"))?;
            if !ids.contains(id) {
                return Err(format!("option {} does not exist", go_quote(id)));
            }
            Ok(())
        }
        PropertyFieldType::MULTISELECT => {
            let list: Vec<String> = serde_json::from_value(value.value.clone())
                .map_err(|_| "expected string array value for multiselect field".to_owned())?;
            let ids =
                extract_option_ids(field).map_err(|e| format!("failed to extract options: {e}"))?;
            for id in list {
                if !ids.contains(&id) {
                    return Err(format!("option {} does not exist", go_quote(&id)));
                }
            }
            Ok(())
        }
        PropertyFieldType::USER => {
            let id = value
                .value
                .as_str()
                .ok_or_else(|| "expected string value for user field".to_owned())?;
            if !id.is_empty() && !is_valid_id(id) {
                return Err("invalid user id".to_owned());
            }
            Ok(())
        }
        PropertyFieldType::MULTIUSER => {
            let list: Vec<String> = serde_json::from_value(value.value.clone())
                .map_err(|_| "expected string array value for multiuser field".to_owned())?;
            for id in list {
                if !is_valid_id(&id) {
                    return Err(format!("invalid user id: {id}"));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `isSelectRankTransition` (type_change_value_cleanup.go:944): both store one option id, so a
/// change between them keeps the values.
pub fn is_select_rank_transition(from: &str, to: &str) -> bool {
    let select_shaped = |t: &str| t == PropertyFieldType::SELECT || t == PropertyFieldType::RANK;
    select_shaped(from) && select_shaped(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(type_: &str, attrs: serde_json::Value) -> PropertyField {
        PropertyField {
            id: "fieldfieldfieldfieldfield1".to_owned(),
            group_id: "groupgroupgroupgroupgroup1".to_owned(),
            name: "f".to_owned(),
            type_: type_.into(),
            attrs: attrs.as_object().cloned(),
            object_type: PROPERTY_FIELD_OBJECT_TYPE_USER.to_owned(),
            target_type: "system".to_owned(),
            ..PropertyField::default()
        }
    }

    fn human() -> PropertyCaller {
        PropertyCaller {
            id: "useruseruseruseruseruser01".to_owned(),
            acting_as_scope: String::new(),
        }
    }

    fn attrs(field: &PropertyField) -> &StringInterface {
        field.attrs.as_ref().expect("attrs")
    }

    /// `sessionCallerID`: an unrestricted session has no user id and is the local admin.
    #[test]
    fn the_local_admin_is_the_unrestricted_session() {
        let session = Session {
            local: true,
            ..Session::default()
        };
        assert_eq!(
            PropertyCaller::from_session(&session).id,
            CALLER_ID_LOCAL_ADMIN
        );
        let session = Session {
            user_id: "u".to_owned(),
            ..Session::default()
        };
        assert_eq!(PropertyCaller::from_session(&session).id, "u");
    }

    /// The trim, the visibility default, and the type-based clearing — options off a text
    /// field, `ldap`/`saml` off a select field.
    #[test]
    fn attrs_are_trimmed_defaulted_and_cleared_by_type() {
        let mut text = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"visibility":"  always ","ldap":" dept ","options":[{"name":"x"}],"value_type":"email"}),
        );
        sanitize_and_validate_field_attrs(&mut text, "").unwrap();
        assert_eq!(attrs(&text)["visibility"], "always");
        assert_eq!(
            attrs(&text)["ldap"],
            "dept",
            "trimmed, and kept on a text field"
        );
        assert!(
            attrs(&text).get("options").is_none(),
            "options cleared off a text field"
        );

        let mut select = field(
            PropertyFieldType::SELECT,
            serde_json::json!({"ldap":"dept","saml":"x","options":[{"name":"A","color":"#fff"}]}),
        );
        sanitize_and_validate_field_attrs(&mut select, "").unwrap();
        assert_eq!(attrs(&select)["visibility"], "when_set", "defaulted");
        assert!(attrs(&select).get("ldap").is_none() && attrs(&select).get("saml").is_none());
        let option = &attrs(&select)["options"][0];
        assert_eq!(
            option["id"].as_str().map(str::len),
            Some(26),
            "an id was minted"
        );
        assert_eq!(option["name"], "A");
        assert_eq!(option["color"], "#fff");
        assert!(option.get("rank").is_none(), "rank is omitted when absent");
    }

    #[test]
    fn each_invalid_attr_is_its_own_refusal_in_order() {
        let cases = [
            (
                PropertyFieldType::TEXT,
                serde_json::json!({"visibility":"sometimes"}),
                "invalid visibility",
            ),
            (
                PropertyFieldType::TEXT,
                serde_json::json!({"value_type":"iban"}),
                "invalid value_type",
            ),
            (
                PropertyFieldType::SELECT,
                serde_json::json!({"value_type":"iban","options":[{"name":"a"}]}),
                "",
            ),
            (
                PropertyFieldType::TEXT,
                serde_json::json!({"managed":"team"}),
                "invalid managed",
            ),
            (
                PropertyFieldType::TEXT,
                serde_json::json!({"display_name":"x".repeat(256)}),
                "display_name exceeds",
            ),
            (
                PropertyFieldType::SELECT,
                serde_json::json!({"options":[]}),
                "invalid options",
            ),
            (
                PropertyFieldType::SELECT,
                serde_json::json!({"options":[{"name":"A"},{"name":"A"}]}),
                "invalid options",
            ),
            (
                PropertyFieldType::SELECT,
                serde_json::json!({"options":"nope"}),
                "invalid options",
            ),
            (
                PropertyFieldType::TEXT,
                serde_json::json!({"sort_order":"first"}),
                "sort_order must be numeric",
            ),
            // Visibility is validated before value_type: a body wrong on both reports visibility.
            (
                PropertyFieldType::TEXT,
                serde_json::json!({"visibility":"x","value_type":"iban"}),
                "invalid visibility",
            ),
        ];
        for (type_, attrs, expected) in cases {
            let mut f = field(type_, attrs.clone());
            let err = sanitize_and_validate_field_attrs(&mut f, "");
            if expected.is_empty() {
                assert!(err.is_ok(), "{attrs}: value_type is only validated on text");
                continue;
            }
            match err {
                Err(PropertyServiceError::InvalidFieldAttrs(msg)) => {
                    assert!(msg.contains(expected), "{attrs}: {msg}")
                }
                other => panic!("{attrs}: expected InvalidFieldAttrs, got {other:?}"),
            }
        }
    }

    /// Ranks: authored directly they must be positive and unique; on a conversion from another
    /// type they are renumbered `1..N` with rank-less options last; select ↔ rank keeps values.
    #[test]
    fn rank_options_are_validated_or_repaired_depending_on_the_previous_type() {
        let mut authored = field(
            PropertyFieldType::RANK,
            serde_json::json!({"options":[{"name":"A","rank":2},{"name":"B"}]}),
        );
        assert!(matches!(
            sanitize_and_validate_field_attrs(&mut authored, PropertyFieldType::RANK),
            Err(PropertyServiceError::InvalidFieldAttrs(msg)) if msg.contains("missing rank")
        ));
        let mut dup = field(
            PropertyFieldType::RANK,
            serde_json::json!({"options":[{"name":"A","rank":1},{"name":"B","rank":1}]}),
        );
        assert!(matches!(
            sanitize_and_validate_field_attrs(&mut dup, ""),
            Err(PropertyServiceError::InvalidFieldAttrs(msg)) if msg.contains("duplicate option rank")
        ));
        let mut zero = field(
            PropertyFieldType::RANK,
            serde_json::json!({"options":[{"name":"A","rank":0}]}),
        );
        assert!(matches!(
            sanitize_and_validate_field_attrs(&mut zero, ""),
            Err(PropertyServiceError::InvalidFieldAttrs(msg)) if msg.contains("positive integer")
        ));

        let mut converted = field(
            PropertyFieldType::RANK,
            serde_json::json!({"options":[{"name":"A","rank":7},{"name":"B"},{"name":"C","rank":7},{"name":"D","rank":2}]}),
        );
        sanitize_and_validate_field_attrs(&mut converted, PropertyFieldType::SELECT).unwrap();
        let ranks: Vec<(String, i64)> = attrs(&converted)["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                (
                    o["name"].as_str().unwrap().to_owned(),
                    o["rank"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            ranks,
            vec![
                ("A".to_owned(), 2),
                ("B".to_owned(), 4),
                ("C".to_owned(), 3),
                ("D".to_owned(), 1)
            ],
            "stable by rank, ties by position, rank-less last"
        );

        assert!(is_select_rank_transition("select", "rank"));
        assert!(is_select_rank_transition("rank", "select"));
        assert!(!is_select_rank_transition("select", "text"));
        assert!(!is_select_rank_transition("multiselect", "rank"));
    }

    #[test]
    fn owners_are_normalised_merged_and_bounded() {
        let mut f = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"owners":[
                {"id":" com.x ","type":"plugin","scopes":[" a ","b","a",""]},
                {"id":"com.x","type":"plugin","scopes":["c"]},
                {"id":"ldap","type":"service"}
            ]}),
        );
        sanitize_and_validate_field_attrs(&mut f, "").unwrap();
        assert_eq!(
            attrs(&f)["owners"],
            serde_json::json!([
                {"id":"com.x","type":"plugin","scopes":["a","b","c"]},
                {"id":"ldap","type":"service","scopes":[]}
            ])
        );

        let mut empty = field(PropertyFieldType::TEXT, serde_json::json!({"owners":[]}));
        sanitize_and_validate_field_attrs(&mut empty, "").unwrap();
        assert!(
            attrs(&empty).get("owners").is_none(),
            "an empty list is removed"
        );

        for (attrs, expected) in [
            (
                serde_json::json!({"owners":[{"id":"","type":"plugin"}]}),
                "owner id cannot be empty",
            ),
            (
                serde_json::json!({"owners":[{"id":"x","type":"robot"}]}),
                "unknown owner type",
            ),
            (
                serde_json::json!({"owners":[{"id":"x","type":"plugin","scopes":["a b"]}]}),
                "invalid characters",
            ),
            (
                serde_json::json!({"owners":[{"id":"x","type":"plugin"}],"managed":"admin"}),
                "managed=admin",
            ),
            (serde_json::json!({"owners":"nope"}), "invalid owners"),
        ] {
            let mut f = field(PropertyFieldType::TEXT, attrs.clone());
            match sanitize_and_validate_field_attrs(&mut f, "") {
                Err(PropertyServiceError::InvalidFieldAttrs(msg)) => {
                    assert!(msg.contains(expected), "{attrs}: {msg}")
                }
                other => panic!("{attrs}: {other:?}"),
            }
        }
        let many: Vec<serde_json::Value> = (0..21)
            .map(|i| serde_json::json!({"id":format!("p{i}"),"type":"plugin"}))
            .collect();
        let mut f = field(PropertyFieldType::TEXT, serde_json::json!({"owners":many}));
        assert!(matches!(
            sanitize_and_validate_field_attrs(&mut f, ""),
            Err(PropertyServiceError::InvalidFieldAttrs(msg)) if msg.contains("too many owners")
        ));
    }

    fn value(v: serde_json::Value) -> PropertyValue {
        PropertyValue {
            value: v,
            ..PropertyValue::default()
        }
    }

    #[test]
    fn values_are_validated_against_the_field_type() {
        let text = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"value_type":"email"}),
        );
        assert!(validate_value_against_field(&text, &value(serde_json::json!("a@b.io"))).is_ok());
        assert!(
            validate_value_against_field(&text, &value(serde_json::json!("  "))).is_ok(),
            "blank passes the value_type check"
        );
        assert!(
            validate_value_against_field(&text, &value(serde_json::json!("nope")))
                .unwrap_err()
                .contains("invalid email")
        );
        assert!(
            validate_value_against_field(&text, &value(serde_json::json!(5)))
                .unwrap_err()
                .contains("expected string value")
        );
        assert!(
            validate_value_against_field(&text, &value(serde_json::json!("x".repeat(65))))
                .unwrap_err()
                .contains("exceeds maximum length")
        );
        let plain_text = field(PropertyFieldType::TEXT, serde_json::json!({}));
        assert!(
            validate_value_against_field(
                &plain_text,
                &value(serde_json::json!(format!(" {} ", "x".repeat(64))))
            )
            .is_ok(),
            "the length is of the trimmed value"
        );

        let select = field(
            PropertyFieldType::SELECT,
            serde_json::json!({"options":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"A"}]}),
        );
        assert!(
            validate_value_against_field(
                &select,
                &value(serde_json::json!("aaaaaaaaaaaaaaaaaaaaaaaaaa"))
            )
            .is_ok()
        );
        assert!(
            validate_value_against_field(&select, &value(serde_json::json!(""))).is_ok(),
            "an empty selection is allowed"
        );
        assert!(
            validate_value_against_field(
                &select,
                &value(serde_json::json!("bbbbbbbbbbbbbbbbbbbbbbbbbb"))
            )
            .unwrap_err()
            .contains("does not exist")
        );
        assert!(
            validate_value_against_field(&select, &value(serde_json::json!(["a"])))
                .unwrap_err()
                .contains("expected string value for select")
        );

        let multi = field(
            PropertyFieldType::MULTISELECT,
            serde_json::json!({"options":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"A"}]}),
        );
        assert!(
            validate_value_against_field(
                &multi,
                &value(serde_json::json!(["aaaaaaaaaaaaaaaaaaaaaaaaaa"]))
            )
            .is_ok()
        );
        assert!(validate_value_against_field(&multi, &value(serde_json::json!([]))).is_ok());
        assert!(
            validate_value_against_field(&multi, &value(serde_json::json!(["zz"])))
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(
            validate_value_against_field(&multi, &value(serde_json::json!("a")))
                .unwrap_err()
                .contains("expected string array")
        );

        let user = field(PropertyFieldType::USER, serde_json::json!({}));
        assert!(
            validate_value_against_field(
                &user,
                &value(serde_json::json!("useruseruseruseruseruser01"))
            )
            .is_ok()
        );
        assert!(validate_value_against_field(&user, &value(serde_json::json!(""))).is_ok());
        assert_eq!(
            validate_value_against_field(&user, &value(serde_json::json!("bob"))).unwrap_err(),
            "invalid user id"
        );
        let multiuser = field(PropertyFieldType::MULTIUSER, serde_json::json!({}));
        assert_eq!(
            validate_value_against_field(&multiuser, &value(serde_json::json!(["bob"])))
                .unwrap_err(),
            "invalid user id: bob"
        );

        let date = field(PropertyFieldType::DATE, serde_json::json!({}));
        assert!(
            validate_value_against_field(&date, &value(serde_json::json!(12345))).is_ok(),
            "a date is not checked"
        );
    }

    /// The value-write gate: owners refuse every human; a protected field refuses a caller who is
    /// not its source plugin; a synced field refuses everyone but its sync service; a public
    /// field refuses nobody.
    #[test]
    fn value_writes_are_gated_by_owners_then_protection_then_sync() {
        let caller = human();
        let owned = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"owners":[{"id":"com.x","type":"plugin"}]}),
        );
        assert!(matches!(
            check_value_write_access(&owned, &caller),
            Err(PropertyServiceError::AccessDenied(_))
        ));

        let protected = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"protected":true,"source_plugin_id":"com.x"}),
        );
        assert!(matches!(
            check_value_write_access(&protected, &caller),
            Err(PropertyServiceError::AccessDenied(_))
        ));
        let source = PropertyCaller {
            id: "com.x".to_owned(),
            acting_as_scope: String::new(),
        };
        assert!(
            check_value_write_access(&protected, &source).is_ok(),
            "the source plugin itself may write"
        );

        let synced = field(PropertyFieldType::TEXT, serde_json::json!({"ldap":"dept"}));
        assert!(matches!(
            check_value_write_access(&synced, &caller),
            Err(PropertyServiceError::SyncLocked(_))
        ));
        let ldap = PropertyCaller {
            id: CALLER_ID_LDAP_SYNC.to_owned(),
            acting_as_scope: String::new(),
        };
        assert!(check_value_write_access(&synced, &ldap).is_ok());
        let saml = PropertyCaller {
            id: CALLER_ID_SAML_SYNC.to_owned(),
            acting_as_scope: String::new(),
        };
        assert!(
            matches!(
                check_value_write_access(&synced, &saml),
                Err(PropertyServiceError::SyncLocked(_))
            ),
            "ldap is not saml"
        );

        // Owners with an implicit service owner: the ldap sync may write, scoped or not.
        let owned_synced = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"owners":[{"id":"com.x","type":"plugin","scopes":["s"]}],"ldap":"dept"}),
        );
        assert!(check_value_write_access(&owned_synced, &ldap).is_ok());

        // The string "true" is not the boolean true: not protected.
        let stringly = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"protected":"true","source_plugin_id":"com.x"}),
        );
        assert!(check_value_write_access(&stringly, &caller).is_ok());
        let public = field(PropertyFieldType::TEXT, serde_json::json!({}));
        assert!(check_value_write_access(&public, &caller).is_ok());
    }

    /// The delete gate: owners let a human through; a protected field with an uninstalled source
    /// plugin is deletable (no plugin host here); a protected field with no source plugin is too.
    #[test]
    fn field_deletes_are_allowed_where_the_source_plugin_is_absent() {
        let caller = human();
        assert!(
            check_field_delete_access(
                &field(
                    PropertyFieldType::TEXT,
                    serde_json::json!({"owners":[{"id":"p","type":"plugin"}]})
                ),
                &caller
            )
            .is_ok()
        );
        assert!(
            check_field_delete_access(
                &field(
                    PropertyFieldType::TEXT,
                    serde_json::json!({"protected":true,"source_plugin_id":"com.x"})
                ),
                &caller
            )
            .is_ok()
        );
        assert!(
            check_field_delete_access(
                &field(
                    PropertyFieldType::TEXT,
                    serde_json::json!({"protected":true})
                ),
                &caller
            )
            .is_ok()
        );
        // But the definition **edit** of the same protected field is refused.
        let protected = field(
            PropertyFieldType::TEXT,
            serde_json::json!({"protected":true,"source_plugin_id":"com.x"}),
        );
        assert!(matches!(
            enforce_field_update_access(&protected, &protected, &caller),
            Err(PropertyServiceError::AccessDenied(_))
        ));
        assert!(
            matches!(
                enforce_field_update_access(
                    &protected,
                    &field(
                        PropertyFieldType::TEXT,
                        serde_json::json!({"protected":true,"source_plugin_id":"com.y"})
                    ),
                    &PropertyCaller {
                        id: "com.x".to_owned(),
                        acting_as_scope: String::new()
                    }
                ),
                Ok(())
            ),
            "the source plugin passes the update gate; the immutability check is separate"
        );
        assert!(matches!(
            ensure_source_plugin_id_unchanged(
                &protected,
                &field(
                    PropertyFieldType::TEXT,
                    serde_json::json!({"source_plugin_id":"com.y"})
                )
            ),
            Err(PropertyServiceError::AccessDenied(_))
        ));
    }

    #[test]
    fn option_ids_ranks_and_clamping() {
        let select: PropertyFieldType = PropertyFieldType::SELECT.into();
        assert_eq!(
            extract_option_ids_from_value(&select, &serde_json::json!("a"))
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            extract_option_ids_from_value(&select, &serde_json::json!(""))
                .unwrap()
                .unwrap()
                .len(),
            0
        );
        assert!(
            extract_option_ids_from_value(&select, &serde_json::Value::Null)
                .unwrap()
                .is_none()
        );
        let multi: PropertyFieldType = PropertyFieldType::MULTISELECT.into();
        assert_eq!(
            extract_option_ids_from_value(&multi, &serde_json::json!(["a", "", "b"]))
                .unwrap()
                .unwrap()
                .len(),
            2
        );
        let text: PropertyFieldType = PropertyFieldType::TEXT.into();
        assert!(extract_option_ids_from_value(&text, &serde_json::json!("a")).is_err());

        let rank = field(
            PropertyFieldType::RANK,
            serde_json::json!({"options":[
                {"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"Low","rank":1},
                {"id":"bbbbbbbbbbbbbbbbbbbbbbbbbb","name":"High","rank":3},
                {"id":"cccccccccccccccccccccccccc","name":"None"}
            ]}),
        );
        let ranks = build_option_rank_map(&rank);
        assert_eq!(ranks.len(), 2, "an option without a rank is skipped");
        let clamped = clamp_rank_value_to_rank(
            value(serde_json::json!("bbbbbbbbbbbbbbbbbbbbbbbbbb")),
            &ranks,
            1,
        )
        .unwrap();
        assert_eq!(clamped.value, "aaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(
            clamp_rank_value_to_rank(value(serde_json::json!("x")), &ranks, 2).is_none(),
            "no option at that rank"
        );
    }
}
