//! Port of the three channel-creation handlers in `channels/api4/channel.go`: `createChannel`
//! (:125), `createDirectChannel` (:625) and `createGroupChannel` (:758).
//!
//! Kept out of [`crate::channel_writes`], which owns the five lifecycle routes: nothing is
//! shared between a create and an update but the `Channel` type, and the two files' forwarding
//! rules differ.
//!
//! # All three answer **201**, including when nothing was created
//!
//! `w.WriteHeader(http.StatusCreated)` is unconditional on every one of them. `POST
//! /channels/direct` and `/channels/group` are idempotent — a second call returns the existing
//! channel — and they still answer 201, not 200. `POST /channels` is the opposite: a second call
//! with the same name is a **400** (`store.sql_channel.save_channel.exists.app_error`), and an
//! **archived** channel still holds its name, so a client that archives and re-creates hits that
//! 400 rather than getting a fresh channel.
//!
//! # The two id-list routes parse their bodies differently, and it is visible
//!
//! `createDirectChannel` uses `NonSortedArrayFromJSON` and `createGroupChannel` uses
//! `SortedArrayFromJSON`. Both de-duplicate; only the second sorts. The DM route then reads its
//! list **positionally** — `userIds[0]` becomes the `direct_added` event's `creator_id` — so the
//! order the client sent survives onto the wire. Sorting there would relabel the event.
//!
//! # A licensed installation is forwarded, but only on `POST /channels`
//!
//! `createChannel` reads `PrivacySettings.UseAnonymousURLs` behind
//! `MinimumEnterpriseAdvancedLicense` and would then **replace the client's channel name with a
//! fresh id**; `CreateChannel` reads `MinimumEnterpriseLicense` again for managed categories.
//! Both are decided on an unlicensed installation and both are reproduced, and a licensed one
//! hands the whole request over. Neither message route consults the licence at all, so neither
//! is gated.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::channel_create::ChannelCreate;
use mm_model::channel::{CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE, Channel};
use mm_model::permission::{
    PERMISSION_CREATE_DIRECT_CHANNEL, PERMISSION_CREATE_GROUP_CHANNEL,
    PERMISSION_CREATE_PRIVATE_CHANNEL, PERMISSION_CREATE_PUBLIC_CHANNEL,
    PERMISSION_MANAGE_PRIVATE_CHANNEL_DISCOVERABILITY, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_VIEW_MEMBERS, make_permission_error,
};
use mm_model::session::Session;
use mm_model::utils::{
    AppError, decode_one_from_json, is_valid_id, non_sorted_array_from_json, sorted_array_from_json,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// `FeatureFlags.DiscoverableChannels` (feature_flags.go:208) — **false** at the pinned SHA, and
/// absent from every configuration document Go writes because it clears `FeatureFlags` before
/// persisting. The same constant [`crate::channel_writes`] declares, and for the same reason;
/// they are separate because neither file may depend on the other's private items.
const FEATURE_FLAG_DISCOVERABLE_CHANNELS: bool = false;

/// `model.PayloadParseError` (utils.go:42).
const PAYLOAD_PARSE_ERROR: &str = "api.payload.parse.error";

/// `w.WriteHeader(http.StatusCreated)` then `json.NewEncoder(w).Encode(channel)` — a 201 and a
/// **trailing newline** ([D-086]).
fn created(handler: &'static str, channel: &Channel) -> Result<Response, ApiError> {
    let mut body = serde_json::to_vec(channel).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise Channel");
        ApiError::from(AppError::new(
            handler,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');
    Ok((
        StatusCode::CREATED,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Whether the installation carries a licence.
///
/// Not [`crate::channels::licence_gate`]: that helper forwards for you and therefore consumes the
/// request, and `create_channel` still owns a second forwarding branch below it.
async fn licensed(state: &AppState) -> Result<bool, ApiError> {
    let state_of_licence = state.app.license_state().await?;
    let licensed = state_of_licence == mm_app::license::LicenseState::Licensed;
    tracing::Span::current().record("licensed", licensed);
    Ok(licensed)
}

/// Read the whole body, keeping the parts so the request can still be forwarded.
async fn split_body(request: Request, parameter: &str) -> Result<(Request, Vec<u8>), ApiError> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            ApiError::invalid_param(parameter)
        })?;
    let rebuilt = Request::from_parts(parts, axum::body::Body::from(bytes.clone()));
    Ok((rebuilt, bytes.to_vec()))
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/channels
// ---------------------------------------------------------------------------------------------

/// Port of `createChannel` (api4/channel.go:125).
///
/// # The handler's order, and why two of the steps look redundant
///
/// 1. decode the body — 400 naming **`channel`**. A body of `null` decodes into a nil pointer on
///    Go's side and is caught by the same test, so it is a 400 too.
/// 2. `IsBoard()` → 400 naming **`type`**; `IsSpace()` → the same. Both are refused *again* in
///    `CreateChannelWithUser` with different ids, which is dead code from this route and live
///    from the import path.
/// 3. the licence read, which is where a licensed installation hands over.
/// 4. `TeamId == ""` → 400 naming **`team_id`**; `DisplayName == ""` → 400 naming
///    **`display_name`**. The app layer repeats only the first of these.
/// 5. the type-specific create permission, on the **team**. Note there is no `default:` arm: a
///    body with `type: "D"` passes every permission check here and is refused by the app layer's
///    `IsGroupOrDirect` with `api.channel.create_channel.direct_channel.app_error`.
/// 6. the three discoverability checks, in order: feature flag, type, permission.
///
/// Step 4 comes after step 3, so on a licensed installation the whole thing is Go's problem; on
/// an unlicensed one an empty `team_id` is a 400 before any permission is consulted, which means
/// **a user with no permissions still gets the 400 and not a 403**.
#[tracing::instrument(skip_all, fields(licensed, forwarded = false, channel_type))]
pub async fn create_channel(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (request, bytes) = match split_body(request, "channel").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };

    // `decode_one_from_json` rather than `from_slice`: `json.Decoder.Decode` reads one value and
    // stops, so `{"…"} garbage` is a success on Go taking the first. See `channel_writes`.
    let mut channel: Channel = match decode_one_from_json::<Option<Channel>>(&bytes) {
        Ok(Some(channel)) => channel,
        Ok(None) => return ApiError::invalid_param("channel").into_response(),
        Err(err) => {
            tracing::debug!(error = %err, "channel body did not decode");
            return ApiError::invalid_param("channel").into_response();
        }
    };
    tracing::Span::current().record("channel_type", channel.channel_type.as_str());

    if channel.is_board() || channel.is_space() {
        return ApiError::invalid_param("type").into_response();
    }

    match licensed(&state).await {
        Ok(true) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(false) => {}
        Err(err) => return err.into_response(),
    }

    match serve_create_channel(&state, &session.0, &mut channel).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_create_channel(
    state: &AppState,
    session: &Session,
    channel: &mut Channel,
) -> Result<Response, ApiError> {
    if channel.team_id.is_empty() {
        return Err(ApiError::invalid_param("team_id"));
    }
    if channel.display_name.is_empty() {
        return Err(ApiError::invalid_param("display_name"));
    }

    let create_permission = match channel.channel_type.as_str() {
        CHANNEL_TYPE_OPEN => Some(&PERMISSION_CREATE_PUBLIC_CHANNEL),
        CHANNEL_TYPE_PRIVATE => Some(&PERMISSION_CREATE_PRIVATE_CHANNEL),
        // No `default:` arm in Go either — every other type reaches the app layer unchecked.
        _ => None,
    };
    if let Some(permission) = create_permission
        && !state
            .app
            .session_has_permission_to_team(session, &channel.team_id, permission)
            .await
    {
        return Err(make_permission_error(session, &[permission]).into());
    }

    if channel.discoverable {
        if !FEATURE_FLAG_DISCOVERABLE_CHANNELS {
            return Err(AppError::boxed(
                "createChannel",
                "api.channel.discoverable_join_request.feature_disabled.app_error",
                None,
                String::new(),
                400,
            )
            .into());
        }
        if channel.channel_type != CHANNEL_TYPE_PRIVATE {
            return Err(AppError::boxed(
                "createChannel",
                "model.channel.is_valid.discoverable.app_error",
                None,
                String::new(),
                400,
            )
            .into());
        }
        if !state
            .app
            .session_has_permission_to_team(
                session,
                &channel.team_id,
                &PERMISSION_MANAGE_PRIVATE_CHANNEL_DISCOVERABILITY,
            )
            .await
        {
            return Err(make_permission_error(
                session,
                &[&PERMISSION_MANAGE_PRIVATE_CHANNEL_DISCOVERABILITY],
            )
            .into());
        }
    }

    state
        .app
        .create_channel_with_user(channel, &session.user_id)
        .await?;
    created("createChannel", channel)
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/channels/direct
// ---------------------------------------------------------------------------------------------

/// Port of `createDirectChannel` (api4/channel.go:625).
///
/// # A one-element list is legal, and only for yourself
///
/// `["<me>"]` is duplicated into `["<me>","<me>"]` — a DM with yourself, which Mattermost calls
/// the self-channel. `["<someone-else>"]` is **not** duplicated and falls into the `len != 2`
/// branch as a 400. The de-duplication upstream is what makes the check necessary:
/// `["<me>","<me>"]` arrives as a one-element list too.
///
/// # `allowed` is set inside the id-validation loop
///
/// So a body naming two other users fails the `allowed` test and needs `manage_system` — a system
/// admin may open a DM between two other people. Everyone else gets a **403 naming
/// `manage_system`**, not `create_direct_channel`.
///
/// # Then `UserCanSeeOtherUser`, whose refusal is a third different 403
///
/// `view_members`. On a stock server nobody is under view restrictions so it always passes; a
/// caller who *is* restricted needs two store lookups this port does not have, and that request
/// is forwarded rather than guessed (see `mm_app::App::user_can_see_other_user`).
#[tracing::instrument(skip_all, fields(forwarded = false))]
pub async fn create_direct_channel(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (request, bytes) = match split_body(request, "user_ids").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };

    let mut user_ids = match non_sorted_array_from_json(&bytes) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::debug!(error = %err, "user id list did not decode");
            return ApiError::from(AppError::new(
                "createDirectChannel",
                PAYLOAD_PARSE_ERROR,
                None,
                String::new(),
                400,
            ))
            .into_response();
        }
    };

    // "single userId allowed if creating a self-channel … NonSortedArrayFromJSON will remove
    // duplicates, so need to add back".
    if user_ids.len() == 1 && user_ids[0] == session.0.user_id {
        let me = user_ids[0].clone();
        user_ids.push(me);
    }
    if user_ids.len() != 2 {
        return ApiError::invalid_param("user_ids").into_response();
    }

    let mut allowed = false;
    for id in &user_ids {
        if !is_valid_id(id) {
            return ApiError::invalid_param("user_id").into_response();
        }
        if *id == session.0.user_id {
            allowed = true;
        }
    }

    match serve_create_direct_channel(&state, &session.0, &user_ids, allowed).await {
        Ok(Some(response)) => response,
        Ok(None) => {
            tracing::Span::current().record("forwarded", true);
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

/// `Ok(None)` means "forward" — either a view-restricted caller or a
/// `RestrictDirectMessage = "team"` installation. Both are decided before anything is written.
async fn serve_create_direct_channel(
    state: &AppState,
    session: &Session,
    user_ids: &[String],
    allowed: bool,
) -> Result<Option<Response>, ApiError> {
    if !state
        .app
        .session_has_permission_to(session, &PERMISSION_CREATE_DIRECT_CHANNEL)
        .await
    {
        return Err(make_permission_error(session, &[&PERMISSION_CREATE_DIRECT_CHANNEL]).into());
    }

    if !allowed
        && !state
            .app
            .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return Err(make_permission_error(session, &[&PERMISSION_MANAGE_SYSTEM]).into());
    }

    // `otherUserId := userIds[0]; if session.UserId == otherUserId { otherUserId = userIds[1] }`
    // — so for a self-DM both are the session's own id and `UserCanSeeOtherUser` short-circuits.
    let other_user_id = if session.user_id == user_ids[0] {
        &user_ids[1]
    } else {
        &user_ids[0]
    };

    match state
        .app
        .user_can_see_other_user(&session.user_id, other_user_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(make_permission_error(session, &[&PERMISSION_VIEW_MEMBERS]).into());
        }
        Err(mm_app::post::PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            return Ok(None);
        }
        Err(mm_app::post::PrepareError::App(err)) => return Err(ApiError::from(*err)),
    }

    // The two ids keep the **body's** order, which is what decides `creator_id` on the event.
    match state
        .app
        .get_or_create_direct_channel(&user_ids[0], &user_ids[1])
        .await?
    {
        ChannelCreate::Created(channel) => created("createDirectChannel", &channel).map(Some),
        ChannelCreate::Forward(reason) => {
            tracing::debug!(reason, "forwarding to Go");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/channels/group
// ---------------------------------------------------------------------------------------------

/// Port of `createGroupChannel` (api4/channel.go:758).
///
/// # The session's own id is appended, not required
///
/// A body that omits the caller gets them added — at the **end** of an otherwise sorted list, and
/// the size bound is checked afterwards in the app layer. So `["a","b","c"]` from a fourth user
/// is a four-member group channel, and eight other users is a 400
/// (`api.channel.create_group.bad_size.app_error`) for nine.
///
/// # An empty list is `user_ids`, a bad id is `user_id`
///
/// Two different 400 bodies one `s` apart, and the empty-list branch fires before the loop, so
/// `[]` never reaches the id validation.
///
/// # `canSeeAll` does not short-circuit
///
/// Go keeps looping after the first user it cannot see, so a body with two invisible users costs
/// two lookups and still answers one 403. Reproduced — the loop's *errors* are returned
/// immediately, and only the boolean is deferred.
#[tracing::instrument(skip_all, fields(forwarded = false, members))]
pub async fn create_group_channel(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (request, bytes) = match split_body(request, "user_ids").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };

    let mut user_ids = match sorted_array_from_json(&bytes) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::debug!(error = %err, "user id list did not decode");
            return ApiError::from(AppError::new(
                "createGroupChannel",
                PAYLOAD_PARSE_ERROR,
                None,
                String::new(),
                400,
            ))
            .into_response();
        }
    };

    if user_ids.is_empty() {
        return ApiError::invalid_param("user_ids").into_response();
    }

    let mut found = false;
    for id in &user_ids {
        if !is_valid_id(id) {
            return ApiError::invalid_param("user_id").into_response();
        }
        if *id == session.0.user_id {
            found = true;
        }
    }
    if !found {
        user_ids.push(session.0.user_id.clone());
    }
    tracing::Span::current().record("members", user_ids.len());

    match serve_create_group_channel(&state, &session.0, &user_ids).await {
        Ok(Some(response)) => response,
        Ok(None) => {
            tracing::Span::current().record("forwarded", true);
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

async fn serve_create_group_channel(
    state: &AppState,
    session: &Session,
    user_ids: &[String],
) -> Result<Option<Response>, ApiError> {
    if !state
        .app
        .session_has_permission_to(session, &PERMISSION_CREATE_GROUP_CHANNEL)
        .await
    {
        return Err(make_permission_error(session, &[&PERMISSION_CREATE_GROUP_CHANNEL]).into());
    }

    let mut can_see_all = true;
    for id in user_ids {
        if session.user_id == *id {
            continue;
        }
        match state
            .app
            .user_can_see_other_user(&session.user_id, id)
            .await
        {
            Ok(true) => {}
            Ok(false) => can_see_all = false,
            Err(mm_app::post::PrepareError::Unreproducible(reason)) => {
                tracing::debug!(reason, "forwarding to Go");
                return Ok(None);
            }
            Err(mm_app::post::PrepareError::App(err)) => return Err(ApiError::from(*err)),
        }
    }

    if !can_see_all {
        return Err(make_permission_error(session, &[&PERMISSION_VIEW_MEMBERS]).into());
    }

    let channel = state
        .app
        .create_group_channel(user_ids, &session.user_id)
        .await?;
    created("createGroupChannel", &channel).map(Some)
}
