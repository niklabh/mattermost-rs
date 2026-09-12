//! Port of the four **read** routes of `api4/properties.go` — the generic PSAv2 property API:
//! `getPropertyFields`, `searchPropertyFields`, `getPropertyValues` and
//! `getSystemPropertyValues`. The five writes in the same file are not ported and fall through to
//! the proxy.
//!
//! ```text
//! GET  /api/v4/properties/groups/{group_name}/{object_type}/fields
//! POST /api/v4/properties/groups/{group_name}/fields/search
//! GET  /api/v4/properties/groups/{group_name}/{object_type}/values/{target_id}
//! GET  /api/v4/properties/groups/{group_name}/system/values
//! ```
//!
//! # These routes are registered, and the reason is one flag nobody would guess
//!
//! `InitProperties` (properties.go:23) is a five-way `if` over `FeatureFlags`:
//! `IntegratedBoards || ManagedChannelCategories || ClassificationMarkings || SessionAttributes ||
//! PostAttributes`. Four of the five default to **false** — and `ClassificationMarkings` defaults
//! to **true** (feature_flags.go:185), so on a stock server the family is live. A reader who
//! sampled any one of the other four would conclude the whole file was dark and forward nine
//! routes that answer. The `if` is reproduced by
//! [`mm_app::config::Config::properties_api_enabled`]; when it is closed these handlers forward,
//! so Go writes its own mux 404 exactly as [`crate::views`] does for `IntegratedBoards`.
//!
//! # The answer depends on which group is named, and there are five of them
//!
//! `getV2Group` (properties.go:42) is the gate every handler opens with, and it has three
//! refusals before any permission check: a group that does not exist is **404
//! `app.property_group.get.app_error`**, a group that is not version 2 is **404
//! `api.property.v2_group_not_found.app_error`**, and `session_attributes` without both its
//! feature flag and an Enterprise Advanced licence is **501
//! `api.property.session_attributes.license.app_error`**. All three measured against the stack's
//! Go server.
//!
//! Which groups exist is not in `api4/` at all — `RegisterBuiltinGroups` (app/server.go:279)
//! writes them unconditionally at startup. See [`mm_app::properties`] for the table of which of
//! them carries a licence hook; the short version is that `boards` and `post_attributes` carry
//! none, so their reads are ordinary reads on any edition and this is not a licence-shaped family
//! the way [`crate::custom_profile_attributes`] is.
//!
//! # Scope is mandatory, and the system shortcut is what makes that survivable
//!
//! `resolveScopeAndCheckPermissions` (properties.go:363) refuses a field search with no scope at
//! all — **400 `api.property_field.get.scope_required.app_error`** — and refuses one with both
//! shapes of scope at once. The exception is `object_types == ["system"]` exactly, which
//! `searchPropertyFieldsCore` (:310) collapses to `target_type=system` and *erases* any
//! channel/team/target filter the caller sent, so `GET …/system/fields` with a bare query string
//! is a 200 rather than the 400 every other object type gives. The collapse is guarded on the
//! list being exactly one element: mixing `system` with another type would silently drop the
//! non-system rows.
//!
//! # `per_page=0` is a 500, on both servers
//!
//! `web.ParamsFromRequest` lets a literal `0` through (params.go:234 clamps only negatives and
//! the maximum), the GET handler passes `c.Params.PerPage` straight into the opts, and the store
//! opens with `if opts.PerPage < 1 { return errors.New(…) }` — which the app layer turns into
//! **500 `app.property_field.search.app_error`**. Measured. The POST route clamps `<= 0` to
//! `PerPageDefault` itself (:285) and cannot reach it, which is the kind of asymmetry between two
//! endpoints on one store that a port is likely to smooth over.
//!
//! # An empty field list is `[]` and an empty value list is `null`
//!
//! The field store opens `fields := []*model.PropertyField{}` and the value store opens
//! `var values []*model.PropertyValue` (property_field_store.go:334, property_value_store.go:206).
//! Nothing downstream normalises either, so the two sibling endpoints disagree on the wire for
//! the empty case. Measured on both, because it is exactly the difference an idiomatic Rust port
//! erases by returning `Vec` from both.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_READ_CHANNEL, PERMISSION_VIEW_MEMBERS, PERMISSION_VIEW_TEAM, Permission,
    make_permission_error,
};
use mm_model::property_field::{
    PROPERTY_FIELD_OBJECT_TYPE_CHANNEL, PROPERTY_FIELD_OBJECT_TYPE_POST,
    PROPERTY_FIELD_OBJECT_TYPE_SYSTEM, PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE,
    PROPERTY_FIELD_OBJECT_TYPE_USER, PROPERTY_FIELD_TARGET_LEVEL_CHANNEL,
    PROPERTY_FIELD_TARGET_LEVEL_SYSTEM, PROPERTY_FIELD_TARGET_LEVEL_TEAM, PropertyFieldSearch,
    PropertyFieldSearchCursor, PropertyFieldSearchOpts, is_valid_property_field_object_type,
    is_valid_psav2_property_field_target_type,
};
use mm_model::property_group::{
    ACCESS_CONTROL_PROPERTY_GROUP_NAME, PropertyGroup, is_valid_property_group_name,
};
use mm_model::property_value::{
    PROPERTY_VALUE_SYSTEM_TARGET_ID, PropertyValueSearchCursor, PropertyValueSearchOpts,
};
use mm_model::session_attributes::SESSION_ATTRIBUTES_PROPERTY_GROUP_NAME;
use mm_model::utils::{AppError, decode_one_from_json, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{PER_PAGE_DEFAULT, PER_PAGE_MAXIMUM, parse_per_page, query_first};
use crate::error::ApiError;
use crate::proxy;

/// `model.ConnectionId` (model/websocket_client.go) — the header the write routes read so a client
/// can be left out of the broadcast for its own change. Spelled here as it is in
/// [`crate::views`] and [`crate::drafts`]; it is one constant in Go and three in this crate, which
/// is a duplication worth collapsing the next time a fourth appears.
const CONNECTION_ID_HEADER: &str = "Connection-Id";

/// `json.NewEncoder(w).Encode(v)` — a JSON body **with** the encoder's trailing newline ([D-086]).
fn encoded(value: &impl serde::Serialize, where_: &'static str) -> Response {
    let mut body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise a properties response");
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
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

/// `c.SetPermissionError(perm)` (web/context.go) — 403 `api.context.permissions.app_error`.
fn permission_error(session: &mm_model::session::Session, permission: &Permission) -> ApiError {
    ApiError::from(make_permission_error(session, &[permission]))
}

/// One of the refusals `api4/properties.go` mints by hand, all of which carry an empty
/// `detailed_error` on the wire once `WipeDetailed` has run.
fn refusal(where_: &'static str, id: &'static str, status: i32) -> ApiError {
    ApiError::from(AppError::new(where_, id, None, String::new(), status))
}

/// What [`v2_group`] decided.
enum Group {
    /// The group resolved and nothing unported stands between the request and the store.
    Serve(Box<PropertyGroup>),
    /// Hand the request to Go: either the family is not registered there at all, or this is a
    /// licensed read of the one group whose hook chain is not ported.
    Forward,
    Failed(ApiError),
}

/// Port of `getV2Group` (properties.go:42), plus the two decisions Go does not have to make
/// because it is the whole server: is the family registered, and is the hook chain reachable.
///
/// Go's order is load, then the v2 check, then the session-attributes gate, and it matters: a
/// group that does not exist is `app.property_group.get.app_error` and never
/// `v2_group_not_found`, even when the name asked for is `session_attributes`.
///
/// # The forward arm is narrower than it looks
///
/// Only `access_control` carries hooks on the read path, so only `access_control` has to be
/// forwarded when a licence is present — every other group reads the same on any edition. The
/// licence is not consulted at all for the others, which is why this is not a
/// [`crate::custom_profile_attributes`]-style gate that forwards the whole family.
async fn v2_group(state: &AppState, group_name: &str, where_: &'static str) -> Group {
    let group = match state.app.property_group(group_name).await {
        Ok(group) => group,
        Err(err) => return Group::Failed(ApiError::from(err)),
    };

    if !group.is_psav2() {
        return Group::Failed(refusal(
            where_,
            "api.property.v2_group_not_found.app_error",
            404,
        ));
    }

    // `group.Name == SessionAttributesPropertyGroupName && (!flag || !MinimumEnterpriseAdvanced)`
    // — the flag is false at the pinned SHA and this deployment is unlicensed, so both halves are
    // shut and the route is a 501. Go's comment says this mirrors the dedicated manifest
    // endpoint's gate so the generic API cannot expose the schema when the feature is off.
    if group.name == SESSION_ATTRIBUTES_PROPERTY_GROUP_NAME {
        let enabled = state.app.config().feature_flag_session_attributes;
        tracing::Span::current().record("session_attributes", enabled);
        if !enabled {
            return Group::Failed(refusal(
                where_,
                "api.property.session_attributes.license.app_error",
                501,
            ));
        }
        // The flag is on, so the answer now turns on `MinimumEnterpriseAdvancedLicense`, which
        // this server can only ever establish as false — and if it were true the group would be
        // readable and every write hook would be in play. Let Go decide.
        return Group::Forward;
    }

    if group.name == ACCESS_CONTROL_PROPERTY_GROUP_NAME {
        match state.app.license_state().await {
            Ok(mm_app::license::LicenseState::Unlicensed) => {
                tracing::Span::current().record("licensed", false);
            }
            Ok(mm_app::license::LicenseState::Licensed) => {
                tracing::Span::current().record("licensed", true);
                return Group::Forward;
            }
            Err(err) => return Group::Failed(ApiError::from(err)),
        }
    }

    Group::Serve(Box::new(group))
}

/// Port of `getPropertyFields` (properties.go:177) —
/// `GET /api/v4/properties/groups/{group_name}/{object_type}/fields`.
///
/// The query string is parsed into the same `PropertyFieldSearchOpts` the POST route builds from
/// a body, and every parse failure is a **400 `api.context.invalid_body_param.app_error`** naming
/// the query parameter — `since`, `cursor_update_at`, `cursor_create_at` or `cursor` — despite
/// nothing being in the body. Go's `SetInvalidParamWithErr` does not know where the value came
/// from.
///
/// `cur.IsValid()` runs here and **not** on the values route (:634), so a malformed cursor is
/// `invalid_body_param` naming `cursor` on this route and
/// `api.property_value.get.invalid_opts.app_error` on that one. Same cursor, same mistake, two
/// ids.
#[tracing::instrument(skip_all, fields(group = %group_name, object_type = %object_type, licensed, session_attributes))]
pub async fn get_property_fields(
    State(state): State<AppState>,
    Path((group_name, object_type)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().properties_api_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }

    // `c.RequireGroupName().RequireObjectType()`, in that order — a request with both wrong
    // reports `group_name`.
    if !is_valid_property_group_name(&group_name) {
        return ApiError::invalid_url_param("group_name").into_response();
    }
    if !is_valid_property_field_object_type(&object_type) {
        return ApiError::invalid_url_param("object_type").into_response();
    }

    let group = match v2_group(&state, &group_name, "getPropertyFields").await {
        Group::Serve(group) => group,
        Group::Forward => return proxy::forward_to_go(State(state), request).await,
        Group::Failed(err) => return err.into_response(),
    };

    let query = request.uri().query();
    let mut opts = PropertyFieldSearchOpts {
        group_id: group.id.clone(),
        object_types: vec![object_type],
        per_page: parse_per_page(query),
        ..PropertyFieldSearchOpts::default()
    };

    match query_int(query, "since") {
        Ok(Some(since)) => opts.since_update_at = since,
        Ok(None) => {}
        Err(err) => return err.into_response(),
    }

    // The cursor is read only when `cursor_id` is present: Go builds the struct inside
    // `if cursorID := query.Get("cursor_id"); cursorID != ""`, so a `cursor_create_at` on its own
    // is silently ignored rather than validated.
    if let Some(cursor_id) = query_first(query, "cursor_id").filter(|v| !v.is_empty()) {
        let mut cursor = PropertyFieldSearchCursor {
            property_field_id: cursor_id,
            ..PropertyFieldSearchCursor::default()
        };
        match query_int(query, "cursor_update_at") {
            Ok(Some(at)) => cursor.update_at = at,
            Ok(None) => {}
            Err(err) => return err.into_response(),
        }
        match query_int(query, "cursor_create_at") {
            Ok(Some(at)) => cursor.create_at = at,
            Ok(None) => {}
            Err(err) => return err.into_response(),
        }
        if cursor.is_valid().is_err() {
            return ApiError::invalid_param("cursor").into_response();
        }
        opts.cursor = cursor;
    }

    opts.channel_id = query_first(query, "channel_id").unwrap_or_default();
    opts.team_id = query_first(query, "team_id").unwrap_or_default();
    opts.target_type = query_first(query, "target_type").unwrap_or_default();
    if let Some(target_id) = query_first(query, "target_id").filter(|v| !v.is_empty()) {
        opts.target_ids = vec![target_id];
    }

    search_fields_core(&state, &session, &group, opts, "getPropertyFields").await
}

/// Port of `searchPropertyFields` (properties.go:240) —
/// `POST /api/v4/properties/groups/{group_name}/fields/search`.
///
/// Unlike the GET route this one validates `object_types` itself — an empty list or any invalid
/// entry is **400 `api.context.invalid_body_param.app_error`** naming `object_types` — and clamps
/// `per_page` into `[1, 200]` before the store can complain.
#[tracing::instrument(skip_all, fields(group = %group_name, licensed, session_attributes))]
pub async fn search_property_fields(
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

    // The group read happens **before** the body is decoded (:245 vs :250), so a request naming a
    // missing group with an unparseable body is a 404 and not a 400.
    let group = match v2_group(&state, &group_name, "searchPropertyFields").await {
        Group::Serve(group) => group,
        Group::Forward => return proxy::forward_to_go(State(state), request).await,
        Group::Failed(err) => return err.into_response(),
    };

    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(error = %err, "could not read the property field search body");
            return ApiError::invalid_param("property_field_search").into_response();
        }
    };
    let Ok(search) = decode_one_from_json::<PropertyFieldSearch>(&body) else {
        return ApiError::invalid_param("property_field_search").into_response();
    };

    let object_types = search.object_types.unwrap_or_default();
    if object_types.is_empty()
        || !object_types
            .iter()
            .all(|ot| is_valid_property_field_object_type(ot))
    {
        return ApiError::invalid_param("object_types").into_response();
    }

    let mut opts = PropertyFieldSearchOpts {
        group_id: group.id.clone(),
        object_types,
        target_type: search.target_type,
        channel_id: search.channel_id,
        team_id: search.team_id,
        since_update_at: search.since_update_at,
        per_page: search.per_page,
        ..PropertyFieldSearchOpts::default()
    };
    if !search.target_id.is_empty() {
        opts.target_ids = vec![search.target_id];
    }
    // No `IsValid` on the cursor here — the whole cursor is copied across unchecked and
    // `opts.IsValid()` inside the core is what rejects it, with a different id from the GET route.
    if !search.cursor_id.is_empty() {
        opts.cursor = PropertyFieldSearchCursor {
            property_field_id: search.cursor_id,
            create_at: search.cursor_create_at,
            update_at: search.cursor_update_at,
        };
    }
    opts.per_page = match opts.per_page {
        p if p <= 0 => PER_PAGE_DEFAULT,
        p if p > PER_PAGE_MAXIMUM => PER_PAGE_MAXIMUM,
        p => p,
    };

    search_fields_core(&state, &session, &group, opts, "searchPropertyFields").await
}

/// Port of `searchPropertyFieldsCore` (properties.go:294) — everything the two field endpoints
/// share once `opts` is populated.
async fn search_fields_core(
    state: &AppState,
    session: &AuthenticatedSession,
    group: &PropertyGroup,
    mut opts: PropertyFieldSearchOpts,
    where_: &'static str,
) -> Response {
    // The system shortcut (:310). Exactly one object type, and it is `system`: any channel/team/
    // target filter is a semantic no-op because a system-object field can only live at the system
    // scope, so rather than answering `scope_conflict` to a legacy caller that sent one, Go erases
    // it. `== 1` is load-bearing — mixing `system` with another type would drop the other's rows.
    if opts.object_types.len() == 1 && opts.object_types[0] == PROPERTY_FIELD_OBJECT_TYPE_SYSTEM {
        opts.channel_id = String::new();
        opts.team_id = String::new();
        opts.target_ids = Vec::new();
        opts.target_type = PROPERTY_FIELD_TARGET_LEVEL_SYSTEM.to_owned();
    }

    if let Err(err) = resolve_scope_and_check_permissions(state, session, &mut opts, where_).await {
        return err.into_response();
    }

    if opts.is_valid().is_err() {
        return refusal(where_, "api.property_field.get.invalid_opts.app_error", 400)
            .into_response();
    }

    match state.app.search_property_fields(group, &opts).await {
        Ok(fields) => encoded(&fields, where_),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `resolveScopeAndCheckPermissions` (properties.go:363).
///
/// Two scope shapes, mutually exclusive, and **one of them is required**: a search with neither is
/// `scope_required`, a search with both is `scope_conflict`. The hierarchical shape also *rewrites*
/// `opts` — a `channel_id` is resolved to its channel so the team can be filled in, which is how
/// "this channel and every ancestor" is expressed to the store.
async fn resolve_scope_and_check_permissions(
    state: &AppState,
    session: &AuthenticatedSession,
    opts: &mut PropertyFieldSearchOpts,
    where_: &'static str,
) -> Result<(), ApiError> {
    let scope_by_chan_team = !opts.channel_id.is_empty() || !opts.team_id.is_empty();
    let scope_by_target = !opts.target_type.is_empty() || !opts.target_ids.is_empty();
    if scope_by_chan_team && scope_by_target {
        return Err(refusal(
            where_,
            "api.property_field.get.scope_conflict.app_error",
            400,
        ));
    }

    if !opts.channel_id.is_empty() {
        let (allowed, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &opts.channel_id,
                &PERMISSION_READ_CHANNEL,
            )
            .await;
        if !allowed {
            return Err(permission_error(&session.0, &PERMISSION_READ_CHANNEL));
        }
        // The permission check runs **before** the channel is fetched, so a channel that does not
        // exist is a 403 and not a 404 — the checker denies an unknown id.
        let channel = state
            .app
            .get_channel(&opts.channel_id)
            .await
            .map_err(ApiError::from)?;
        opts.channel_id = channel.id;
        opts.team_id = channel.team_id;
        return Ok(());
    }

    if !opts.team_id.is_empty() {
        if !state
            .app
            .session_has_permission_to_team(&session.0, &opts.team_id, &PERMISSION_VIEW_TEAM)
            .await
        {
            return Err(permission_error(&session.0, &PERMISSION_VIEW_TEAM));
        }
        return Ok(());
    }

    if !opts.target_type.is_empty() {
        if !is_valid_psav2_property_field_target_type(&opts.target_type) {
            return Err(refusal(
                where_,
                "api.property_field.get.invalid_target_type.app_error",
                400,
            ));
        }
        match opts.target_type.as_str() {
            PROPERTY_FIELD_TARGET_LEVEL_CHANNEL => {
                let Some(target) = opts.target_ids.first() else {
                    return Err(refusal(
                        where_,
                        "api.property_field.get.target_id_required.app_error",
                        400,
                    ));
                };
                let (allowed, _) = state
                    .app
                    .session_has_permission_to_channel(&session.0, target, &PERMISSION_READ_CHANNEL)
                    .await;
                if !allowed {
                    return Err(permission_error(&session.0, &PERMISSION_READ_CHANNEL));
                }
            }
            PROPERTY_FIELD_TARGET_LEVEL_TEAM => {
                let Some(target) = opts.target_ids.first() else {
                    return Err(refusal(
                        where_,
                        "api.property_field.get.target_id_required.app_error",
                        400,
                    ));
                };
                if !state
                    .app
                    .session_has_permission_to_team(&session.0, target, &PERMISSION_VIEW_TEAM)
                    .await
                {
                    return Err(permission_error(&session.0, &PERMISSION_VIEW_TEAM));
                }
            }
            // `system`: visible to every authenticated user, with no check at all.
            _ => {}
        }
        return Ok(());
    }

    if !opts.target_ids.is_empty() {
        return Err(refusal(
            where_,
            "api.property_field.get.target_type_required.app_error",
            400,
        ));
    }

    Err(refusal(
        where_,
        "api.property_field.get.scope_required.app_error",
        400,
    ))
}

/// Port of `deletePropertyField` (properties.go:544) —
/// `DELETE /api/v4/properties/groups/{group_name}/{object_type}/fields/{field_id}`.
///
/// The first of the five writes in this file to be ported, and it is the one whose whole path is
/// reachable: on `boards` and `post_attributes` the only pre-hook a delete runs is the licence
/// check, which does not manage those groups, so there is nothing unported between the handler
/// and the `UPDATE … SET DeleteAt`. See [`mm_app::properties`] for the group table.
///
/// # Four refusals, in Go's order, and three of them are 404s that mean different things
///
/// 1. `RequireGroupName().RequireObjectType().RequireFieldId()` — 400 `invalid_url_param`.
/// 2. `getV2Group` — a missing group is `app.property_group.get.app_error`, a non-v2 group is
///    `api.property.v2_group_not_found.app_error`. Both 404, different ids.
/// 3. `GetPropertyField` — a field that is not in this group is 404
///    `app.property.not_found.app_error`.
/// 4. **The object type in the URL must match the field's**, and a mismatch is a *third* 404,
///    `api.property_field.object_type_mismatch.app_error`. Go's comment says why it is a 404 and
///    not a 400: it lets fields be bucketed by URL without leaking cross-bucket existence.
///
/// Only then the permission check, which is **403 with its own id** —
/// `api.property_field.delete.no_permission.app_error`, not the generic
/// `api.context.permissions.app_error` every other route in this file answers with.
///
/// # The success body has no trailing newline
///
/// `ReturnStatusOK` is a bare `w.Write` of `{"status":"OK"}`, not an encoder — the other side of
/// [D-086] from every other route here.
#[tracing::instrument(skip_all, fields(group = %group_name, object_type = %object_type, field_id = %field_id, licensed, session_attributes))]
pub async fn delete_property_field(
    State(state): State<AppState>,
    Path((group_name, object_type, field_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
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

    let connection_id = request
        .headers()
        .get(CONNECTION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    let group = match v2_group(&state, &group_name, "deletePropertyField").await {
        Group::Serve(group) => group,
        Group::Forward => return proxy::forward_to_go(State(state), request).await,
        Group::Failed(err) => return err.into_response(),
    };

    let field = match state.app.get_property_field(&group.id, &field_id).await {
        Ok(field) => field,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if field.object_type != object_type {
        return refusal(
            "deletePropertyField",
            "api.property_field.object_type_mismatch.app_error",
            404,
        )
        .into_response();
    }

    if !state
        .app
        .session_has_permission_to_edit_property_field(&session.0, &field)
        .await
    {
        return refusal(
            "deletePropertyField",
            "api.property_field.delete.no_permission.app_error",
            403,
        )
        .into_response();
    }

    match state
        .app
        .delete_property_field(&group, &field, &connection_id)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `web.ReturnStatusOK` (web/handlers.go) — `{"status":"OK"}` with **no** trailing newline.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        br#"{"status":"OK"}"#.as_slice(),
    )
        .into_response()
}

/// Port of `getPropertyValues` (properties.go:582) —
/// `GET /api/v4/properties/groups/{group_name}/{object_type}/values/{target_id}`.
///
/// Three refusals fire **before** `RequireTargetId` and before the group is read, so they are the
/// answer even for a group that does not exist: `template` is `template_no_values`, `system` is
/// `system_use_dedicated_route`, and only then is the target id validated.
#[tracing::instrument(skip_all, fields(group = %group_name, object_type = %object_type, licensed, session_attributes))]
pub async fn get_property_values(
    State(state): State<AppState>,
    Path((group_name, object_type, target_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
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
            "getPropertyValues",
            "api.property_value.template_no_values.app_error",
            400,
        )
        .into_response();
    }
    if object_type == PROPERTY_FIELD_OBJECT_TYPE_SYSTEM {
        return refusal(
            "getPropertyValues",
            "api.property_value.system_use_dedicated_route.app_error",
            400,
        )
        .into_response();
    }
    if !is_valid_id(&target_id) {
        return ApiError::invalid_url_param("target_id").into_response();
    }

    values_core(
        state,
        session,
        &group_name,
        &object_type,
        &target_id,
        request,
    )
    .await
}

/// Port of `getSystemPropertyValues` (properties.go:607) —
/// `GET /api/v4/properties/groups/{group_name}/system/values`.
///
/// The dedicated route the object-typed one refuses in favour of. It supplies `system` for both
/// the object type and the target id, so `PropertyValueSystemTargetID` — the string `"system"` —
/// is what the store filters `TargetID` on.
#[tracing::instrument(skip_all, fields(group = %group_name, licensed, session_attributes))]
pub async fn get_system_property_values(
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

    values_core(
        state,
        session,
        &group_name,
        PROPERTY_FIELD_OBJECT_TYPE_SYSTEM,
        PROPERTY_VALUE_SYSTEM_TARGET_ID,
        request,
    )
    .await
}

/// Port of `getPropertyValuesCore` (properties.go:616).
///
/// The group read comes first, then the target access check, then the query string — so a caller
/// with no access to the target still learns whether the group exists, and a malformed `since`
/// is reported only to a caller who could have read the values.
async fn values_core(
    state: AppState,
    session: AuthenticatedSession,
    group_name: &str,
    object_type: &str,
    target_id: &str,
    request: Request,
) -> Response {
    let group = match v2_group(&state, group_name, "getPropertyValues").await {
        Group::Serve(group) => group,
        Group::Forward => return proxy::forward_to_go(State(state), request).await,
        Group::Failed(err) => return err.into_response(),
    };

    match has_target_access(&state, &session, object_type, target_id).await {
        TargetAccess::Allowed => {}
        TargetAccess::Denied(err) => return err.into_response(),
        TargetAccess::Forward => return proxy::forward_to_go(State(state), request).await,
    }

    let query = request.uri().query();
    let mut opts = PropertyValueSearchOpts {
        group_id: group.id.clone(),
        target_type: object_type.to_owned(),
        target_ids: vec![target_id.to_owned()],
        per_page: parse_per_page(query),
        ..PropertyValueSearchOpts::default()
    };

    match query_int(query, "since") {
        Ok(Some(since)) => opts.since_update_at = since,
        Ok(None) => {}
        Err(err) => return err.into_response(),
    }

    if let Some(cursor_id) = query_first(query, "cursor_id").filter(|v| !v.is_empty()) {
        let mut cursor = PropertyValueSearchCursor {
            property_value_id: cursor_id,
            ..PropertyValueSearchCursor::default()
        };
        match query_int(query, "cursor_update_at") {
            Ok(Some(at)) => cursor.update_at = at,
            Ok(None) => {}
            Err(err) => return err.into_response(),
        }
        match query_int(query, "cursor_create_at") {
            Ok(Some(at)) => cursor.create_at = at,
            Ok(None) => {}
            Err(err) => return err.into_response(),
        }
        // No `cur.IsValid()` on this route — see [`get_property_fields`].
        opts.cursor = cursor;
    }

    if opts.is_valid().is_err() {
        return refusal(
            "getPropertyValues",
            "api.property_value.get.invalid_opts.app_error",
            400,
        )
        .into_response();
    }

    match state.app.search_property_values(&group, &opts).await {
        // **`null`, not `[]`, for the empty case** — the Go store leaves its slice nil. See the
        // module docs.
        Ok(values) if values.is_empty() => encoded(&serde_json::Value::Null, "getPropertyValues"),
        Ok(values) => encoded(&values, "getPropertyValues"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `hasTargetAccess` (properties.go:812) for `write = false`, which is the only arm the
/// four read routes reach.
///
/// The `user` arm is the one with a shape of its own: self-access and an unrestricted (local-mode)
/// session pass with no lookup at all, and only another user's values need
/// `UserCanSeeOtherUser` — whose denial is reported as `view_members`, a permission the check
/// never actually consulted.
async fn has_target_access(
    state: &AppState,
    session: &AuthenticatedSession,
    object_type: &str,
    target_id: &str,
) -> TargetAccess {
    match object_type {
        PROPERTY_FIELD_OBJECT_TYPE_CHANNEL => {
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(&session.0, target_id, &PERMISSION_READ_CHANNEL)
                .await;
            if !allowed {
                return TargetAccess::Denied(permission_error(
                    &session.0,
                    &PERMISSION_READ_CHANNEL,
                ));
            }
        }
        PROPERTY_FIELD_OBJECT_TYPE_POST => {
            let post = match state.app.get_single_post(target_id, false).await {
                Ok(post) => post,
                Err(err) => return TargetAccess::Denied(ApiError::from(err)),
            };
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(
                    &session.0,
                    &post.channel_id,
                    &PERMISSION_READ_CHANNEL,
                )
                .await;
            if !allowed {
                return TargetAccess::Denied(permission_error(
                    &session.0,
                    &PERMISSION_READ_CHANNEL,
                ));
            }
        }
        PROPERTY_FIELD_OBJECT_TYPE_USER => {
            if target_id == session.0.user_id || session.0.is_unrestricted() {
                return TargetAccess::Allowed;
            }
            match state
                .app
                .user_can_see_other_user(&session.0.user_id, target_id)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return TargetAccess::Denied(permission_error(
                        &session.0,
                        &PERMISSION_VIEW_MEMBERS,
                    ));
                }
                // The caller's account carries view restrictions, which needs team and channel
                // membership lookups this port does not have. Forward rather than guess; see
                // [`mm_app::App::user_can_see_other_user`].
                Err(mm_app::post::PrepareError::Unreproducible(reason)) => {
                    tracing::debug!(reason, "forwarding to Go");
                    return TargetAccess::Forward;
                }
                Err(mm_app::post::PrepareError::App(err)) => {
                    return TargetAccess::Denied(ApiError::from(err));
                }
            }
        }
        // Any authenticated user can read system-scoped values; only a write needs
        // `PermissionManageSystem`, and no read route asks for one.
        PROPERTY_FIELD_OBJECT_TYPE_SYSTEM => {}
        PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE => {
            return TargetAccess::Denied(refusal(
                "hasTargetAccess",
                "api.property_value.template_no_values.app_error",
                400,
            ));
        }
        _ => {
            return TargetAccess::Denied(refusal(
                "hasTargetAccess",
                "api.property_value.invalid_object_type.app_error",
                400,
            ));
        }
    }
    TargetAccess::Allowed
}

/// What [`has_target_access`] decided.
enum TargetAccess {
    Allowed,
    Denied(ApiError),
    Forward,
}

/// `strconv.ParseInt(query.Get(key), 10, 64)` guarded by `if s != ""`.
///
/// An absent **or empty** parameter is `None` and never an error; anything else that will not
/// parse is `SetInvalidParamWithErr(key)`. `?since=` is therefore not a 400 on either server.
fn query_int(query: Option<&str>, key: &'static str) -> Result<Option<i64>, ApiError> {
    let Some(raw) = query_first(query, key).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    raw.parse::<i64>()
        .map(Some)
        .map_err(|_| ApiError::invalid_param(key))
}
