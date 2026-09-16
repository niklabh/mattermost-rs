//! Port of seven of the eight routes in `api4/custom_profile_attributes.go` — the "User
//! Attributes" API, which keeps the older CPA name in every identifier and URL for backward
//! compatibility (MM-68235).
//!
//! The eighth, `GET /custom_profile_attributes/group`, was ported earlier and lives in
//! [`crate::gated_reads`]: it is the one CPA route whose licence check is written out inline in
//! the handler, because `GetPropertyGroup` is the one property call the licence hook does not
//! cover. The other seven get theirs from the hook, and that difference is this module's subject.
//!
//! # The gate is a licence, it is not uniform, and it is not first
//!
//! `App.Srv().propertyService` registers a `LicenseCheckHook` scoped to the `access_control`
//! group as its **first** hook (app/server.go:322). Unlicensed it refuses with 403
//! `app.property.license_error`. But the hook has fourteen arms, and they do not all fire:
//!
//! - `PreCreatePropertyField` refuses unconditionally, so **POST /fields is always a 403**.
//! - `PostGetPropertyField` runs only after the row is **found**, so a patch or delete of an
//!   unknown field is **404 `app.property.not_found.app_error`**, not a 403.
//! - `PostGetPropertyFields` and `PostGetPropertyValues` return `nil` when handed an empty
//!   slice, so an unlicensed **`GET /fields` answers `[]`** on a group with no user fields and
//!   **`GET /users/{id}/custom_profile_attributes` answers `{}`** for a user with no values —
//!   both 200, both real reads.
//! - The batch value patch reads its fields by id first, so a batch naming a field that does not
//!   exist is **404 `app.property_field.not_found.app_error`** — a different id from the
//!   single-field miss, one word apart.
//!
//! Every one of those was checked against the Go server on this stack, twice: once against the
//! seeded database, and once with an `access_control` user field inserted, which flips the two
//! 200s to 403s and the 404s to 403s. A port that answered a flat 403 to all seven would be
//! wrong on five.
//!
//! # And past the gate, a licensed server is served too
//!
//! Since 2026-09-13 nothing here forwards on the licence. The success path of every write —
//! `CreatePropertyField`/`UpdatePropertyField`/`DeletePropertyField`/`UpsertPropertyValues`,
//! the access-control and attribute-validation hooks, the field limit, the type-change cleanup
//! and the four CPA websocket events — is [`mm_app::App`]'s, and this module is the handler
//! logic proper: the body, the id, the permission, the object-type check, the CPA event, in Go's
//! order. Compared against the licensed Go oracle by `parity::custom_profile_attributes`.
//!
//! # The permission checks that only a licensed server reaches
//!
//! `patchCPAField` chooses between two checks on the **shape of the patch**: a patch that touches
//! only `attrs.options` on a field whose type has options needs
//! `SessionHasPermissionToManagePropertyFieldOptions`; anything else needs
//! `SessionHasPermissionToEditPropertyField`, which additionally refuses a protected field
//! outright. Both levels are pinned to `sysadmin` by the attribute hook for every field in this
//! group, so in practice both mean `manage_system` — but the two refusals carry different ids.
//! `cpaPatchValues` asks `SessionHasPermissionToSetPropertyFieldValues` per field against the
//! **target user**, which for a `user` field on a system target is "any authenticated user" at
//! `member` level and `manage_system` at `sysadmin` level (a `managed: admin` field).
//!
//! # The one hand-over left
//!
//! `hasTargetAccess`'s read arm asks `UserCanSeeOtherUser`, which needs the team and channel
//! membership lookups this port lacks whenever the caller's account carries view restrictions.
//! That request is forwarded untouched; it is a permission question, not a licence one.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::property_hooks::PropertyCaller;
use mm_model::custom_profile_attributes::CPAField;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM, PERMISSION_VIEW_MEMBERS,
    make_permission_error,
};
use mm_model::property_field::{
    PROPERTY_FIELD_ATTRIBUTE_OPTIONS, PROPERTY_FIELD_OBJECT_TYPE_USER,
    PROPERTY_FIELD_TARGET_LEVEL_SYSTEM, PropertyFieldPatch,
};
use mm_model::utils::{AppError, is_valid_id};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_CPA_FIELD_CREATED, WEBSOCKET_EVENT_CPA_FIELD_DELETED,
    WEBSOCKET_EVENT_CPA_FIELD_UPDATED, WEBSOCKET_EVENT_CPA_VALUES_UPDATED, WebSocketEvent,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::resolve_me;
use crate::error::ApiError;

/// Port of `maxPropertyValuePatchItems` (api4/properties.go:20).
const MAX_PROPERTY_VALUE_PATCH_ITEMS: usize = 50;

/// `model.ConnectionId` — the header a websocket client sends so its own connection is omitted
/// from the broadcast of what it did.
const CONNECTION_ID_HEADER: &str = "Connection-Id";

/// `json.NewEncoder(w).Encode(v)` — a JSON body **with** the encoder's trailing newline ([D-086]),
/// at `status`. Go's encoder escapes `<`, `>` and `&`, which matters here: field names, option
/// names and text values are user-supplied.
fn encoded_with_status(
    status: StatusCode,
    value: &impl serde::Serialize,
    where_: &'static str,
) -> Response {
    let mut body = match mm_model::utils::go_json_marshal(value) {
        Ok(body) => body.into_bytes(),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise a CPA response");
            return ApiError::from(AppError::new(
                where_,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    body.push(b'\n');

    (
        status,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

fn encoded(value: &impl serde::Serialize, where_: &'static str) -> Response {
    encoded_with_status(StatusCode::OK, value, where_)
}

/// `web.ReturnStatusOK` (web/handlers.go) — `{"status":"OK"}` with **no** trailing newline.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}

fn connection_id(request: &Request) -> String {
    request
        .headers()
        .get(CONNECTION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// `json.NewDecoder(r.Body).Decode(&v)` over a body already read into memory: the **first** JSON
/// value in it, or `None` when there is none.
///
/// Go's decoder reads one value and ignores whatever follows, where `serde_json::from_slice`
/// rejects the trailing token — which would turn a body Go accepts into a 400. An empty body is
/// `io.EOF`, an error rather than a zero value, so it is `None` here too.
pub(crate) fn first_value(body: &[u8]) -> Option<serde_json::Value> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()
}

/// `Decode(&v)` where `v` is a `*Struct`, as both field handlers write it.
///
/// `None` is Go's `err != nil || v == nil` — one branch, because both handlers take it together.
/// **A JSON array is not a struct**: `encoding/json` refuses it, and so must this, which serde
/// would not do on its own (a struct deserialises happily from a sequence of its fields, so `[]`
/// would otherwise decode to an all-default patch and a caller sending `[]` would get a *write*
/// where Go gives a 400). A literal `null` decodes without error in Go and leaves the pointer
/// nil, which lands on the same `None`.
pub(crate) fn decode_struct<T: serde::de::DeserializeOwned>(body: &[u8]) -> Option<T> {
    match first_value(body)? {
        object @ serde_json::Value::Object(_) => serde_json::from_value(object).ok(),
        _ => None,
    }
}

/// `Decode(&v)` where `v` is a `map[string]json.RawMessage`, as both value handlers write it.
///
/// The difference from [`decode_struct`] is `null`: it decodes into a **nil map without an
/// error**, so the handler falls through to the empty-batch refusal instead of answering
/// `invalid_body_param`. Those are two different error ids at the same status from the same four
/// bytes, and Go gives the second.
fn decode_map(body: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
    match first_value(body)? {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => Some(serde_json::Map::new()),
        _ => None,
    }
}

/// Port of `listCPAFields` (api4/custom_profile_attributes.go:34) —
/// `GET /api/v4/custom_profile_attributes/fields`.
///
/// Unlicensed this is `[]` or a 403, and which one depends on whether the `access_control` group
/// holds a `user`-object field (module docs). Licensed it is the fields, sorted by `sort_order`
/// then id, each as the caller may see it.
#[tracing::instrument(skip_all)]
pub async fn list_cpa_fields(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    let caller = PropertyCaller::from_session(&session.0);
    let group = match state.app.cpa_property_group().await {
        Ok(group) => group,
        Err(err) => return ApiError::from(err).into_response(),
    };
    match state.app.cpa_list_fields(&group, &caller).await {
        Ok(fields) => encoded(&fields, "listCPAFields"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `createCPAField` (:62) — `POST /api/v4/custom_profile_attributes/fields`.
///
/// # Three things happen before the licence, and all three are comparable on any server
///
/// The body is decoded into a `*model.CPAField` — a bad body, a bare `null` and a JSON array are
/// all **400 `api.context.invalid_body_param.app_error`** naming `property_field`. Then
/// `PermissionManageSystem`, which is the scope check the generic property handler would have
/// applied to a system-typed field, so a non-admin gets a **permission error** and never learns
/// whether the server is licensed. Only then does the group read run, and only then the hook.
///
/// # The server-controlled fields are stamped, and the caller's copies discarded
///
/// `ToPropertyField`, then id, group, object type (`user`), target (`system`, no id), `Protected`
/// and both `*By` are overwritten — so a caller cannot inject an id, a target or a protected flag.
/// What is **not** overwritten is `linked_field_id`, `type` and the three permission levels,
/// which ride through to the app layer and are refused or repinned there.
///
/// **201**, and the CPA event `custom_profile_attributes_field_created` carries the field as a
/// CPA field — typed attrs — where the generic `property_field_created` the app layer publishes
/// carries the raw one as a string.
#[tracing::instrument(skip_all, fields(field_id))]
pub async fn create_cpa_field(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let connection_id = connection_id(&request);
    let Some(body) = read_body(request).await else {
        return body_unreadable();
    };
    let Some(mut cpa_field) = decode_struct::<CPAField>(&body) else {
        return ApiError::invalid_param("property_field").into_response();
    };
    cpa_field.property_field.name = cpa_field.property_field.name.trim().to_owned();

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let group = match state.app.cpa_property_group().await {
        Ok(group) => group,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let mut field = cpa_field.to_property_field();
    field.id = String::new();
    field.group_id = group.id.clone();
    field.object_type = PROPERTY_FIELD_OBJECT_TYPE_USER.to_owned();
    field.target_type = PROPERTY_FIELD_TARGET_LEVEL_SYSTEM.to_owned();
    field.target_id = String::new();
    field.protected = false;
    field.created_by = session.0.user_id.clone();
    field.updated_by = session.0.user_id.clone();

    let caller = PropertyCaller::from_session(&session.0);
    let created = match state
        .app
        .cpa_create_field(&group, &caller, field, &connection_id)
        .await
    {
        Ok(created) => created,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("field_id", &created.id);

    let cpa_field = match CPAField::from_property_field(&created) {
        Ok(cpa_field) => cpa_field,
        Err(err) => return conversion_error("createCPAField", err).into_response(),
    };

    let mut message = WebSocketEvent::new(WEBSOCKET_EVENT_CPA_FIELD_CREATED, "", "", "", None, "");
    message.add("field", cpa_json(&cpa_field));
    state.app.publish(message).await;

    encoded_with_status(StatusCode::CREATED, &cpa_field, "createCPAField")
}

/// Port of `patchCPAField` (:130) — `PATCH /api/v4/custom_profile_attributes/fields/{field_id}`.
///
/// # The name is trimmed **before** it is validated
///
/// `*patch.Name = strings.TrimSpace(*patch.Name)` runs at :142 and `patch.IsValid()` at :149, so
/// a patch of `{"name":"   "}` is `model.property_field.is_valid.app_error` — "value cannot be
/// empty" — and not a successful rename to three spaces. Reversing the two would turn a 400 into
/// a write.
///
/// `TargetID` and `TargetType` are cleared before validation too, so a caller cannot patch them
/// and cannot fail validation on them either.
///
/// # The patch is applied with `mergeAttrs = true`
///
/// A key in `attrs` overwrites that one attr and a `null` deletes it; the rest of the blob is
/// kept. The attribute hook then re-sanitises the merged result, which is how a patch of
/// `{"attrs":{"visibility":""}}` lands as `when_set`.
///
/// `delete_values` on the CPA event is whether the type-change cleanup cleared this field's
/// values — the signal a pre-PSAv2 client uses to drop its cache.
#[tracing::instrument(skip_all, fields(field_id = %field_id, options_only))]
pub async fn patch_cpa_field(
    State(state): State<AppState>,
    Path(field_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `c.RequireFieldId()` (web/context.go) — first, before the body is even read.
    if !is_valid_id(&field_id) {
        return ApiError::invalid_url_param("field_id").into_response();
    }

    let connection_id = connection_id(&request);
    let Some(body) = read_body(request).await else {
        return body_unreadable();
    };
    let Some(mut patch) = decode_struct::<PropertyFieldPatch>(&body) else {
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

    let group = match state.app.cpa_property_group().await {
        Ok(group) => group,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let caller = PropertyCaller::from_session(&session.0);

    let mut existing = match state.app.cpa_get_field(&group, &caller, &field_id).await {
        Ok(existing) => existing,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if existing.object_type != PROPERTY_FIELD_OBJECT_TYPE_USER {
        return refusal(
            "patchCPAField",
            "api.property_field.object_type_mismatch.app_error",
            404,
        )
        .into_response();
    }

    let options_only = is_options_only_patch(&patch) && existing.type_.supports_options();
    tracing::Span::current().record("options_only", options_only);
    if options_only {
        if !state
            .app
            .session_has_permission_to_manage_property_field_options(&session.0, &existing)
            .await
        {
            return refusal(
                "patchCPAField",
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
            "patchCPAField",
            "api.property_field.update.no_field_permission.app_error",
            403,
        )
        .into_response();
    }

    existing.patch(&patch, true);
    existing.updated_by = session.0.user_id.clone();

    let update = match state
        .app
        .cpa_update_field(&group, &caller, existing, &connection_id)
        .await
    {
        Ok(update) => update,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let cpa_field = match CPAField::from_property_field(&update.field) {
        Ok(cpa_field) => cpa_field,
        Err(err) => return conversion_error("patchCPAField", err).into_response(),
    };

    let mut message = WebSocketEvent::new(WEBSOCKET_EVENT_CPA_FIELD_UPDATED, "", "", "", None, "");
    message.add("field", cpa_json(&cpa_field));
    message.add(
        "delete_values",
        serde_json::Value::Bool(!update.cleared_field_ids.is_empty()),
    );
    state.app.publish(message).await;

    encoded(&cpa_field, "patchCPAField")
}

/// Port of `deleteCPAField` (:236) — `DELETE /api/v4/custom_profile_attributes/fields/{field_id}`.
///
/// The same read as the patch and the same first two answers, with **no body** between the id
/// check and the group read — so a delete of an unknown id is a 404 where the patch of the same
/// id might still be a 400, because the patch reads a body first. Then the field-edit permission
/// (a **different** id from the patch's, `delete.no_permission`), the delete, and the CPA event
/// carrying only the id. The body is `ReturnStatusOK`'s, without a newline.
#[tracing::instrument(skip_all, fields(field_id = %field_id))]
pub async fn delete_cpa_field(
    State(state): State<AppState>,
    Path(field_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&field_id) {
        return ApiError::invalid_url_param("field_id").into_response();
    }
    let connection_id = connection_id(&request);

    let group = match state.app.cpa_property_group().await {
        Ok(group) => group,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let caller = PropertyCaller::from_session(&session.0);

    let existing = match state.app.cpa_get_field(&group, &caller, &field_id).await {
        Ok(existing) => existing,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if existing.object_type != PROPERTY_FIELD_OBJECT_TYPE_USER {
        return refusal(
            "deleteCPAField",
            "api.property_field.object_type_mismatch.app_error",
            404,
        )
        .into_response();
    }
    if !state
        .app
        .session_has_permission_to_edit_property_field(&session.0, &existing)
        .await
    {
        return refusal(
            "deleteCPAField",
            "api.property_field.delete.no_permission.app_error",
            403,
        )
        .into_response();
    }

    if let Err(err) = state
        .app
        .delete_property_field_with_hooks(&group, &caller, &field_id, &connection_id)
        .await
    {
        return ApiError::from(err).into_response();
    }

    let mut message = WebSocketEvent::new(WEBSOCKET_EVENT_CPA_FIELD_DELETED, "", "", "", None, "");
    message.add("field_id", serde_json::Value::String(field_id));
    state.app.publish(message).await;

    status_ok()
}

/// Port of `patchCPAValues` (:434) — `PATCH /api/v4/custom_profile_attributes/values`.
///
/// The target is **always the session's own user**, so `hasTargetAccess`'s self-access arm always
/// passes and no permission can refuse this route. Everything else it does is
/// [`cpa_patch_values`].
#[tracing::instrument(skip_all)]
pub async fn patch_cpa_values(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let connection_id = connection_id(&request);
    let Some(body) = read_body(request).await else {
        return body_unreadable();
    };
    let user_id = session.0.user_id.clone();
    cpa_patch_values(&state, &session, &user_id, &body, &connection_id).await
}

/// Port of `listCPAValues` (:371) —
/// `GET /api/v4/users/{user_id}/custom_profile_attributes`.
///
/// # The target check runs **before** the group read here, and after it on the PATCH routes
///
/// `listCPAValues` calls `hasTargetAccess` at :377 and `GetPropertyGroup` at :383;
/// `cpaPatchValues`, which both PATCH routes share, reads the group at :306 and checks the target
/// at :311. The asymmetry is only observable when the group is missing — a 404 versus a 403 — but
/// it is reproduced rather than tidied, because tidying it is how a port acquires a divergence
/// nobody chose.
///
/// # The body is `{}`, never `null`
///
/// `returnValue := make(map[string]json.RawMessage)` is always non-nil, so a user with no values
/// gets an empty object. A `BTreeMap` serialises the same way, and its ordering is Go's too:
/// `encoding/json` sorts map keys.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn list_cpa_values(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let user_id = resolve_me(&user_id, &session).to_owned();
    if !is_valid_id(&user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    match target_access_read(&state, &session, &user_id).await {
        TargetAccess::Denied(err) => return err.into_response(),
        TargetAccess::Allowed => {}
    }

    let group = match state.app.cpa_property_group().await {
        Ok(group) => group,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let caller = PropertyCaller::from_session(&session.0);
    match state.app.cpa_list_values(&group, &caller, &user_id).await {
        Ok(values) => encoded(&values, "listCPAValues"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `patchCPAValuesForUser` (:460) —
/// `PATCH /api/v4/users/{user_id}/custom_profile_attributes`.
///
/// The id check runs before the body is read, and the write-side target check —
/// `PermissionEditOtherUsers` for anyone but yourself — runs inside [`cpa_patch_values`], after
/// the group read.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn patch_cpa_values_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = resolve_me(&user_id, &session).to_owned();
    if !is_valid_id(&user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    let connection_id = connection_id(&request);
    let Some(body) = read_body(request).await else {
        return body_unreadable();
    };
    cpa_patch_values(&state, &session, &user_id, &body, &connection_id).await
}

/// Port of `cpaPatchValues` (:302), shared by both PATCH-values routes.
///
/// # The order of the refusals is the whole content
///
/// 1. The body must decode as a JSON **object** — `map[string]json.RawMessage`. An array or a
///    number is 400 `api.context.invalid_body_param.app_error` naming `value`; a literal `null`
///    is **not**, because Go decodes it into a nil map without error and falls through to (3).
/// 2. The group is read, then the target check — `PermissionEditOtherUsers` unless the target is
///    the caller.
/// 3. An empty batch is 400 `api.property_value.patch.empty_body.app_error`. `{}` and `null` both
///    land here.
/// 4. More than fifty entries is 400 `api.property_value.patch.too_many_items.request_error`,
///    **before** any id is checked — so an oversized batch of malformed ids reports the size.
/// 5. Every key must be a valid id: 400 `api.property_value.patch.invalid_field_id.app_error`.
/// 6. The fields are read by id, through the hooks: a short read is 404
///    `app.property_field.not_found.app_error`; unlicensed, a full read is the 403.
/// 7. Per field, in batch order: a non-`user` field is 404 `object_type_mismatch`, and a caller
///    without the field's value permission **on the target user** is 403
///    `no_values_permission`. The first refusal wins, so the batch's order decides which.
/// 8. `UpsertPropertyValues`, then the CPA event with the `{field_id: value}` map the response
///    also carries.
///
/// Go's duplicate-`FieldID` check from the generic handler is absent here and its comment says
/// why: the keys come from a JSON object, so uniqueness is already guaranteed. **Batch order is
/// map order** — Go ranges over the decoded map, whose iteration order is random, so the
/// per-field loop in (7) runs in an order Go itself does not fix; a batch with two refusable
/// fields is answered with either. Sorted key order here, which is one of Go's possible answers.
async fn cpa_patch_values(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: &str,
    body: &[u8],
    connection_id: &str,
) -> Response {
    let Some(updates) = decode_map(body) else {
        return ApiError::invalid_param("value").into_response();
    };

    let group = match state.app.cpa_property_group().await {
        Ok(group) => group,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if user_id != session.0.user_id
        && !session.0.is_unrestricted()
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_EDIT_OTHER_USERS)
            .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    if updates.is_empty() {
        return value_patch_error("api.property_value.patch.empty_body.app_error", None)
            .into_response();
    }
    if updates.len() > MAX_PROPERTY_VALUE_PATCH_ITEMS {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "Max".to_owned(),
            serde_json::Value::from(MAX_PROPERTY_VALUE_PATCH_ITEMS),
        );
        return value_patch_error(
            "api.property_value.patch.too_many_items.request_error",
            Some(params),
        )
        .into_response();
    }

    let mut field_ids: Vec<String> = Vec::with_capacity(updates.len());
    for field_id in updates.keys() {
        if !is_valid_id(field_id) {
            return value_patch_error("api.property_value.patch.invalid_field_id.app_error", None)
                .into_response();
        }
        field_ids.push(field_id.clone());
    }

    let caller = PropertyCaller::from_session(&session.0);
    let fields = match state.app.cpa_get_fields(&group, &caller, &field_ids).await {
        Ok(fields) => fields,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let by_id: std::collections::HashMap<&str, &mm_model::property_field::PropertyField> =
        fields.iter().map(|f| (f.id.as_str(), f)).collect();
    for field_id in &field_ids {
        let Some(field) = by_id.get(field_id.as_str()) else {
            let mut params = std::collections::HashMap::new();
            params.insert(
                "FieldID".to_owned(),
                serde_json::Value::String(field_id.clone()),
            );
            return ApiError::from(AppError::new(
                "cpaPatchValues",
                "api.property_value.patch.field_not_found.app_error",
                Some(params),
                String::new(),
                404,
            ))
            .into_response();
        };
        if field.object_type != PROPERTY_FIELD_OBJECT_TYPE_USER {
            return refusal(
                "cpaPatchValues",
                "api.property_field.object_type_mismatch.app_error",
                404,
            )
            .into_response();
        }
        if !state
            .app
            .session_has_permission_to_set_property_field_values(&session.0, field, user_id)
            .await
        {
            return refusal(
                "cpaPatchValues",
                "api.property_value.patch.no_values_permission.app_error",
                403,
            )
            .into_response();
        }
    }

    let caller_id = session.0.user_id.clone();
    let values: Vec<mm_model::property_value::PropertyValue> = field_ids
        .iter()
        .map(|field_id| mm_model::property_value::PropertyValue {
            target_id: user_id.to_owned(),
            target_type: PROPERTY_FIELD_OBJECT_TYPE_USER.to_owned(),
            group_id: group.id.clone(),
            field_id: field_id.clone(),
            value: updates
                .get(field_id)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            created_by: caller_id.clone(),
            updated_by: caller_id.clone(),
            ..Default::default()
        })
        .collect();

    let upserted = match state
        .app
        .cpa_upsert_values(&group, &caller, values, user_id, connection_id)
        .await
    {
        Ok(upserted) => upserted,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let results: std::collections::BTreeMap<String, serde_json::Value> = upserted
        .into_iter()
        .map(|value| (value.field_id, value.value))
        .collect();

    let mut message = WebSocketEvent::new(WEBSOCKET_EVENT_CPA_VALUES_UPDATED, "", "", "", None, "");
    message.add("user_id", serde_json::Value::String(user_id.to_owned()));
    message.add(
        "values",
        serde_json::to_value(&results).unwrap_or(serde_json::Value::Null),
    );
    state.app.publish(message).await;

    encoded(&results, "cpaPatchValues")
}

/// `isOptionsOnlyPatch` (api4/properties.go:928): nothing but `attrs`, and `attrs` holding
/// exactly the one key `options`.
pub(crate) fn is_options_only_patch(patch: &PropertyFieldPatch) -> bool {
    if patch.name.is_some()
        || patch.type_.is_some()
        || patch.target_id.is_some()
        || patch.target_type.is_some()
        || patch.linked_field_id.is_some()
    {
        return false;
    }
    let Some(attrs) = patch.attrs.as_ref() else {
        return false;
    };
    attrs.len() == 1 && attrs.contains_key(PROPERTY_FIELD_ATTRIBUTE_OPTIONS)
}

/// A CPA field as a websocket payload — `message.Add("field", cpaField)`, an object.
fn cpa_json(field: &CPAField) -> serde_json::Value {
    serde_json::to_value(field).unwrap_or(serde_json::Value::Null)
}

/// One of the refusals `api4/custom_profile_attributes.go` mints by hand, all of which carry an
/// empty `detailed_error` on the wire once `WipeDetailed` has run.
fn refusal(where_: &'static str, id: &'static str, status: i32) -> ApiError {
    ApiError::from(AppError::new(where_, id, None, String::new(), status))
}

/// `app.custom_profile_attributes.property_field_conversion.app_error`, the handler's 500 when a
/// written field will not convert back to a CPA field.
fn conversion_error(where_: &'static str, err: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %err, "a CPA field would not convert");
    ApiError::from(AppError::new(
        where_,
        "app.custom_profile_attributes.property_field_conversion.app_error",
        None,
        String::new(),
        500,
    ))
}

/// The three 400s `cpaPatchValues` raises itself, all with `Where` = `cpaPatchValues`.
fn value_patch_error(
    id: &'static str,
    params: Option<std::collections::HashMap<String, serde_json::Value>>,
) -> ApiError {
    ApiError::from(AppError::new(
        "cpaPatchValues",
        id,
        params,
        String::new(),
        400,
    ))
}

/// `hasTargetAccess(c, PropertyFieldObjectTypeUser, targetID, write=false)`
/// (api4/properties.go:872), the read arm.
///
/// Self-access and an unrestricted (local-mode) session pass without a query. Anyone else must be
/// able to *see* the target, which is [`mm_app::App::user_can_see_other_user`].
enum TargetAccess {
    Allowed,
    Denied(ApiError),
}

async fn target_access_read(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: &str,
) -> TargetAccess {
    if user_id == session.0.user_id || session.0.is_unrestricted() {
        return TargetAccess::Allowed;
    }

    match state
        .app
        .user_can_see_other_user(&session.0.user_id, user_id)
        .await
    {
        Ok(true) => TargetAccess::Allowed,
        Ok(false) => TargetAccess::Denied(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_MEMBERS],
        ))),
        Err(err) => TargetAccess::Denied(ApiError::from(err)),
    }
}

/// The request body, or `None` when it will not read — which [`body_unreadable`] answers, the
/// same plain-text 400 [`crate::proxy::forward_to_go`] gives for the same failure.
async fn read_body(request: Request) -> Option<axum::body::Bytes> {
    match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            None
        }
    }
}

fn body_unreadable() -> Response {
    (StatusCode::BAD_REQUEST, "could not read request body").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Decode` into a pointer accepts `null` and leaves the pointer nil; the two handlers that
    /// decode a pointer then answer `invalid_body_param`. The two that decode a **map** treat the
    /// same `null` as an empty map and fall through to the empty-batch error instead — a
    /// different id at the same status, from the same four bytes.
    #[test]
    fn a_null_body_is_nil_for_a_struct_and_empty_for_a_map() {
        assert!(decode_struct::<PropertyFieldPatch>(b"null").is_none());
        assert_eq!(decode_map(b"null"), Some(serde_json::Map::new()));
    }

    /// An empty body is `io.EOF` on Go's decoder, which is an error and not a zero value — so
    /// every route here answers its own `invalid_body_param` to a bodyless request rather than
    /// treating it as `{}`.
    #[test]
    fn an_empty_body_does_not_decode() {
        assert!(decode_struct::<PropertyFieldPatch>(b"").is_none());
        assert!(decode_map(b"").is_none());
        assert!(decode_map(b"  ").is_none());
    }

    /// **A JSON array is not a struct, and serde does not know that.** A struct deserialises from
    /// a sequence of its fields, so `[]` decodes to an all-default `PropertyFieldPatch` unless
    /// something refuses it first — which would let `PATCH …/fields/{id}` with a body of `[]`
    /// through to a *write* where Go answers 400. This is the test that caught it.
    #[test]
    fn an_array_is_not_an_object() {
        assert!(decode_struct::<PropertyFieldPatch>(b"[]").is_none());
        assert!(decode_struct::<PropertyFieldPatch>(br#"["x"]"#).is_none());
        assert!(decode_map(b"[]").is_none());
    }

    /// Nor is a scalar.
    #[test]
    fn a_scalar_is_not_an_object() {
        assert!(decode_struct::<PropertyFieldPatch>(b"5").is_none());
        assert!(decode_map(b"5").is_none());
        assert!(decode_map(br#""x""#).is_none());
    }

    /// An object whose keys are the right names still has to have the right **types**: a
    /// numeric `name` is a decode error in Go and must be one here.
    #[test]
    fn an_object_with_the_wrong_field_type_does_not_decode() {
        assert!(decode_struct::<PropertyFieldPatch>(br#"{"name":5}"#).is_none());
        assert!(decode_struct::<PropertyFieldPatch>(br#"{"name":"ok"}"#).is_some());
    }

    /// **Go's decoder stops at the first value and ignores the rest.** `serde_json::from_slice`
    /// would reject the trailing token, which would turn a body Go accepts into a 400 — so
    /// [`first_value`] uses the streaming form. This is the assertion that keeps it that way.
    #[test]
    fn trailing_content_is_ignored_the_way_go_ignores_it() {
        let decoded = decode_map(br#"{"a":1} {"b":2}"#).expect("the first value decodes");
        assert_eq!(decoded.len(), 1);
        assert!(decoded.contains_key("a"));
    }

    /// `isOptionsOnlyPatch`: `attrs` alone, holding exactly `options`. A second key, an empty
    /// `attrs`, or any other patched member makes it a field edit — which needs the other
    /// permission and answers the other id.
    #[test]
    fn an_options_only_patch_is_exactly_one_attr_and_nothing_else() {
        let only: PropertyFieldPatch = serde_json::from_str(r#"{"attrs":{"options":[]}}"#).unwrap();
        assert!(is_options_only_patch(&only));
        let two: PropertyFieldPatch =
            serde_json::from_str(r#"{"attrs":{"options":[],"visibility":"always"}}"#).unwrap();
        assert!(!is_options_only_patch(&two));
        let named: PropertyFieldPatch =
            serde_json::from_str(r#"{"name":"x","attrs":{"options":[]}}"#).unwrap();
        assert!(!is_options_only_patch(&named));
        let empty: PropertyFieldPatch = serde_json::from_str(r#"{"attrs":{}}"#).unwrap();
        assert!(!is_options_only_patch(&empty));
        let none: PropertyFieldPatch = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!is_options_only_patch(&none));
    }

    /// The cap is fifty, and it is a `>` and not a `>=`: exactly fifty entries is accepted.
    #[test]
    fn the_batch_cap_is_fifty_inclusive() {
        assert_eq!(MAX_PROPERTY_VALUE_PATCH_ITEMS, 50);
    }
}
