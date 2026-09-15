//! Port of the app layer behind `api4/custom_profile_attributes.go` — `App.GetPropertyGroup`
//! (app/property_group.go:25), `App.SearchPropertyFields`, `App.GetPropertyField(s)`,
//! `App.CreatePropertyField`, `App.UpdatePropertyField`, `App.DeletePropertyField`
//! (app/property_field.go), `App.SearchPropertyValues` and `App.UpsertPropertyValues`
//! (app/property_value.go) — as the seven CPA routes reach them, on **any** licence.
//!
//! # The licence is one hook, and the hook is not uniform
//!
//! Go registers a `LicenseCheckHook` over the `access_control` property group at startup
//! (app/server.go:322) and it is the *first* hook, so it runs before access control and attribute
//! validation on every field and value operation in that group. Without an Enterprise licence it
//! returns `ErrLicenseRequired`, which `mapPropertyServiceError` turns into **403
//! `app.property.license_error`** (app/property_errors.go:44). It does **not** fire uniformly:
//!
//! | hook arm | when it refuses |
//! |---|---|
//! | `PreCreatePropertyField` | always — a create is a 403 before it touches the table |
//! | `PostGetPropertyField` | only once the row has been **found**; a miss is a 404 first |
//! | `PostGetPropertyFields` | `len(fields) == 0` returns **nil**, so an empty page is a 200 |
//! | `PostGetPropertyValues` | same empty short-circuit |
//!
//! So an unlicensed server answers `[]` to `listCPAFields` on a group with no user fields, `{}`
//! to `listCPAValues` for a user with no values, `404` to a patch of a field that does not exist,
//! and `403` the moment any of those reads finds a row. All four were verified against the Go
//! server on this stack, with and without a seeded `access_control` user field.
//!
//! Until 2026-09-13 only that unlicensed contract was served and a licensed installation was
//! forwarded ([D-300]). The licence is readable now (`App::license`), the rest of the chain is
//! [`crate::property_hooks`], and every function here runs the chain in Go's order: licence,
//! access control, attribute validation, field limit, then the service's own invariants, then the
//! store, then the post-hooks. Measured against the licensed Go oracle.
//!
//! # Two handlers order the group read and the target check differently
//!
//! `listCPAValues` runs `hasTargetAccess` **before** `GetPropertyGroup`
//! (custom_profile_attributes.go:378); `cpaPatchValues`, which both PATCH routes share, runs the
//! group read **first** (:311). Nothing here can enforce that — the target check is
//! session-bound and lives in the API layer — but the asymmetry is real and observable when the
//! group is missing, so it is written down where a reader porting the next property route will
//! see it.

use std::collections::{BTreeMap, HashMap, HashSet};

use mm_model::custom_profile_attributes::{CPAField, cpa_fields_from_property_fields};
use mm_model::property_field::{
    PROPERTY_FIELD_ATTRIBUTE_OPTIONS, PROPERTY_FIELD_OBJECT_TYPE_SYSTEM,
    PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE, PROPERTY_FIELD_OBJECT_TYPE_USER,
    PROPERTY_FIELD_TARGET_LEVEL_SYSTEM, PermissionLevel, PropertyField, PropertyFieldSearchOpts,
    PropertyFieldType,
};
use mm_model::property_group::{
    ACCESS_CONTROL_GROUP_FIELD_LIMIT, ACCESS_CONTROL_PROPERTY_GROUP_NAME, PropertyGroup,
};
use mm_model::property_value::{
    PROPERTY_VALUE_TARGET_TYPE_USER, PropertyValue, PropertyValueSearchOpts,
    sanitize_property_value,
};
use mm_model::utils::{AppError, AppResult, go_quote, is_valid_id};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_PROPERTY_FIELD_CREATED, WEBSOCKET_EVENT_PROPERTY_FIELD_UPDATED,
    WEBSOCKET_EVENT_PROPERTY_VALUES_UPDATED, WebSocketEvent,
};
use mm_store::{PropertyStore, StoreError};

use crate::App;
use crate::property_hooks::{
    PropertyCaller, PropertyServiceError, check_value_write_access, is_select_rank_transition,
};

/// Port of the page size every CPA read asks for (`AccessControlGroupFieldLimit + 5`).
///
/// The `+ 5` is Go's, and the comment on [`ACCESS_CONTROL_GROUP_FIELD_LIMIT`] says why the whole
/// result set is read in one page rather than paginated: the limit is assumed to bound it.
const CPA_READ_PER_PAGE: i64 = ACCESS_CONTROL_GROUP_FIELD_LIMIT + 5;

/// What `App.UpdatePropertyField` hands back: the field as written and the ids of every field
/// whose values a post-hook cleared — the type-change cleanup, for a type change that is not
/// select ↔ rank.
#[derive(Debug, Clone)]
pub struct CpaFieldUpdate {
    pub field: PropertyField,
    pub cleared_field_ids: Vec<String>,
}

impl App {
    /// Port of `App.GetPropertyGroup` (app/property_group.go:25) for the CPA group.
    ///
    /// One error id across both arms — `app.property_group.get.app_error` — and only the status
    /// differs: **404** for a missing group, 500 for anything else. The store collapses a driver
    /// failure into not-found (see [`mm_store::SqlPropertyStore::get_group`]), so the 500 arm is
    /// currently unreachable; it is written out because the id is shared and a reader would
    /// otherwise assume the 404 is the only answer.
    ///
    /// Go reads this through `PropertyService.GetPropertyGroup`, which goes **straight to the
    /// store** — it is `Group()`, used at startup, that consults the in-memory cache. So this is
    /// a query on every request on both sides, not a cache read.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn cpa_property_group(&self) -> AppResult<PropertyGroup> {
        let group = self
            .store()
            .property()
            .get_group(ACCESS_CONTROL_PROPERTY_GROUP_NAME)
            .await
            .map_err(|err| {
                let not_found = err.is_not_found();
                if !not_found {
                    tracing::error!(error = ?err, "the CPA property group lookup failed");
                }
                AppError::boxed(
                    "GetPropertyGroup",
                    "app.property_group.get.app_error",
                    None,
                    String::new(),
                    if not_found { 404 } else { 500 },
                )
            })?;

        tracing::Span::current().record("found", true);
        Ok(group)
    }

    /// `LicenseCheckHook.requireLicense` for the managed group: `MinimumEnterpriseLicense`
    /// (license_check.go:39). A Professional licence is **not** enough.
    async fn cpa_licensed(&self) -> AppResult<bool> {
        let license = self.license().await?;
        Ok(mm_model::license::minimum_enterprise_license(
            license.as_deref(),
        ))
    }

    /// The post-get chain on a list of fields — `PostGetPropertyFields` in the licence hook
    /// (license_check.go:100) and the access-control hook (access_control.go:312): the empty
    /// short-circuit, the licence, then read access control. A group the hooks do not manage
    /// passes straight through.
    pub async fn managed_post_get_fields(
        &self,
        group: &PropertyGroup,
        fields: Vec<PropertyField>,
        caller: &PropertyCaller,
        where_: &'static str,
    ) -> AppResult<Vec<PropertyField>> {
        if fields.is_empty() || !is_managed_group(group) {
            return Ok(fields);
        }
        if !self.cpa_licensed().await? {
            return Err(property_licence_refusal(where_));
        }
        Ok(self
            .apply_field_read_access_control_to_list(fields, caller)
            .await)
    }

    /// `PostGetPropertyField` in both hooks, for a single found row: the licence, then read
    /// access control. There is no empty short-circuit — the row exists.
    pub async fn managed_post_get_field(
        &self,
        group: &PropertyGroup,
        field: PropertyField,
        caller: &PropertyCaller,
        where_: &'static str,
    ) -> AppResult<PropertyField> {
        if !is_managed_group(group) {
            return Ok(field);
        }
        if !self.cpa_licensed().await? {
            return Err(property_licence_refusal(where_));
        }
        Ok(self.apply_field_read_access_control(field, caller).await)
    }

    /// `PostGetPropertyValues` in both hooks: the empty short-circuit, the licence, then read
    /// access control, which **drops** values the caller may not see rather than refusing.
    pub async fn managed_post_get_values(
        &self,
        group: &PropertyGroup,
        values: Vec<PropertyValue>,
        caller: &PropertyCaller,
        where_: &'static str,
    ) -> AppResult<Vec<PropertyValue>> {
        if values.is_empty() || !is_managed_group(group) {
            return Ok(values);
        }
        if !self.cpa_licensed().await? {
            return Err(property_licence_refusal(where_));
        }
        self.apply_value_read_access_control(values, caller)
            .await
            .map_err(|err| err.into_app_error(where_, "app.property_value.search.app_error"))
    }

    /// Port of `App.SearchPropertyFields` as `listCPAFields` calls it (custom_profile_attributes
    /// .go:40): every live `user` field in the group, through the post-get hooks, converted and
    /// sorted by `sort_order` then id.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, fields))]
    pub async fn cpa_list_fields(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
    ) -> AppResult<Vec<CPAField>> {
        let fields = self.cpa_search_user_fields(&group.id).await?;
        tracing::Span::current().record("fields", fields.len());
        let fields = self
            .managed_post_get_fields(group, fields, caller, "SearchPropertyFields")
            .await?;
        cpa_fields_from_property_fields(&fields)
            .map_err(|err| conversion_error("listCPAFields", err))
    }

    /// Port of `App.GetPropertyField` (app/property_field.go:137) through the post-get hooks:
    /// a miss is 404 `app.property.not_found.app_error` **before** the licence is consulted, a
    /// hit is the licence refusal or the field as the caller may see it.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, field_id = %field_id))]
    pub async fn cpa_get_field(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        field_id: &str,
    ) -> AppResult<PropertyField> {
        let field = self
            .store()
            .property()
            .get_field(&group.id, field_id)
            .await
            .map_err(|err| property_read_error("GetPropertyField", err))?;
        self.managed_post_get_field(group, field, caller, "GetPropertyField")
            .await
    }

    /// Port of `App.GetPropertyFields` (app/property_field.go:149) through the post-get hooks.
    ///
    /// The store returns what it finds; the service compares `len(fields) < len(ids)` and raises
    /// `ErrFieldNotFound`, which the app layer answers as **404
    /// `app.property_field.not_found.app_error`** — one word away from the single-field miss's
    /// id. Only once every id resolves does the licence hook see a non-empty slice.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, wanted = ids.len()))]
    pub async fn cpa_get_fields(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        ids: &[String],
    ) -> AppResult<Vec<PropertyField>> {
        let fields = self
            .get_property_fields_raw(&group.id, ids)
            .await
            .map_err(|err| {
                err.into_app_error("GetPropertyFields", "app.property_field.get_many.app_error")
            })?;
        self.managed_post_get_fields(group, fields, caller, "GetPropertyFields")
            .await
    }

    /// Port of `App.CreatePropertyField` (app/property_field.go:100) with the hook chain and the
    /// service's own `createPropertyField` (app/properties/property_field.go:118).
    ///
    /// In order: the intrinsic invariants (canonicalise a system-object field, trim the name, the
    /// rank feature gate, the protected refusal); the four pre-create hooks — licence, access
    /// control, attribute validation, field limit; the group/field version match; the linked
    /// source, if any; the name-conflict check; the insert; and the `property_field_created`
    /// broadcast. The handler adds the CPA-specific event and the audit record after this
    /// returns.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, name = %field.name, field_id))]
    pub async fn cpa_create_field(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        mut field: PropertyField,
        connection_id: &str,
    ) -> AppResult<PropertyField> {
        const WHERE: &str = "CreatePropertyField";

        canonicalize_system_object_field(&mut field);
        field.name = field.name.trim().to_owned();

        self.rank_property_field_gate(WHERE, &field)?;
        if field.protected {
            return Err(AppError::boxed(
                WHERE,
                "app.property_field.create.protected.app_error",
                None,
                "cannot create protected field",
                400,
            ));
        }

        let created = self
            .cpa_create_field_service(group, caller, field)
            .await
            .map_err(|err| err.into_app_error(WHERE, "app.property_field.create.app_error"))?;
        tracing::Span::current().record("field_id", &created.id);

        self.publish_property_field_event(
            WEBSOCKET_EVENT_PROPERTY_FIELD_CREATED,
            &created,
            connection_id,
        )
        .await;
        Ok(created)
    }

    /// `PropertyService.CreatePropertyField` (property_field.go:544): the pre-create hooks, then
    /// `createPropertyField`.
    async fn cpa_create_field_service(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        mut field: PropertyField,
    ) -> Result<PropertyField, PropertyServiceError> {
        if is_managed_group(group) {
            // Hook 1: the licence — unconditional on a create.
            if !self.cpa_licensed().await? {
                return Err(PropertyServiceError::LicenseRequired);
            }
            // Hook 2: access control.
            self.access_control_pre_create_field(&mut field, caller)
                .await?;
            // Hook 3: attribute validation, which also pins the three permission levels.
            self.attribute_validation_pre_create_field(&mut field, caller)
                .await?;
            // Hook 5: the field limit (hook 4, the value audit, has no field arms).
            self.field_limit_pre_create_field(&field).await?;
        }

        // createPropertyField (property_field.go:118).
        enforce_field_group_version_match("CreatePropertyField", group, &field)?;
        if field.is_psav1() {
            return Ok(self.store().property().create_field(field).await?);
        }

        if let Some(linked) = field.linked_field_id.clone().filter(|id| !id.is_empty()) {
            self.inherit_linked_source(&mut field, &linked).await?;
        }

        let conflict = self
            .store()
            .property()
            .check_property_name_conflict(&field, "")
            .await?;
        if !conflict.is_empty() {
            return Err(name_conflict(
                "CreatePropertyField",
                "create",
                &field,
                &conflict,
            ));
        }

        Ok(self.store().property().create_field(field).await?)
    }

    /// The linked-source block of `createPropertyField` (property_field.go:130): seven refusals
    /// in order, then the source's type, options and permission levels are copied onto the new
    /// field. Unreachable from `createCPAField`'s own fields (a CPA field can only be linked to a
    /// **template**, which the CPA API cannot create) but reachable from its body, which carries
    /// `linked_field_id` through `ToPropertyField` untouched.
    async fn inherit_linked_source(
        &self,
        field: &mut PropertyField,
        linked: &str,
    ) -> Result<(), PropertyServiceError> {
        let refuse = |id: &str, detail: String| {
            PropertyServiceError::App(AppError::boxed(
                "CreatePropertyField",
                id,
                None,
                detail,
                400,
            ))
        };
        if field.object_type == PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE {
            return Err(refuse(
                "app.property_field.create.template_cannot_be_linked.app_error",
                "template fields cannot have a linked_field_id".to_owned(),
            ));
        }
        let source = match self.store().property().get_field_in_any_group(linked).await {
            Ok(source) => source,
            Err(err) if err.is_not_found() => {
                return Err(refuse(
                    "app.property_field.create.linked_source_not_found.app_error",
                    format!("linked source field {} not found", go_quote(linked)),
                ));
            }
            Err(err) => return Err(PropertyServiceError::Store(err)),
        };
        if source.group_id != field.group_id {
            return Err(refuse(
                "app.property_field.create.linked_source_cross_group.app_error",
                format!(
                    "cannot link to field {} in group {}: source must be in the same group {}",
                    go_quote(linked),
                    go_quote(&source.group_id),
                    go_quote(&field.group_id)
                ),
            ));
        }
        if source.delete_at != 0 {
            return Err(refuse(
                "app.property_field.create.linked_source_deleted.app_error",
                format!("linked source field {} is deleted", go_quote(linked)),
            ));
        }
        if source.object_type != PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE {
            return Err(refuse(
                "app.property_field.create.linked_source_not_template.app_error",
                "can only link to template fields".to_owned(),
            ));
        }
        if field.target_type != source.target_type {
            return Err(refuse(
                "app.property_field.create.linked_target_type_mismatch.app_error",
                format!(
                    "linked field target_type {} must match source template target_type {}",
                    go_quote(&field.target_type),
                    go_quote(&source.target_type)
                ),
            ));
        }
        if source
            .linked_field_id
            .as_deref()
            .is_some_and(|id| !id.is_empty())
        {
            return Err(refuse(
                "app.property_field.create.linked_source_is_linked.app_error",
                "cannot link to a field that is itself linked (no chains allowed)".to_owned(),
            ));
        }

        field.type_ = source.type_.clone();
        let attrs = field
            .attrs
            .get_or_insert_with(mm_model::utils::StringInterface::new);
        if let Some(options) = source
            .attrs
            .as_ref()
            .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS))
        {
            attrs.insert(PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_owned(), options.clone());
        }
        if source.permission_field.is_some() {
            field.permission_field = source.permission_field.clone();
        }
        if source.permission_values.is_some() {
            field.permission_values = source.permission_values.clone();
        }
        if source.permission_options.is_some() {
            field.permission_options = source.permission_options.clone();
        }
        Ok(())
    }

    /// Port of `rankPropertyFieldGate` (app/property_field.go:80): a `rank` **user** field
    /// needs the `PropertyFieldRank` flag, which is on by default.
    fn rank_property_field_gate(&self, where_: &'static str, field: &PropertyField) -> AppResult {
        if field.type_.as_str() != PropertyFieldType::RANK
            || field.object_type != PROPERTY_FIELD_OBJECT_TYPE_USER
            || self.config().feature_flag_property_field_rank
        {
            return Ok(());
        }
        Err(AppError::boxed(
            where_,
            "app.property_field.rank_disabled.app_error",
            None,
            "rank property fields are not enabled",
            400,
        ))
    }

    /// Port of `App.UpdatePropertyField` → `App.UpdatePropertyFields` (app/property_field.go:270)
    /// for one field, with the hook chain and the service's `updatePropertyFields`
    /// (app/properties/property_field.go:317).
    ///
    /// `field` is the caller's copy — the existing row as the post-get hooks showed it, patched
    /// in place, with `updated_by` stamped. The existing row is read **twice** here as it is in
    /// Go: once through the hooks for the app layer's own invariants, once raw for the service's
    /// optimistic-concurrency expectation, which must be the row as stored.
    ///
    /// # Two copies of the linked-field invariants, with different error shapes
    ///
    /// The app layer refuses a type or options change on a linked field, a link-target change
    /// and a late link with the `FieldID` param; the service refuses the same four without it,
    /// a few calls later. The app layer's copy runs first and is what a client sees. Both are
    /// here because a caller that bypasses the app layer — Go's plugin API — meets the second.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, field_id = %field.id, cleared))]
    pub async fn cpa_update_field(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        mut field: PropertyField,
        connection_id: &str,
    ) -> AppResult<CpaFieldUpdate> {
        const WHERE: &str = "UpdatePropertyFields";
        field.name = field.name.trim().to_owned();

        let ids = vec![field.id.clone()];
        let existing = self
            .get_property_fields_raw(&group.id, &ids)
            .await
            .map_err(|err| {
                err.into_app_error(WHERE, "app.property_field.update.get_existing.app_error")
            })?;
        let existing = self
            .managed_post_get_fields(group, existing, caller, WHERE)
            .await?
            .into_iter()
            .next();

        if let Some(existing) = existing.as_ref() {
            self.rank_property_field_gate(WHERE, &field)?;

            let existing_linked = existing
                .linked_field_id
                .as_deref()
                .is_some_and(|id| !id.is_empty());
            let incoming_linked = field
                .linked_field_id
                .as_deref()
                .is_some_and(|id| !id.is_empty());
            let refuse = |id: &str, detail: &str, status: i32| {
                let mut params = HashMap::new();
                params.insert(
                    "FieldID".to_owned(),
                    serde_json::Value::String(existing.id.clone()),
                );
                AppError::boxed(WHERE, id, Some(params), detail, status)
            };
            if existing_linked {
                if field.type_ != existing.type_ {
                    return Err(refuse(
                        "app.property_field.update.linked_type_change.app_error",
                        "cannot modify type of a linked field",
                        400,
                    ));
                }
                let existing_opts = existing
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS));
                let incoming_opts = field
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS));
                if existing_opts != incoming_opts {
                    return Err(refuse(
                        "app.property_field.update.linked_options_change.app_error",
                        "cannot modify options of a linked field",
                        400,
                    ));
                }
                if incoming_linked && field.linked_field_id != existing.linked_field_id {
                    return Err(refuse(
                        "app.property_field.update.cannot_change_link_target.app_error",
                        "cannot change link target",
                        400,
                    ));
                }
            } else if incoming_linked {
                return Err(refuse(
                    "app.property_field.update.cannot_link_existing.app_error",
                    "linked_field_id can only be set at creation time",
                    400,
                ));
            }

            if existing.protected {
                return Err(refuse(
                    "app.property_field.update.protected.app_error",
                    "cannot update protected field",
                    403,
                ));
            }
        }

        let (updated, propagated, cleared) = self
            .cpa_update_field_service(group, caller, field)
            .await
            .map_err(|err| err.into_app_error(WHERE, "app.property_field.update.app_error"))?;
        tracing::Span::current().record("cleared", cleared.len());

        self.publish_property_field_event(
            WEBSOCKET_EVENT_PROPERTY_FIELD_UPDATED,
            &updated,
            connection_id,
        )
        .await;
        for field in &propagated {
            self.publish_property_field_event(WEBSOCKET_EVENT_PROPERTY_FIELD_UPDATED, field, "")
                .await;
        }
        for field_id in &cleared {
            let mut message = WebSocketEvent::new(
                WEBSOCKET_EVENT_PROPERTY_VALUES_UPDATED,
                "",
                "",
                "",
                None,
                "",
            );
            message.add("field_id", serde_json::Value::String(field_id.clone()));
            message.add("values", serde_json::Value::String("[]".to_owned()));
            self.publish(message).await;
        }

        Ok(CpaFieldUpdate {
            field: updated,
            cleared_field_ids: cleared,
        })
    }

    /// `PropertyService.UpdatePropertyFields` (property_field.go:658) for one field: the
    /// pre-update hooks against the raw row, then `updatePropertyFields`, then the post-update
    /// hooks. Returns the field as written, the propagated dependents, and the cleared ids.
    async fn cpa_update_field_service(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        mut field: PropertyField,
    ) -> Result<(PropertyField, Vec<PropertyField>, Vec<String>), PropertyServiceError> {
        const WHERE: &str = "UpdatePropertyFields";
        let managed = is_managed_group(group);
        if managed && !self.cpa_licensed().await? {
            return Err(PropertyServiceError::LicenseRequired);
        }
        let ids = vec![field.id.clone()];
        let raw_existing = self
            .get_property_fields_raw(&group.id, &ids)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| PropertyServiceError::FieldNotFound(field.id.clone()))?;

        if managed {
            self.access_control_pre_update_field(&raw_existing, &field, caller)?;
            self.attribute_validation_pre_update_field(&raw_existing, &mut field, caller)
                .await?;
        }

        // updatePropertyFields (property_field.go:317).
        enforce_field_group_version_match(WHERE, group, &field)?;
        if !field.is_psav1() {
            let refuse = |id: &str, detail: &str, status: i32| {
                PropertyServiceError::App(AppError::boxed(WHERE, id, None, detail, status))
            };
            let existing_linked = raw_existing
                .linked_field_id
                .as_deref()
                .is_some_and(|id| !id.is_empty());
            if existing_linked && field.type_ != raw_existing.type_ {
                return Err(refuse(
                    "app.property_field.update.linked_type_change.app_error",
                    "cannot modify type of a linked field",
                    400,
                ));
            }
            if existing_linked && options_changed(raw_existing.attrs.as_ref(), field.attrs.as_ref())
            {
                return Err(refuse(
                    "app.property_field.update.linked_options_change.app_error",
                    "cannot modify options of a linked field",
                    400,
                ));
            }
            if field.linked_field_id.as_deref() == Some("") {
                field.linked_field_id = None;
            }
            let new_linked = field.linked_field_id.is_some();
            if !existing_linked && new_linked {
                return Err(refuse(
                    "app.property_field.update.cannot_link_existing.app_error",
                    "linked_field_id can only be set at creation time",
                    400,
                ));
            }
            if existing_linked
                && new_linked
                && field.linked_field_id != raw_existing.linked_field_id
            {
                return Err(refuse(
                    "app.property_field.update.cannot_change_link_target.app_error",
                    "cannot change link target; unlink first then create a new linked field",
                    400,
                ));
            }
            if field.type_ != raw_existing.type_ {
                let dependents = self
                    .store()
                    .property()
                    .count_linked_fields(&field.id)
                    .await?;
                if dependents > 0 {
                    return Err(refuse(
                        "app.property_field.update.type_change_with_dependents.app_error",
                        "cannot change type of a field with active linked dependents",
                        409,
                    ));
                }
            }
            if raw_existing.name != field.name
                || raw_existing.target_type != field.target_type
                || raw_existing.target_id != field.target_id
                || raw_existing.object_type != field.object_type
            {
                let conflict = self
                    .store()
                    .property()
                    .check_property_name_conflict(&field, &field.id)
                    .await?;
                if !conflict.is_empty() {
                    return Err(name_conflict(WHERE, "update", &field, &conflict));
                }
            }
        }

        let (updated, propagated) = self
            .store()
            .property()
            .update_field(&group.id, field, raw_existing.update_at)
            .await?;

        // Post-hook: the type-change value cleanup (type_change_value_cleanup.go:953). Best
        // effort — a failed cleanup is logged and the update stands.
        let mut cleared = Vec::new();
        if raw_existing.type_ != updated.type_
            && !is_select_rank_transition(raw_existing.type_.as_str(), updated.type_.as_str())
        {
            match self
                .store()
                .property()
                .delete_values_for_field(&group.id, &updated.id)
                .await
            {
                Ok(()) => cleared.push(updated.id.clone()),
                Err(err) => tracing::error!(
                    group_id = %group.id,
                    field_id = %updated.id,
                    from_type = %raw_existing.type_.as_str(),
                    to_type = %updated.type_.as_str(),
                    error = ?err,
                    "type-change value cleanup failed"
                ),
            }
        }
        Ok((updated, propagated, cleared))
    }

    /// Port of `App.DeletePropertyField` (app/property_field.go:425) with the hook chain, for
    /// any group — the hooks fire only on the managed one.
    ///
    /// The existing row is read through the post-get hooks first — so a licence refusal, not a
    /// 404, is what an unlicensed caller gets for a real field — then the protected refusal,
    /// then the pre-delete hooks (licence again, access control), then the service's three
    /// statements and the broadcast, which [`App::delete_property_field`] already holds.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, field_id = %field_id))]
    pub async fn delete_property_field_with_hooks(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        field_id: &str,
        connection_id: &str,
    ) -> AppResult<()> {
        const WHERE: &str = "DeletePropertyField";
        let existing = self.cpa_get_field(group, caller, field_id).await?;
        if existing.protected {
            return Err(AppError::boxed(
                WHERE,
                "app.property_field.delete.protected.app_error",
                None,
                "cannot delete protected field",
                403,
            ));
        }

        let raw = self
            .store()
            .property()
            .get_field(&group.id, field_id)
            .await
            .map_err(|err| property_read_error(WHERE, err))?;
        if is_managed_group(group) {
            if !self.cpa_licensed().await? {
                return Err(property_licence_refusal(WHERE));
            }
            self.access_control_pre_delete_field(&raw, caller)
                .map_err(|err| err.into_app_error(WHERE, "app.property_field.delete.app_error"))?;
        }

        self.delete_property_field(group, &raw, connection_id).await
    }

    /// Port of `App.UpsertPropertyValues` (app/property_value.go:643) for the `user` object
    /// type, with the hook chain and the service's `upsertPropertyValues`.
    ///
    /// The app layer's own checks come first — one group, valid and distinct field ids, then the
    /// fields through the post-get hooks so an unknown or wrongly-typed field is a **404** — then
    /// the pre-upsert hooks against the raw fields: licence, write access (owners, protected,
    /// sync lock), value validation. Then the template refusal, the upsert, the audit line and
    /// the `property_values_updated` broadcast. Values are sanitised (`SanitizePropertyValue`)
    /// before any of it.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, target_id = %target_id, values = values.len()))]
    pub async fn cpa_upsert_values(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        values: Vec<PropertyValue>,
        target_id: &str,
        connection_id: &str,
    ) -> AppResult<Vec<PropertyValue>> {
        self.upsert_property_values(
            group,
            caller,
            values,
            PROPERTY_FIELD_OBJECT_TYPE_USER,
            target_id,
            connection_id,
        )
        .await
    }

    /// Port of `App.UpsertPropertyValues` (app/property_value.go:169) for any object type —
    /// the generic `patchPropertyValues` routes call it with the URL's object type, the CPA
    /// route with `user`. `objectType` is never empty over REST, so the mismatch check and the
    /// broadcast always run; the broadcast's scope is `resolveValueBroadcastParams`'s
    /// ([`App::resolve_value_broadcast_params`]), and a failure there is logged and the event
    /// skipped, as Go does.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, object_type = %object_type, target_id = %target_id, values = values.len()))]
    pub async fn upsert_property_values(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        mut values: Vec<PropertyValue>,
        object_type: &str,
        target_id: &str,
        connection_id: &str,
    ) -> AppResult<Vec<PropertyValue>> {
        const WHERE: &str = "UpsertPropertyValues";
        if values.is_empty() {
            return Err(AppError::boxed(
                WHERE,
                "app.property_value.invalid_input.app_error",
                None,
                "property values are required",
                400,
            ));
        }

        let group_id = values[0].group_id.clone();
        let mut seen: HashSet<String> = HashSet::new();
        let mut field_ids = Vec::with_capacity(values.len());
        for value in values.iter_mut() {
            let mut params = HashMap::new();
            params.insert(
                "FieldID".to_owned(),
                serde_json::Value::String(value.field_id.clone()),
            );
            if value.group_id != group_id {
                return Err(AppError::boxed(
                    WHERE,
                    "app.property_value.upsert.mixed_groups.app_error",
                    None,
                    "all values in a batch must belong to the same group",
                    400,
                ));
            }
            if !is_valid_id(&value.field_id) {
                return Err(AppError::boxed(
                    WHERE,
                    "app.property_value.upsert.invalid_field_id.app_error",
                    Some(params),
                    "invalid field ID",
                    400,
                ));
            }
            if !seen.insert(value.field_id.clone()) {
                return Err(AppError::boxed(
                    WHERE,
                    "app.property_value.upsert.duplicate_field_id.app_error",
                    Some(params),
                    "duplicate field ID in batch",
                    400,
                ));
            }
            field_ids.push(value.field_id.clone());
            value.value = sanitize_property_value(&value.value);
        }

        let fields = self.cpa_get_fields(group, caller, &field_ids).await?;
        let by_id: HashMap<&str, &PropertyField> =
            fields.iter().map(|f| (f.id.as_str(), f)).collect();
        for value in &values {
            let mut params = HashMap::new();
            params.insert(
                "FieldID".to_owned(),
                serde_json::Value::String(value.field_id.clone()),
            );
            let Some(field) = by_id.get(value.field_id.as_str()) else {
                return Err(AppError::boxed(
                    WHERE,
                    "app.property_value.upsert.field_not_found.app_error",
                    Some(params),
                    "field not found",
                    404,
                ));
            };
            if field.object_type != object_type {
                // 404 on purpose: "callers cannot distinguish 'no such field' from 'field exists
                // but in a different object-type bucket'".
                return Err(AppError::boxed(
                    WHERE,
                    "app.property_value.upsert.object_type_mismatch.app_error",
                    Some(params),
                    "object type mismatch",
                    404,
                ));
            }
        }

        let result = self
            .cpa_upsert_values_service(group, caller, values)
            .await
            .map_err(|err| err.into_app_error(WHERE, "app.property_value.upsert_many.app_error"))?;

        let (team_id, channel_id) = match self
            .resolve_value_broadcast_params(object_type, target_id)
            .await
        {
            Ok(scope) => scope,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to resolve broadcast params for property values");
                return Ok(result);
            }
        };
        match mm_model::utils::go_json_marshal(&result) {
            Ok(values_json) => {
                let mut message = WebSocketEvent::new(
                    WEBSOCKET_EVENT_PROPERTY_VALUES_UPDATED,
                    &team_id,
                    &channel_id,
                    "",
                    None,
                    connection_id,
                );
                message.add(
                    "object_type",
                    serde_json::Value::String(object_type.to_owned()),
                );
                message.add("target_id", serde_json::Value::String(target_id.to_owned()));
                message.add("values", serde_json::Value::String(values_json));
                self.publish(message).await;
            }
            Err(err) => tracing::warn!(error = %err, "Failed to encode property values to JSON"),
        }
        Ok(result)
    }

    /// `PropertyService.UpsertPropertyValues` (property_value.go:1030): the pre-upsert hooks,
    /// the template refusal, the store, the post-upsert audit.
    /// Port of `resolveValueBroadcastParams` (app/property_value.go:15): `(teamID, channelID)`
    /// for the `property_values_updated` event — a post's channel, a channel itself, and
    /// system-wide for `user` and `system`; any other object type is the 400.
    async fn resolve_value_broadcast_params(
        &self,
        object_type: &str,
        target_id: &str,
    ) -> AppResult<(String, String)> {
        match object_type {
            mm_model::property_field::PROPERTY_FIELD_OBJECT_TYPE_POST => {
                let post = self.get_single_post(target_id, false).await?;
                Ok((String::new(), post.channel_id))
            }
            mm_model::property_field::PROPERTY_FIELD_OBJECT_TYPE_CHANNEL => {
                Ok((String::new(), target_id.to_owned()))
            }
            PROPERTY_FIELD_OBJECT_TYPE_USER | PROPERTY_FIELD_OBJECT_TYPE_SYSTEM => {
                Ok((String::new(), String::new()))
            }
            other => {
                let mut params = HashMap::new();
                params.insert(
                    "ObjectType".to_owned(),
                    serde_json::Value::String(other.to_owned()),
                );
                Err(AppError::boxed(
                    "resolveValueBroadcastParams",
                    "app.property_value.resolve_broadcast_params.unknown_object_type.app_error",
                    Some(params),
                    "unrecognized object type",
                    400,
                ))
            }
        }
    }

    async fn cpa_upsert_values_service(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        values: Vec<PropertyValue>,
    ) -> Result<Vec<PropertyValue>, PropertyServiceError> {
        let managed = is_managed_group(group);
        if managed && !self.cpa_licensed().await? {
            return Err(PropertyServiceError::LicenseRequired);
        }
        let fields = self.fields_for_values(&values).await?;
        if managed {
            for value in &values {
                let Some(field) = fields.get(&value.field_id) else {
                    return Err(PropertyServiceError::FieldNotFound(value.field_id.clone()));
                };
                check_value_write_access(field, caller)
                    .map_err(|err| prefix_field(&value.field_id, err))?;
            }
            self.attribute_validation_values(&values, &fields)?;
        }

        // rejectTemplateValues (property_value.go:780): the same rows, read by id across groups
        // in Go; the ids are the primary key, so the rows are the ones already in hand.
        for field in fields.values() {
            if field.object_type == PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE {
                return Err(PropertyServiceError::App(AppError::boxed(
                    "PropertyService",
                    "app.property_value.template_no_values.app_error",
                    None,
                    format!("template field {} cannot have values", go_quote(&field.id)),
                    400,
                )));
            }
        }

        let upserted = self.store().property().upsert_values(values).await?;

        // PropertyValueAuditHook → App.auditCPAValueChange (app/cpa_value_audit.go:14): one
        // content-level audit record per value, which is a log line on both sides.
        for value in &upserted {
            tracing::info!(
                audit = "cpa_value_change",
                caller_id = %caller.id,
                action = "upsert",
                target_type = %value.target_type,
                target_id = %value.target_id,
                field_id = %value.field_id,
                new_value = %value.value,
                "cpa value change"
            );
        }
        Ok(upserted)
    }

    /// Port of `App.SearchPropertyValues` as `listCPAValues` calls it (custom_profile_attributes
    /// .go:389): every live value of one user in the group, through the post-get hooks — the
    /// licence's empty short-circuit, then read access control, which drops what the caller may
    /// not see — keyed by field id.
    ///
    /// The response is keyed by `field_id`, and a `BTreeMap` is right rather than merely
    /// convenient: Go builds a `map[string]json.RawMessage` and `encoding/json` **sorts map keys**
    /// when it marshals one, so the wire order is the sorted order on both sides.
    #[tracing::instrument(skip_all, fields(group_id = %group.id, user_id = %user_id, values))]
    pub async fn cpa_list_values(
        &self,
        group: &PropertyGroup,
        caller: &PropertyCaller,
        user_id: &str,
    ) -> AppResult<BTreeMap<String, serde_json::Value>> {
        let opts = PropertyValueSearchOpts {
            group_id: group.id.clone(),
            target_type: PROPERTY_VALUE_TARGET_TYPE_USER.to_owned(),
            target_ids: vec![user_id.to_owned()],
            per_page: CPA_READ_PER_PAGE,
            ..PropertyValueSearchOpts::default()
        };
        let values = self
            .store()
            .property()
            .search_values(&opts)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the CPA value search failed");
                AppError::boxed(
                    "SearchPropertyValues",
                    "app.property_value.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("values", values.len());

        let values = self
            .managed_post_get_values(group, values, caller, "SearchPropertyValues")
            .await?;

        Ok(values
            .into_iter()
            .map(|value| (value.field_id, value.value))
            .collect())
    }

    /// `SearchPropertyFields(group, {ObjectType: user, PerPage: limit+5})`, the raw read.
    async fn cpa_search_user_fields(&self, group_id: &str) -> AppResult<Vec<PropertyField>> {
        let opts = PropertyFieldSearchOpts {
            group_id: group_id.to_owned(),
            object_type: PROPERTY_FIELD_OBJECT_TYPE_USER.to_owned(),
            per_page: CPA_READ_PER_PAGE,
            ..PropertyFieldSearchOpts::default()
        };

        self.store()
            .property()
            .search_fields(&opts)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the CPA field search failed");
                AppError::boxed(
                    "SearchPropertyFields",
                    "app.property_field.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }
}

/// Whether the hook chain applies: every hook is constructed with `cpaGroup.ID`
/// (app/server.go:325), the row named `access_control`. By name here rather than id — same
/// predicate, one lookup fewer, and it cannot go stale if the row is ever recreated.
pub fn is_managed_group(group: &PropertyGroup) -> bool {
    group.name == ACCESS_CONTROL_PROPERTY_GROUP_NAME
}

/// `field %s: %w` — the per-field prefix the batch hooks wrap their refusals in.
fn prefix_field(field_id: &str, err: PropertyServiceError) -> PropertyServiceError {
    match err {
        PropertyServiceError::AccessDenied(msg) => {
            PropertyServiceError::AccessDenied(format!("field {field_id}: {msg}"))
        }
        PropertyServiceError::SyncLocked(msg) => {
            PropertyServiceError::SyncLocked(format!("field {field_id}: {msg}"))
        }
        other => other,
    }
}

/// Port of `CanonicalizeSystemObjectField` (app/property_field_helpers.go:33): a **system**
/// object field is pinned to the system target and to sysadmin on all three levels. A no-op for
/// every other object type, the CPA `user` fields included.
pub fn canonicalize_system_object_field(field: &mut PropertyField) {
    if field.object_type != PROPERTY_FIELD_OBJECT_TYPE_SYSTEM {
        return;
    }
    field.target_type = PROPERTY_FIELD_TARGET_LEVEL_SYSTEM.to_owned();
    field.target_id = String::new();
    let sysadmin = PermissionLevel(PermissionLevel::SYSADMIN.to_owned());
    field.permission_field = Some(sysadmin.clone());
    field.permission_values = Some(sysadmin.clone());
    field.permission_options = Some(sysadmin);
}

/// Port of `enforceFieldGroupVersionMatch` (app/properties/property_field.go:99): a PSAv1 field
/// on a PSAv1 group or a PSAv2 field on a PSAv2 group; anything else is 400 `version_mismatch`.
fn enforce_field_group_version_match(
    where_: &'static str,
    group: &PropertyGroup,
    field: &PropertyField,
) -> Result<(), PropertyServiceError> {
    if (group.is_psav1() && field.is_psav1()) || (group.is_psav2() && field.is_psav2()) {
        return Ok(());
    }
    Err(PropertyServiceError::App(AppError::boxed(
        where_,
        "app.property_field.version_mismatch.app_error",
        None,
        "field and group version mismatch",
        400,
    )))
}

/// `app.property_field.<op>.name_conflict.app_error`, **409**, with the name and level as params
/// (property_field.go:240, :454).
fn name_conflict(
    where_: &'static str,
    op: &str,
    field: &PropertyField,
    level: &str,
) -> PropertyServiceError {
    let mut params = HashMap::new();
    params.insert(
        "Name".to_owned(),
        serde_json::Value::String(field.name.clone()),
    );
    params.insert(
        "ConflictLevel".to_owned(),
        serde_json::Value::String(level.to_owned()),
    );
    PropertyServiceError::App(AppError::boxed(
        where_,
        format!("app.property_field.{op}.name_conflict.app_error"),
        Some(params),
        format!(
            "property name {} conflicts with existing {level}-level property",
            go_quote(&field.name)
        ),
        409,
    ))
}

/// Port of `optionsChanged` (app/properties/property_field.go:704): compare the two `options`
/// attrs as id-keyed maps of objects; a length difference, a missing id or a differing object
/// is a change, and two empty lists are not.
fn options_changed(
    old_attrs: Option<&mm_model::utils::StringInterface>,
    new_attrs: Option<&mm_model::utils::StringInterface>,
) -> bool {
    let as_slice = |attrs: Option<&mm_model::utils::StringInterface>| -> Vec<serde_json::Map<String, serde_json::Value>> {
        attrs
            .and_then(|a| a.get(PROPERTY_FIELD_ATTRIBUTE_OPTIONS))
            .and_then(|o| o.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_object().cloned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let old = as_slice(old_attrs);
    let new = as_slice(new_attrs);
    if old.len() != new.len() {
        return true;
    }
    if old.is_empty() {
        return false;
    }
    let new_by_id: HashMap<&str, &serde_json::Map<String, serde_json::Value>> = new
        .iter()
        .filter_map(|opt| opt.get("id").and_then(|id| id.as_str()).map(|id| (id, opt)))
        .filter(|(id, _)| !id.is_empty())
        .collect();
    for old_opt in &old {
        let id = old_opt
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or_default();
        match new_by_id.get(id) {
            None => return true,
            Some(new_opt) if *new_opt != old_opt => return true,
            Some(_) => {}
        }
    }
    false
}

fn conversion_error(where_: &'static str, err: impl std::fmt::Display) -> Box<AppError> {
    tracing::error!(error = %err, "a CPA field would not convert");
    AppError::boxed(
        where_,
        "app.custom_profile_attributes.property_field_conversion.app_error",
        None,
        String::new(),
        500,
    )
}

/// Port of `properties.ErrLicenseRequired` as `mapPropertyServiceError` renders it
/// (app/property_errors.go:44).
///
/// **403, not 501**, and the `detailed_error` is empty: Go's `NewAppError(..., nil, "", 403)`
/// carries the sentinel only through `Wrap`, which never reaches the wire. The same id and status
/// as `getCPAGroup`'s inline check in [`crate::App`]'s API layer, which is the point — Go's own
/// comment says that route exists to reproduce this hook's contract by hand.
pub fn property_licence_refusal(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "app.property.license_error",
        None,
        String::new(),
        403,
    )
}

/// `mapPropertyServiceError`'s `*store.ErrNotFound` arm (app/property_errors.go:62) and the 500
/// fallback the callers wrap around it.
///
/// The id here is `app.property.not_found.app_error` — the **generic** one, because the sentinel
/// `ErrFieldNotFound` is raised only by the multi-id read. A single-field miss goes through the
/// store's plain not-found and lands on this id.
fn property_read_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    let not_found = err.is_not_found();
    if !not_found {
        tracing::error!(error = ?err, "a CPA property read failed");
    }
    AppError::boxed(
        where_,
        if not_found {
            "app.property.not_found.app_error"
        } else {
            "app.property_field.get.app_error"
        },
        None,
        String::new(),
        if not_found { 404 } else { 500 },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two not-found ids are **one word apart and mean different things**: a single-field read
    /// that misses is `app.property.not_found.app_error`, a batch read that is short a row is
    /// `app.property_field.not_found.app_error`. Both are 404, so a port that reused one would
    /// pass every status assertion and answer the wrong id to a client that branches on it.
    #[test]
    fn the_single_and_batch_misses_carry_different_ids() {
        let single = property_read_error(
            "GetPropertyField",
            StoreError::NotFound {
                entity: "PropertyField",
                criteria: "Id=x".to_owned(),
            },
        );
        assert_eq!(single.id, "app.property.not_found.app_error");
        assert_eq!(single.status_code, 404);
        let batch = PropertyServiceError::FieldNotFound("x".to_owned())
            .into_app_error("GetPropertyFields", "app.property_field.get_many.app_error");
        assert_eq!(batch.id, "app.property_field.not_found.app_error");
        assert_eq!(batch.status_code, 404);
        assert_ne!(single.id, batch.id);
    }

    /// A driver failure on a single-field read is a 500 with its own id, not the 404.
    #[test]
    fn a_broken_field_read_is_a_five_hundred() {
        let broken = property_read_error(
            "GetPropertyField",
            StoreError::Db {
                context: "boom".to_owned(),
                source: sqlx::Error::RowNotFound,
            },
        );
        assert_eq!(broken.id, "app.property_field.get.app_error");
        assert_eq!(broken.status_code, 500);
    }

    /// **403, and an empty `detailed_error`.** The status is the one thing a licence refusal in
    /// this tree does not agree on — the neighbouring gated reads answer 501 — and Go's own
    /// `getCPAGroup` comment exists because this contract is easy to get wrong.
    #[test]
    fn the_licence_refusal_is_a_forbidden() {
        let refusal = property_licence_refusal("SearchPropertyFields");
        assert_eq!(refusal.id, "app.property.license_error");
        assert_eq!(refusal.status_code, 403);
        assert_eq!(refusal.detailed_error, "");
        // And the hook's own sentinel maps to the same thing.
        let mapped = PropertyServiceError::LicenseRequired.into_app_error("x", "y");
        assert_eq!(
            (mapped.id.as_str(), mapped.status_code),
            (refusal.id.as_str(), 403)
        );
    }

    /// The page size is `AccessControlGroupFieldLimit + 5`, and the `+ 5` is load-bearing: it is
    /// how Go tells "a full page" from "the group is at its cap", so a port that asked for
    /// exactly the limit would silently drop the field that proves the cap was hit.
    #[test]
    fn the_read_page_is_the_group_limit_plus_five() {
        assert_eq!(CPA_READ_PER_PAGE, 205);
        assert_eq!(CPA_READ_PER_PAGE, ACCESS_CONTROL_GROUP_FIELD_LIMIT + 5);
    }

    fn attrs_with_options(options: serde_json::Value) -> mm_model::utils::StringInterface {
        let mut attrs = mm_model::utils::StringInterface::new();
        attrs.insert("options".to_owned(), options);
        attrs
    }

    /// `optionsChanged` compares by id, not by position: the same options in another order are
    /// **not** a change, a renamed option is, and two empty lists are not.
    #[test]
    fn options_changed_compares_by_id_and_ignores_order() {
        let a =
            attrs_with_options(serde_json::json!([{"id":"1","name":"x"},{"id":"2","name":"y"}]));
        let b =
            attrs_with_options(serde_json::json!([{"id":"2","name":"y"},{"id":"1","name":"x"}]));
        let c =
            attrs_with_options(serde_json::json!([{"id":"1","name":"z"},{"id":"2","name":"y"}]));
        let d = attrs_with_options(serde_json::json!([{"id":"1","name":"x"}]));
        assert!(!options_changed(Some(&a), Some(&b)));
        assert!(options_changed(Some(&a), Some(&c)));
        assert!(options_changed(Some(&a), Some(&d)));
        assert!(!options_changed(None, None));
        assert!(!options_changed(
            Some(&attrs_with_options(serde_json::json!([]))),
            None
        ));
    }

    /// `mapPropertyServiceError`'s table, sentinel by sentinel, with the two statuses that are
    /// not 400/403: the limits are **422** and the stale write is **409**.
    #[test]
    fn every_service_error_maps_to_its_id_and_status() {
        let cases: Vec<(PropertyServiceError, &str, i32)> = vec![
            (
                PropertyServiceError::AccessDenied(String::new()),
                "app.property.access_denied.app_error",
                403,
            ),
            (
                PropertyServiceError::SyncLocked(String::new()),
                "app.property.sync_lock.app_error",
                403,
            ),
            (
                PropertyServiceError::InvalidAccessMode(String::new()),
                "app.property.invalid_access_mode.app_error",
                400,
            ),
            (
                PropertyServiceError::FieldLimitReached(String::new()),
                "app.property_field.create.limit_reached.app_error",
                422,
            ),
            (
                PropertyServiceError::GroupFieldLimitReached(String::new()),
                "app.property_field.create.group_limit_reached.app_error",
                422,
            ),
            (
                PropertyServiceError::InvalidFieldAttrs(String::new()),
                "app.property_field.invalid_attrs.app_error",
                400,
            ),
            (
                PropertyServiceError::InvalidValue(String::new()),
                "app.property_value.validate.app_error",
                400,
            ),
            (
                PropertyServiceError::AdminRequired(String::new()),
                "app.property_field.managed_admin.permission.app_error",
                403,
            ),
            (
                PropertyServiceError::Store(StoreError::Stale {
                    entity: "PropertyField",
                    detail: "x",
                }),
                "app.property_field.update.conflict.app_error",
                409,
            ),
            (
                PropertyServiceError::Store(StoreError::NotFound {
                    entity: "PropertyField",
                    criteria: String::new(),
                }),
                "app.property.not_found.app_error",
                404,
            ),
            (
                PropertyServiceError::Store(StoreError::Db {
                    context: String::new(),
                    source: sqlx::Error::RowNotFound,
                }),
                "fallback",
                500,
            ),
        ];
        for (err, id, status) in cases {
            let mapped = err.into_app_error("w", "fallback");
            assert_eq!((mapped.id.as_str(), mapped.status_code), (id, status));
        }
    }
}
