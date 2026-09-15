//! Port of the four remaining writes of `api4/properties.go` — `createPropertyField` (:65),
//! `patchPropertyField` (:419), `patchPropertyValues` (:681) and `patchSystemPropertyValues`
//! (:705):
//!
//! ```text
//! POST  /api/v4/properties/groups/{group_name}/{object_type}/fields
//! PATCH /api/v4/properties/groups/{group_name}/{object_type}/fields/{field_id}
//! PATCH /api/v4/properties/groups/{group_name}/{object_type}/values/{target_id}
//! PATCH /api/v4/properties/groups/{group_name}/system/values
//! ```
//!
//! The reads and the delete live in [`crate::properties`], whose gates these share: the
//! `properties_api_enabled` flag (closed, Go writes its own mux 404), `getV2Group`, and the
//! `Require*` checks on the path. Behind the handler each write is the app function the CPA
//! family already ported with the group as an argument — `cpa_create_field`, `cpa_update_field`
//! and the generalised [`mm_app::App::upsert_property_values`] — whose hook chain runs only
//! for the `access_control` group, so on `boards` and `post_attributes` these are ordinary
//! writes on any edition.
//!
//! # What the handlers decide, and in which order
//!
//! - **Create:** the body (`err != nil || field == nil`), `protected` refused before anything
//!   is canonicalised, the template arm's `manage_system`, then the target-type switch —
//!   `channel` and `team` want a target id and a scoped permission, `system` wants
//!   `manage_system`, anything else is `invalid_target_type` — and the permission levels: a
//!   non-admin's three are **pinned** to the default, an admin's are filled in only where nil.
//! - **Patch field:** the body, `target_id`/`target_type` erased before `IsValid`, the row,
//!   PSAv1 refused, the URL's object type matched (404, "indistinguishable from no such
//!   field"), then options-only against full-edit permission — options-only collapses to a
//!   full edit when the type does not support options.
//! - **Patch values:** the two object-type refusals **before** `RequireTargetId`, the body
//!   (`null` is an empty batch, not a decode failure), `hasTargetAccess` for a **write**, the
//!   empty and 50-item limits, the id checks, the fields, and per field the object-type match
//!   and `SessionHasPermissionToSetPropertyFieldValues`.
//!
//! The websocket events (`property_field_created`, `property_field_updated`,
//! `property_values_updated`) are published by the app layer, scoped as
//! `resolveValueBroadcastParams` scopes them. The audit records are log lines.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::custom_profile_attributes::canonicalize_system_object_field;
use mm_app::property_hooks::PropertyCaller;
use mm_model::channel::{CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE};
use mm_model::permission::{
    PERMISSION_CREATE_POST, PERMISSION_EDIT_OTHER_USERS,
    PERMISSION_MANAGE_PRIVATE_CHANNEL_PROPERTIES, PERMISSION_MANAGE_PUBLIC_CHANNEL_PROPERTIES,
    PERMISSION_MANAGE_SYSTEM, PERMISSION_MANAGE_TEAM, PERMISSION_READ_CHANNEL,
};
use mm_model::property_field::{
    PROPERTY_FIELD_OBJECT_TYPE_CHANNEL, PROPERTY_FIELD_OBJECT_TYPE_POST,
    PROPERTY_FIELD_OBJECT_TYPE_SYSTEM, PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE,
    PROPERTY_FIELD_OBJECT_TYPE_USER, PermissionLevel, PropertyField, PropertyFieldPatch,
    is_valid_property_field_object_type,
};
use mm_model::property_group::{PropertyGroup, is_valid_property_group_name};
use mm_model::property_value::{
    PROPERTY_VALUE_SYSTEM_TARGET_ID, PropertyValue, PropertyValuePatchItem,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::custom_profile_attributes::{decode_struct, first_value, is_options_only_patch};
use crate::error::ApiError;
use crate::properties::{
    CONNECTION_ID_HEADER, Group, encoded, permission_error, refusal, v2_group,
};
use crate::proxy;

/// `maxPropertyValuePatchItems` (properties.go:20).
const MAX_PROPERTY_VALUE_PATCH_ITEMS: usize = 50;

/// `DefaultPropertyFieldPermissionLevel` (app/property_field_helpers.go:15): sysadmin for a
/// template or system field, member for the rest.
fn default_permission_level(field: &PropertyField) -> PermissionLevel {
    if field.object_type == PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE
        || field.object_type == PROPERTY_FIELD_OBJECT_TYPE_SYSTEM
    {
        PermissionLevel(PermissionLevel::SYSADMIN.to_owned())
    } else {
        PermissionLevel(PermissionLevel::MEMBER.to_owned())
    }
}

/// The request body and the `Connection-Id` header, with the request consumed.
async fn body_and_connection_id(request: Request) -> Result<(Vec<u8>, String), ()> {
    let connection_id = request
        .headers()
        .get(CONNECTION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
        })?;
    Ok((bytes.to_vec(), connection_id))
}

/// `json.NewEncoder(w).Encode(v)` behind `w.WriteHeader(http.StatusCreated)`.
fn created(value: &impl serde::Serialize, where_: &'static str) -> Response {
    let mut response = encoded(value, where_);
    if response.status() == StatusCode::OK {
        *response.status_mut() = StatusCode::CREATED;
    }
    response
}

/// Port of `createPropertyField` (properties.go:65) —
/// `POST /api/v4/properties/groups/{group_name}/{object_type}/fields`.
#[tracing::instrument(skip_all, fields(group = %group_name, object_type = %object_type))]
pub async fn create_property_field(
    State(state): State<AppState>,
    Path((group_name, object_type)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "createPropertyField";
    if !state.app.config().properties_api_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    if !is_valid_property_group_name(&group_name) {
        return ApiError::invalid_url_param("group_name").into_response();
    }
    if !is_valid_property_field_object_type(&object_type) {
        return ApiError::invalid_url_param("object_type").into_response();
    }

    let group = match v2_group(&state, &group_name, WHERE).await {
        Group::Serve(group) => group,
        Group::Forward => return proxy::forward_to_go(State(state), request).await,
        Group::Failed(err) => return err.into_response(),
    };

    let Ok((bytes, connection_id)) = body_and_connection_id(request).await else {
        return ApiError::invalid_param("property_field").into_response();
    };
    let Some(mut field) = decode_struct::<PropertyField>(&bytes) else {
        return ApiError::invalid_param("property_field").into_response();
    };

    field.object_type = object_type;
    field.group_id = group.id.clone();

    if field.protected {
        return refusal(
            WHERE,
            "api.property_field.create.protected_via_api.app_error",
            400,
        )
        .into_response();
    }

    // "Pre-canonicalize system objects so the scope check below cannot be bypassed by
    // submitting ObjectType=system with TargetType=channel."
    canonicalize_system_object_field(&mut field);

    let is_admin = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;

    // "Templates are always sysadmin-only, regardless of TargetType."
    if field.object_type == PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE && !is_admin {
        return permission_error(&session.0, &PERMISSION_MANAGE_SYSTEM).into_response();
    }

    match field.target_type.as_str() {
        "channel" => {
            if field.target_id.is_empty() {
                return refusal(
                    WHERE,
                    "api.property_field.create.target_id_required.app_error",
                    400,
                )
                .into_response();
            }
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(
                    &session.0,
                    &field.target_id,
                    &PERMISSION_CREATE_POST,
                )
                .await;
            if !allowed {
                return permission_error(&session.0, &PERMISSION_CREATE_POST).into_response();
            }
        }
        "team" => {
            if field.target_id.is_empty() {
                return refusal(
                    WHERE,
                    "api.property_field.create.target_id_required.app_error",
                    400,
                )
                .into_response();
            }
            if !state
                .app
                .session_has_permission_to_team(
                    &session.0,
                    &field.target_id,
                    &PERMISSION_MANAGE_TEAM,
                )
                .await
            {
                return permission_error(&session.0, &PERMISSION_MANAGE_TEAM).into_response();
            }
        }
        "system" => {
            if !is_admin {
                return permission_error(&session.0, &PERMISSION_MANAGE_SYSTEM).into_response();
            }
        }
        _ => {
            return refusal(
                WHERE,
                "api.property_field.create.invalid_target_type.app_error",
                400,
            )
            .into_response();
        }
    }

    // "Default permission levels: pin all three for non-admins, nil-fill for admins."
    let default_level = default_permission_level(&field);
    if !is_admin {
        field.permission_field = Some(default_level.clone());
        field.permission_values = Some(default_level.clone());
        field.permission_options = Some(default_level);
    } else {
        if field.permission_field.is_none() {
            field.permission_field = Some(default_level.clone());
        }
        if field.permission_values.is_none() {
            field.permission_values = Some(default_level.clone());
        }
        if field.permission_options.is_none() {
            field.permission_options = Some(default_level);
        }
    }

    field.created_by = session.0.user_id.clone();
    field.updated_by = session.0.user_id.clone();

    let caller = PropertyCaller::from_session(&session.0);
    match state
        .app
        .cpa_create_field(&group, &caller, field, &connection_id)
        .await
    {
        Ok(created_field) => created(&created_field, WHERE),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `patchPropertyField` (properties.go:419) —
/// `PATCH /api/v4/properties/groups/{group_name}/{object_type}/fields/{field_id}`.
#[tracing::instrument(skip_all, fields(group = %group_name, object_type = %object_type, field_id = %field_id))]
pub async fn patch_property_field(
    State(state): State<AppState>,
    Path((group_name, object_type, field_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "patchPropertyField";
    if !state.app.config().properties_api_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    if !is_valid_property_group_name(&group_name) {
        return ApiError::invalid_url_param("group_name").into_response();
    }
    if !is_valid_property_field_object_type(&object_type) {
        return ApiError::invalid_url_param("object_type").into_response();
    }
    if !is_valid_id(&field_id) {
        return ApiError::invalid_url_param("field_id").into_response();
    }

    let group = match v2_group(&state, &group_name, WHERE).await {
        Group::Serve(group) => group,
        Group::Forward => return proxy::forward_to_go(State(state), request).await,
        Group::Failed(err) => return err.into_response(),
    };

    let Ok((bytes, connection_id)) = body_and_connection_id(request).await else {
        return ApiError::invalid_param("property_field_patch").into_response();
    };
    let Some(mut patch) = decode_struct::<PropertyFieldPatch>(&bytes) else {
        return ApiError::invalid_param("property_field_patch").into_response();
    };

    if let Some(name) = patch.name.as_mut() {
        *name = name.trim().to_owned();
    }
    patch.target_id = None;
    patch.target_type = None;

    if let Err(err) = patch.is_valid() {
        return ApiError::from(err).into_response();
    }

    let caller = PropertyCaller::from_session(&session.0);
    let mut existing = match state
        .app
        .get_property_field(&group, &field_id, &caller)
        .await
    {
        Ok(field) => field,
        Err(err) => return ApiError::from(err).into_response(),
    };

    // "PSAv2 routes only operate on PSAv2 fields. Reject legacy fields."
    if existing.is_psav1() {
        return refusal(
            WHERE,
            "api.property_field.patch.legacy_field.app_error",
            400,
        )
        .into_response();
    }
    if existing.object_type != object_type {
        return refusal(
            WHERE,
            "api.property_field.object_type_mismatch.app_error",
            404,
        )
        .into_response();
    }

    let mut options_only = is_options_only_patch(&patch);
    if options_only && !existing.type_.supports_options() {
        options_only = false;
    }
    if options_only {
        if !state
            .app
            .session_has_permission_to_manage_property_field_options(&session.0, &existing)
            .await
        {
            return refusal(
                WHERE,
                "api.property_field.update.no_options_permission.app_error",
                403,
            )
            .into_response();
        }
    } else if !state
        .app
        .session_has_permission_to_edit_property_field(&session.0, &existing)
        .await
    {
        return refusal(
            WHERE,
            "api.property_field.update.no_field_permission.app_error",
            403,
        )
        .into_response();
    }

    existing.patch(&patch, true);
    existing.updated_by = session.0.user_id.clone();

    match state
        .app
        .cpa_update_field(&group, &caller, existing, &connection_id)
        .await
    {
        Ok(updated) => encoded(&updated.field, WHERE),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `patchPropertyValues` (properties.go:681) —
/// `PATCH /api/v4/properties/groups/{group_name}/{object_type}/values/{target_id}`.
///
/// The two object-type refusals come **before** `RequireTargetId`, so `template` and `system`
/// are refused with their own ids even for a target that is not an id.
#[tracing::instrument(skip_all, fields(group = %group_name, object_type = %object_type, target_id = %target_id))]
pub async fn patch_property_values(
    State(state): State<AppState>,
    Path((group_name, object_type, target_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "patchPropertyValues";
    if !state.app.config().properties_api_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    if !is_valid_property_group_name(&group_name) {
        return ApiError::invalid_url_param("group_name").into_response();
    }
    if !is_valid_property_field_object_type(&object_type) {
        return ApiError::invalid_url_param("object_type").into_response();
    }
    if object_type == PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE {
        return refusal(
            WHERE,
            "api.property_value.template_no_values.app_error",
            400,
        )
        .into_response();
    }
    if object_type == PROPERTY_FIELD_OBJECT_TYPE_SYSTEM {
        return refusal(
            WHERE,
            "api.property_value.system_use_dedicated_route.app_error",
            400,
        )
        .into_response();
    }
    if !is_valid_id(&target_id) {
        return ApiError::invalid_url_param("target_id").into_response();
    }
    patch_values_core(
        state,
        &group_name,
        &object_type,
        &target_id,
        session,
        request,
    )
    .await
}

/// Port of `patchSystemPropertyValues` (properties.go:705) —
/// `PATCH /api/v4/properties/groups/{group_name}/system/values`.
#[tracing::instrument(skip_all, fields(group = %group_name))]
pub async fn patch_system_property_values(
    State(state): State<AppState>,
    Path(group_name): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().properties_api_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    if !is_valid_property_group_name(&group_name) {
        return ApiError::invalid_url_param("group_name").into_response();
    }
    patch_values_core(
        state,
        &group_name,
        PROPERTY_FIELD_OBJECT_TYPE_SYSTEM,
        PROPERTY_VALUE_SYSTEM_TARGET_ID,
        session,
        request,
    )
    .await
}

/// `Decode(&items)` where `items` is `[]PropertyValuePatchItem`: `null` is a nil slice and no
/// error (the empty-batch refusal follows), an array decodes item by item, anything else is
/// the decode error.
fn decode_items(body: &[u8]) -> Option<Vec<PropertyValuePatchItem>> {
    match first_value(body)? {
        serde_json::Value::Null => Some(Vec::new()),
        array @ serde_json::Value::Array(_) => serde_json::from_value(array).ok(),
        _ => None,
    }
}

/// What the write half of `hasTargetAccess` decided.
enum WriteAccess {
    Allowed,
    Denied(ApiError),
}

/// `hasTargetAccess(c, objectType, targetID, write=true)` (properties.go:826): the channel's
/// type picks `manage_public_channel_properties`, `manage_private_channel_properties` or, for
/// a direct or group channel, `read_channel`; a post wants `create_post` in its channel;
/// another user's values want `edit_other_users` (self and the unrestricted session pass
/// first); `system` wants `manage_system`; `template` is `template_no_values`.
async fn has_target_write_access(
    state: &AppState,
    session: &AuthenticatedSession,
    object_type: &str,
    target_id: &str,
) -> WriteAccess {
    match object_type {
        PROPERTY_FIELD_OBJECT_TYPE_CHANNEL => {
            let channel = match state.app.get_channel(target_id).await {
                Ok(channel) => channel,
                Err(err) => return WriteAccess::Denied(ApiError::from(err)),
            };
            let permission = match channel.channel_type.as_str() {
                CHANNEL_TYPE_OPEN => &PERMISSION_MANAGE_PUBLIC_CHANNEL_PROPERTIES,
                CHANNEL_TYPE_PRIVATE => &PERMISSION_MANAGE_PRIVATE_CHANNEL_PROPERTIES,
                _ => &PERMISSION_READ_CHANNEL,
            };
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(&session.0, target_id, permission)
                .await;
            if !allowed {
                return WriteAccess::Denied(permission_error(&session.0, permission));
            }
        }
        PROPERTY_FIELD_OBJECT_TYPE_POST => {
            let post = match state.app.get_single_post(target_id, false).await {
                Ok(post) => post,
                Err(err) => return WriteAccess::Denied(ApiError::from(err)),
            };
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(
                    &session.0,
                    &post.channel_id,
                    &PERMISSION_CREATE_POST,
                )
                .await;
            if !allowed {
                return WriteAccess::Denied(permission_error(&session.0, &PERMISSION_CREATE_POST));
            }
        }
        PROPERTY_FIELD_OBJECT_TYPE_USER => {
            if target_id == session.0.user_id || session.0.is_unrestricted() {
                return WriteAccess::Allowed;
            }
            if !state
                .app
                .session_has_permission_to(&session.0, &PERMISSION_EDIT_OTHER_USERS)
                .await
            {
                return WriteAccess::Denied(permission_error(
                    &session.0,
                    &PERMISSION_EDIT_OTHER_USERS,
                ));
            }
        }
        PROPERTY_FIELD_OBJECT_TYPE_SYSTEM => {
            if !state
                .app
                .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
                .await
            {
                return WriteAccess::Denied(permission_error(
                    &session.0,
                    &PERMISSION_MANAGE_SYSTEM,
                ));
            }
        }
        PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE => {
            return WriteAccess::Denied(refusal(
                "hasTargetAccess",
                "api.property_value.template_no_values.app_error",
                400,
            ));
        }
        _ => {
            return WriteAccess::Denied(refusal(
                "hasTargetAccess",
                "api.property_value.invalid_object_type.app_error",
                400,
            ));
        }
    }
    WriteAccess::Allowed
}

/// Port of `patchPropertyValuesCore` (properties.go:714), shared by the two value routes.
async fn patch_values_core(
    state: AppState,
    group_name: &str,
    object_type: &str,
    target_id: &str,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "patchPropertyValues";
    let group: Box<PropertyGroup> = match v2_group(&state, group_name, WHERE).await {
        Group::Serve(group) => group,
        Group::Forward => return proxy::forward_to_go(State(state), request).await,
        Group::Failed(err) => return err.into_response(),
    };

    let Ok((bytes, connection_id)) = body_and_connection_id(request).await else {
        return ApiError::invalid_param("property_values").into_response();
    };
    let Some(items) = decode_items(&bytes) else {
        return ApiError::invalid_param("property_values").into_response();
    };

    if let WriteAccess::Denied(err) =
        has_target_write_access(&state, &session, object_type, target_id).await
    {
        return err.into_response();
    }

    if items.is_empty() {
        return refusal(WHERE, "api.property_value.patch.empty_body.app_error", 400)
            .into_response();
    }
    if items.len() > MAX_PROPERTY_VALUE_PATCH_ITEMS {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "Max".to_owned(),
            serde_json::Value::from(MAX_PROPERTY_VALUE_PATCH_ITEMS),
        );
        return ApiError::from(AppError::new(
            WHERE,
            "api.property_value.patch.too_many_items.request_error",
            Some(params),
            String::new(),
            400,
        ))
        .into_response();
    }

    let mut seen = std::collections::HashSet::new();
    let mut field_ids = Vec::with_capacity(items.len());
    for item in &items {
        if !is_valid_id(&item.field_id) {
            return refusal(
                WHERE,
                "api.property_value.patch.invalid_field_id.app_error",
                400,
            )
            .into_response();
        }
        if !seen.insert(item.field_id.as_str()) {
            return refusal(
                WHERE,
                "api.property_value.patch.duplicate_field_id.app_error",
                400,
            )
            .into_response();
        }
        field_ids.push(item.field_id.clone());
    }

    let caller = PropertyCaller::from_session(&session.0);
    let fields = match state.app.cpa_get_fields(&group, &caller, &field_ids).await {
        Ok(fields) => fields,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let by_id: std::collections::HashMap<&str, &PropertyField> =
        fields.iter().map(|f| (f.id.as_str(), f)).collect();
    for item in &items {
        let Some(field) = by_id.get(item.field_id.as_str()) else {
            let mut params = std::collections::HashMap::new();
            params.insert(
                "FieldID".to_owned(),
                serde_json::Value::String(item.field_id.clone()),
            );
            return ApiError::from(AppError::new(
                WHERE,
                "api.property_value.patch.field_not_found.app_error",
                Some(params),
                String::new(),
                404,
            ))
            .into_response();
        };
        if field.object_type != object_type {
            return refusal(
                WHERE,
                "api.property_field.object_type_mismatch.app_error",
                404,
            )
            .into_response();
        }
        if !state
            .app
            .session_has_permission_to_set_property_field_values(&session.0, field, target_id)
            .await
        {
            return refusal(
                WHERE,
                "api.property_value.patch.no_values_permission.app_error",
                403,
            )
            .into_response();
        }
    }

    let user_id = session.0.user_id.clone();
    let values: Vec<PropertyValue> = items
        .into_iter()
        .map(|item| PropertyValue {
            target_id: target_id.to_owned(),
            target_type: object_type.to_owned(),
            group_id: group.id.clone(),
            field_id: item.field_id,
            value: item.value,
            created_by: user_id.clone(),
            updated_by: user_id.clone(),
            ..PropertyValue::default()
        })
        .collect();

    match state
        .app
        .upsert_property_values(
            &group,
            &caller,
            values,
            object_type,
            target_id,
            &connection_id,
        )
        .await
    {
        Ok(upserted) => encoded(&upserted, WHERE),
        Err(err) => ApiError::from(err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Decode(&items)` into a slice: `null` is an empty batch, an object or a scalar is the
    /// decode error, an array of objects decodes, an array of scalars does not.
    #[test]
    fn the_items_body_is_decoded_like_gos_slice_decoder() {
        assert_eq!(decode_items(b"null").unwrap().len(), 0);
        assert_eq!(decode_items(b"[]").unwrap().len(), 0);
        assert!(decode_items(b"{}").is_none());
        assert!(decode_items(b"5").is_none());
        assert!(decode_items(b"").is_none());
        assert!(decode_items(b"[1]").is_none());
        let items = decode_items(br#"[{"field_id":"abc","value":"x"} trailing"#).unwrap();
        assert_eq!(items[0].field_id, "abc");
    }

    #[test]
    fn the_default_level_is_sysadmin_only_for_template_and_system() {
        let mut field = PropertyField::default();
        assert_eq!(default_permission_level(&field).0, "member");
        field.object_type = PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE.to_owned();
        assert_eq!(default_permission_level(&field).0, "sysadmin");
        field.object_type = PROPERTY_FIELD_OBJECT_TYPE_SYSTEM.to_owned();
        assert_eq!(default_permission_level(&field).0, "sysadmin");
        field.object_type = PROPERTY_FIELD_OBJECT_TYPE_USER.to_owned();
        assert_eq!(default_permission_level(&field).0, "member");
    }
}
