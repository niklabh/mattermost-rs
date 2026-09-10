//! Port of the six channel-membership **write** handlers of `channels/api4/channel.go`:
//! `addChannelMember` (:2333), `setChannelMembers` (:2585), `removeChannelMember` (:2757),
//! `updateChannelMemberRoles` (:2142), `updateChannelMemberSchemeRoles` (:2184) and
//! `updateChannelMemberNotifyProps` (:2224).
//!
//! In a new module rather than in `channels.rs` because the read handlers there are 4,200 lines
//! already, and because these six share four helpers with each other and none with the reads.
//!
//! # `MapFromJSON` never returns nil, and that kills a branch
//!
//! Three of these handlers decode their body with `model.MapFromJSON` (utils.go:507), which
//! **swallows every decode error and substitutes an empty map**. So `updateChannelMemberNotifyProps`'s
//! `if props == nil { SetInvalidParam("notify_props") }` is unreachable: a body of `[]`, `"x"`,
//! `{"desktop": 5}` or nothing at all all arrive as `{}` and are accepted with a 200. Reproduced,
//! and asserted — a port that returned 400 for a malformed body here would be *more* correct and
//! wrong on the wire. See [`map_from_json`].
//!
//! # Two of the six reject board and space channels; the other four do not
//!
//! `rejectBoardChannelByID`/`rejectSpaceChannelByID` guard the three `PUT …/{user_id}/…` handlers
//! only. `addChannelMember`, `setChannelMembers` and `removeChannelMember` have neither, so a
//! board id reaches `GetChannel` there and answers its 404 instead of the guards' 400. Adding the
//! guards uniformly would be tidier and would change three routes' answers.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::channel_member::{ChannelMemberOpts, MemberWrite};
use mm_model::channel::{
    CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE,
    CHANNEL_TYPE_SPACE, Channel,
};
use mm_model::channel_member::ChannelMember;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_INVITE_USER, PERMISSION_JOIN_PUBLIC_CHANNELS,
    PERMISSION_MANAGE_CHANNEL_ROLES, PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS,
    PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS, PERMISSION_MANAGE_SYSTEM, PERMISSION_READ_CHANNEL,
    make_permission_error,
};
use mm_model::role::is_valid_channel_member_roles;
use mm_model::scheme::SchemeRoles;
use mm_model::utils::{AppError, StringMap, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// `maxListSize` (api4/channel.go:18) — the cap on `user_ids`.
const MAX_LIST_SIZE: usize = 1000;

/// Port of `model.MapFromJSON` (utils.go:507).
///
/// **Every failure is an empty map**, including a body that is not an object, an object with a
/// non-string value, and an empty body. Go's `json.NewDecoder(...).Decode(&objmap)` result is
/// discarded and a nil map is replaced with an allocated one, so the caller can never distinguish
/// "no keys" from "unparseable".
///
/// Note the partial-decode case: Go's decoder fills the map as it goes and only *then* fails, but
/// `objmap` is non-nil by then, so `{"a":"b","c":5}` yields `{"a":"b"}` — not `{}`. `serde_json`
/// has no partial result, so this returns `{}` for that input. The difference is reachable only
/// with a mixed-type object, and only on the three routes that read a single key out of the map;
/// see the parity suite, which asserts the shared cases and records this one.
fn map_from_json(bytes: &[u8]) -> StringMap {
    serde_json::from_slice::<StringMap>(bytes).unwrap_or_default()
}

/// Port of `model.StringInterfaceFromJSON` (utils.go:590) — the same swallow-everything shape for
/// `map[string]any`.
fn string_interface_from_json(bytes: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(bytes).unwrap_or_default()
}

/// `ReturnStatusOK` (api4/apitestlib-free; web/handlers) — `{"status":"OK"}`, encoder-framed.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        "{\"status\":\"OK\"}\n",
    )
        .into_response()
}

/// Port of `rejectBoardChannelByID` (api4/channel.go:23) and `rejectSpaceChannelByID` (:36), in
/// Go's order — board first.
///
/// **Both treat "found" as the refusal.** `rejectBoardChannelByID` tests `err == nil`, so any
/// error — a broken database included — reads as "not a board" and the request carries on.
/// `rejectSpaceChannelByID` is stricter and says so in its comment: it fails **closed**, returning
/// the error itself for anything that is not a 404, so a database failure there is a 500 rather
/// than a pass. The asymmetry is Go's.
///
/// The `AppError`'s `where` is the empty string in both — `NewAppError("", …)` — and
/// `handleContextError` overwrites it with the request path, so it is invisible to a client.
async fn reject_board_or_space_channel(state: &AppState, channel_id: &str) -> Option<ApiError> {
    if state.app.get_board_channel(channel_id).await.is_ok() {
        return Some(ApiError::from(AppError::new(
            "",
            "api.channel.board_channel.app_error",
            None,
            "board channels cannot be accessed via /channels endpoints".to_owned(),
            400,
        )));
    }

    match state
        .app
        .get_channel_of_type(channel_id, CHANNEL_TYPE_SPACE)
        .await
    {
        Ok(_) => Some(ApiError::from(AppError::new(
            "",
            "api.channel.space_channel.app_error",
            None,
            "space channels cannot be accessed via /channels endpoints".to_owned(),
            400,
        ))),
        Err(err) if err.status_code == 404 => None,
        Err(err) => Some(ApiError::from(err)),
    }
}

/// Go's `c.RequireChannelId().RequireUserId()`, in that order — the channel id's error wins when
/// both segments are malformed.
#[allow(clippy::result_large_err)]
fn require_channel_and_user(channel_id: &str, user_id: &str) -> Result<(), ApiError> {
    if !is_valid_id(channel_id) {
        return Err(ApiError::invalid_url_param("channel_id"));
    }
    if !is_valid_id(user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }
    Ok(())
}

/// Port of `updateChannelMemberRoles` (api4/channel.go:2142) —
/// `PUT /api/v4/channels/{channel_id}/members/{user_id}/roles`.
///
/// # The order of the four gates is observable
///
/// ids → board/space → **body validation** → permission. So a caller holding nothing at all who
/// sends `{"roles":"system_admin"}` gets a **400**, not a 403: `IsValidChannelMemberRoles` runs
/// first. Swapping the last two would leak nothing but would answer 403 where Go answers 400.
///
/// # A missing `roles` key is the empty string, and the empty string is *valid*
///
/// `props["roles"]` on a map without the key is `""`, and `IsValidChannelMemberRoles("")` is
/// **true** — its loop over `strings.Fields` never runs. So an empty body passes this gate and
/// fails four layers down with `update_channel_member_roles.unset_user_scheme.app_error`, because
/// clearing every role leaves the member with no base scheme role.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn update_channel_member_roles(
    State(state): State<AppState>,
    Path((channel_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_channel_and_user(&channel_id, &user_id) {
        return err.into_response();
    }
    if let Some(err) = reject_board_or_space_channel(&state, &channel_id).await {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("roles").into_response();
        }
    };
    let props = map_from_json(&bytes);
    let new_roles = props.get("roles").map(String::as_str).unwrap_or_default();

    if !is_valid_channel_member_roles(new_roles) {
        return ApiError::invalid_param("roles").into_response();
    }

    let (granted, _) = state
        .app
        .session_has_permission_to_channel(
            &session.0,
            &channel_id,
            &PERMISSION_MANAGE_CHANNEL_ROLES,
        )
        .await;
    if !granted {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_CHANNEL_ROLES],
        ))
        .into_response();
    }

    match state
        .app
        .update_channel_member_roles(&channel_id, &user_id, new_roles)
        .await
    {
        Ok(_) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateChannelMemberSchemeRoles` (api4/channel.go:2184) —
/// `PUT /api/v4/channels/{channel_id}/members/{user_id}/schemeRoles`.
///
/// # The body is a struct here, not a map
///
/// `json.NewDecoder(r.Body).Decode(&schemeRoles)` — so unlike its `/roles` sibling this route
/// **does** 400 on a malformed body, with `SetInvalidParamWithErr("scheme_roles", err)`: the same
/// `api.context.invalid_body_param.app_error` plus the decoder's message wrapped underneath, which
/// reaches `detailed_error` only when `EnableDeveloper` is on.
///
/// `model.SchemeRoles` has three plain `bool`s and no `Default` behaviour of its own, so a body of
/// `{}` decodes cleanly to three `false`s — which the app layer then refuses with
/// `unset_user_scheme`.
///
/// # There is no `roles`-style validation, and the interaction with `/roles` is easy to invert
///
/// This route sets the three *scheme flags* and leaves `explicit_roles` alone (on a migrated
/// server); `/roles` sets `explicit_roles` and derives the flags from the submitted names. So the
/// two routes write disjoint halves of the same row, and a client that uses `/schemeRoles` to
/// grant admin does not lose the custom roles `/roles` gave it.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn update_channel_member_scheme_roles(
    State(state): State<AppState>,
    Path((channel_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_channel_and_user(&channel_id, &user_id) {
        return err.into_response();
    }
    if let Some(err) = reject_board_or_space_channel(&state, &channel_id).await {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("scheme_roles").into_response();
        }
    };
    let scheme_roles: SchemeRoles = match serde_json::from_slice(&bytes) {
        Ok(roles) => roles,
        Err(err) => {
            tracing::debug!(error = %err, "scheme_roles body did not decode");
            return ApiError::invalid_param("scheme_roles").into_response();
        }
    };

    let (granted, _) = state
        .app
        .session_has_permission_to_channel(
            &session.0,
            &channel_id,
            &PERMISSION_MANAGE_CHANNEL_ROLES,
        )
        .await;
    if !granted {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_CHANNEL_ROLES],
        ))
        .into_response();
    }

    match state
        .app
        .update_channel_member_scheme_roles(
            &channel_id,
            &user_id,
            scheme_roles.scheme_guest,
            scheme_roles.scheme_user,
            scheme_roles.scheme_admin,
        )
        .await
    {
        Ok(_) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateChannelMemberNotifyProps` (api4/channel.go:2224) —
/// `PUT /api/v4/channels/{channel_id}/members/{user_id}/notify_props`.
///
/// # The permission is about the **user**, not the channel
///
/// `SessionHasPermissionToUser` — self, or `edit_other_users`. So a system admin can change
/// anybody's notification settings in any channel, and a caller cannot change their *own* settings
/// in a channel they are not in: that fails later, as a 404 from the store's re-select, not as a
/// 403 here.
///
/// # Nothing validates the values
///
/// See [`mm_app::App::update_channel_member_notify_props`]: the ten known keys are copied out and
/// the rest dropped, with no call to `IsChannelMemberNotifyPropsValid`. `{"desktop":"banana"}` is a
/// 200.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn update_channel_member_notify_props(
    State(state): State<AppState>,
    Path((channel_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_channel_and_user(&channel_id, &user_id) {
        return err.into_response();
    }
    if let Some(err) = reject_board_or_space_channel(&state, &channel_id).await {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("notify_props").into_response();
        }
    };
    // `if props == nil` is unreachable — `MapFromJSON` allocates. See the module docs.
    let props = map_from_json(&bytes);

    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    match state
        .app
        .update_channel_member_notify_props(&props, &channel_id, &user_id)
        .await
    {
        Ok(_) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `removeChannelMember` (api4/channel.go:2757) —
/// `DELETE /api/v4/channels/{channel_id}/members/{user_id}`.
///
/// # No board or space guard, and the channel type check is an allow-list
///
/// `!(Type == 'O' || Type == 'P')` → 400 `api.channel.remove_channel_member.type.app_error`. That
/// covers DM, GM *and* every backing type, so a space id that survives `GetChannel` — it does not,
/// because `Get` filters its type out — would land here rather than on the space guard's message.
///
/// # A bot is exempt from the group-constraint refusal, a human is not
///
/// `IsGroupConstrained() && userId != session.UserId && !user.IsBot` → 400. So an integration can
/// be removed from a group-synced channel and a person cannot, and **leaving one yourself is
/// always allowed** — the `userId != session.UserId` clause is what makes the leave button work in
/// a group-synced channel.
///
/// # The permission check is skipped entirely for a self-removal
///
/// Which is why leaving a private channel needs no `manage_private_channel_members`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id, forwarded))]
pub async fn remove_channel_member(
    State(state): State<AppState>,
    Path((channel_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_channel_and_user(&channel_id, &user_id) {
        return err.into_response();
    }

    let channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if channel.channel_type != CHANNEL_TYPE_OPEN && channel.channel_type != CHANNEL_TYPE_PRIVATE {
        return ApiError::from(AppError::new(
            "removeChannelMember",
            "api.channel.remove_channel_member.type.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    if channel.is_group_constrained() && user_id != session.0.user_id && !user.is_bot {
        return ApiError::from(AppError::new(
            "removeChannelMember",
            "api.channel.remove_member.group_constrained.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    if user_id != session.0.user_id {
        let permission = if channel.channel_type == CHANNEL_TYPE_OPEN {
            &PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS
        } else {
            &PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS
        };
        let (granted, _) = state
            .app
            .session_has_permission_to_channel(&session.0, &channel.id, permission)
            .await;
        if !granted {
            return ApiError::from(*make_permission_error(&session.0, &[permission]))
                .into_response();
        }
    }

    match state
        .app
        .remove_user_from_channel(&user_id, &session.0.user_id, &channel)
        .await
    {
        Ok(MemberWrite::Done(())) => {
            tracing::Span::current().record("forwarded", false);
            status_ok()
        }
        Ok(MemberWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the member removal to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// What `addChannelMember`'s body says to do, once the three shapes are resolved.
#[derive(Debug)]
struct AddMemberRequest {
    user_ids: Vec<String>,
    /// True when the body carried a `user_id` **key** at all, of any type. It is what decides
    /// whether the answer is a bare object or an array — see [`add_channel_member`].
    had_user_id_key: bool,
    /// The `user_id` value, when it was a string. Compared against the single resulting member.
    user_id_value: Option<String>,
}

/// Port of `addChannelMember` (api4/channel.go:2333) —
/// `POST /api/v4/channels/{channel_id}/members`.
///
/// # Three body shapes, and `user_ids` wins
///
/// `{"user_ids": [...]}` is tried first; **anything else** falls through to `{"user_id": "..."}`,
/// including a `user_ids` that is present but not an array. Both are validated with `IsValidId` and
/// answer different parameter names — `"user_id in user_ids"` versus `"user_id or user_ids"` —
/// which is the only way a client tells the two failures apart.
///
/// # The answer's shape depends on a key, not on the count
///
/// `props["user_id"]` being **present** *and* exactly one member resulting *and* that member's id
/// equalling the value ⇒ a bare `ChannelMember` object. Otherwise an array. So `{"user_ids":["x"]}`
/// answers `[{…}]` while `{"user_id":"x"}` answers `{…}`, and a body carrying *both* keys answers
/// an object when the single added member happens to be the `user_id` one. Go compares a `string`
/// to an `any`, so a non-string `user_id` never matches.
///
/// # `201`, and the body can be `null`
///
/// The status is written before the encode, so a nil member slice — reachable when every id in
/// `user_ids` was skipped for a reason that did not set an error — is `201` with `null\n`.
///
/// # A partial failure writes **two** JSON documents
///
/// The per-id loop calls `c.SetInvalidParam`/`c.SetPermissionError`, which set `c.Err` and are
/// *not* cleared by a later success. `handleContextError` runs after the handler, so a request that
/// added one member and was refused another answers `201` with the member array **followed by the
/// error object**. Reproduced: it is on the wire, and it is not something a reader would guess.
///
/// # What is forwarded
///
/// A **guest** session (`UserCanSeeOtherUser`'s restricted branch), a `post_root_id`
/// (`GetSinglePost` plus a `ThreadMemberships` write), a **discoverable private** channel (the
/// join-request queue), and a **group-constrained** channel (`FilterNonGroupChannelMembers`).
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, added, forwarded))]
pub async fn add_channel_member(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&channel_id) {
        return ApiError::invalid_url_param("channel_id").into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("user_id or user_ids").into_response();
        }
    };
    let props = string_interface_from_json(&bytes);

    let parsed = match parse_add_member_body(&props) {
        Ok(parsed) => parsed,
        Err(err) => return err.into_response(),
    };

    // `post_root_id` pulls the caller into a thread: `GetSinglePost`, a channel check, and then
    // `UpdateThreadFollowForUserFromChannelAdd` per added member — a `ThreadMemberships` write.
    match props.get("post_root_id").and_then(|v| v.as_str()) {
        Some(root_id) if !root_id.is_empty() => {
            if !is_valid_id(root_id) {
                return ApiError::invalid_param("post_root_id").into_response();
            }
            tracing::Span::current().record("forwarded", true);
            return forward(state, parts, bytes).await;
        }
        _ => {}
    }

    let channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if session.0.is_guest() {
        // Go checks `read_channel` and then `UserCanSeeOtherUser` per id; the latter's restricted
        // branch needs the team/channel membership lookups this port does not have.
        tracing::Span::current().record("forwarded", true);
        return forward(state, parts, bytes).await;
    }

    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP {
        // Note the `where`: Go writes `addUserToChannel`, not `addChannelMember`.
        return ApiError::from(AppError::new(
            "addUserToChannel",
            "api.channel.add_user_to_channel.type.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    let mut can_add_self = false;
    let mut can_add_others = false;
    if channel.channel_type == CHANNEL_TYPE_OPEN {
        can_add_self = state
            .app
            .session_has_permission_to_team(
                &session.0,
                &channel.team_id,
                &PERMISSION_JOIN_PUBLIC_CHANNELS,
            )
            .await;
        can_add_others = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &channel.id,
                &PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS,
            )
            .await
            .0;
    }

    if channel.channel_type == CHANNEL_TYPE_PRIVATE {
        let (granted, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &channel.id,
                &PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS,
            )
            .await;
        if !granted {
            return ApiError::from(*make_permission_error(
                &session.0,
                &[&PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS],
            ))
            .into_response();
        }

        // `IsDiscoverableSelfAddBlocked` needs `FeatureFlags.DiscoverableChannels`, which this
        // port does not read. Forwarding on the *channel* flag alone matches Go on both sides of
        // the feature flag, at the cost of forwarding a discoverable channel's add even when the
        // flag is off.
        if channel.discoverable {
            tracing::Span::current().record("forwarded", true);
            return forward(state, parts, bytes).await;
        }
    }

    if channel.is_group_constrained() {
        tracing::Span::current().record("forwarded", true);
        return forward(state, parts, bytes).await;
    }

    // Go's loop, with `lastError` and `c.Err` tracked separately: `lastError` decides whether the
    // request failed outright, and `c.Err` — which the loop sets and never clears — decides
    // whether an error document is appended to a successful body.
    let mut last_error: Option<Box<AppError>> = None;
    let mut context_error: Option<Box<AppError>> = None;
    let mut new_members: Vec<ChannelMember> = Vec::new();

    for member_user_id in &parsed.user_ids {
        let existing = match state
            .app
            .get_channel_member(&channel.id, member_user_id)
            .await
        {
            Ok(member) => Some(member),
            Err(err) if err.id == "app.channel.get_member.missing.app_error" => None,
            Err(err) => {
                tracing::warn!(user_id = %member_user_id, error = %err, "Error adding channel member, error getting channel member");
                last_error = Some(err);
                continue;
            }
        };

        if channel.channel_type == CHANNEL_TYPE_OPEN {
            let is_self_add = member_user_id == &session.0.user_id;
            if is_self_add && existing.is_some() {
                // Already a member: allowed even with no permission at all.
                if let Some(member) = existing {
                    new_members.push(member);
                }
                continue;
            } else if is_self_add && !can_add_self {
                // `c.SetPermissionError` sets `c.Err`, and `lastError = c.Err` aliases the same
                // pointer in Go. Two separate values here, built from the same inputs, because
                // `AppError` is not `Clone` — and they are read for different purposes: one
                // decides whether the request failed, the other whether an error document is
                // appended to a successful body.
                context_error = Some(make_permission_error(
                    &session.0,
                    &[&PERMISSION_JOIN_PUBLIC_CHANNELS],
                ));
                last_error = Some(make_permission_error(
                    &session.0,
                    &[&PERMISSION_JOIN_PUBLIC_CHANNELS],
                ));
                continue;
            } else if !is_self_add && !can_add_others {
                context_error = Some(make_permission_error(
                    &session.0,
                    &[&PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS],
                ));
                last_error = Some(make_permission_error(
                    &session.0,
                    &[&PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS],
                ));
                continue;
            }
        }

        if let Some(member) = existing {
            new_members.push(member);
            continue;
        }

        let opts = ChannelMemberOpts {
            user_requestor_id: session.0.user_id.clone(),
            post_root_id: String::new(),
            skip_team_member_integrity_check: false,
        };
        match state
            .app
            .add_channel_member(member_user_id, &channel, &opts)
            .await
        {
            Ok(MemberWrite::Done(member)) => new_members.push(member),
            Ok(MemberWrite::Forward(why)) => {
                tracing::Span::current().record("forwarded", true);
                tracing::debug!(reason = why, "handing the member add to Go");
                return forward(state, parts, bytes).await;
            }
            Err(err) => {
                tracing::warn!(user_id = %member_user_id, error = %err, "Error adding channel member");
                last_error = Some(err);
                continue;
            }
        }
    }

    if new_members.is_empty()
        && let Some(err) = last_error
    {
        return ApiError::from(err).into_response();
    }

    let me = &session.0.user_id;
    for member in &mut new_members {
        member.sanitize_for_current_user(me);
    }
    tracing::Span::current().record("added", new_members.len());
    tracing::Span::current().record("forwarded", false);

    let single = parsed.had_user_id_key
        && new_members.len() == 1
        && parsed.user_id_value.as_deref() == Some(new_members[0].user_id.as_str());

    let encoded = if single {
        mm_model::utils::go_json_marshal(&new_members[0])
    } else {
        mm_model::utils::go_json_marshal(&new_members)
    };
    let mut body = match encoded {
        Ok(json) => json + "\n",
        Err(err) => {
            tracing::warn!(error = %err, "Error while writing response");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // The second document. `handleContextError` writes it after the handler returns; the status is
    // already committed, so only the body grows.
    if let Some(err) = context_error {
        // `into_wire` is the same path `IntoResponse` takes — request id minted, `detailed_error`
        // wiped — minus the status, which is already committed as 201. The same split
        // `getChannelsForUser`'s streaming error uses.
        let (_status, appended) = ApiError::from(err).into_wire();
        if let Some(appended) = appended
            && let Ok(text) = String::from_utf8(appended)
        {
            body.push_str(&text);
        }
    }

    (
        StatusCode::CREATED,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

/// The three body shapes of `addChannelMember` (api4/channel.go:2340-2364).
#[allow(clippy::result_large_err)]
fn parse_add_member_body(
    props: &serde_json::Map<String, serde_json::Value>,
) -> Result<AddMemberRequest, ApiError> {
    let had_user_id_key = props.contains_key("user_id");
    let user_id_value = props
        .get("user_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    if let Some(serde_json::Value::Array(ids)) = props.get("user_ids") {
        if ids.len() > MAX_LIST_SIZE {
            return Err(ApiError::invalid_param("user_ids"));
        }
        let mut user_ids = Vec::with_capacity(ids.len());
        for id in ids {
            match id.as_str() {
                Some(id) if is_valid_id(id) => user_ids.push(id.to_owned()),
                // One message for both "not a string" and "not an id".
                _ => return Err(ApiError::invalid_param("user_id in user_ids")),
            }
        }
        return Ok(AddMemberRequest {
            user_ids,
            had_user_id_key,
            user_id_value,
        });
    }

    // Anything other than an array under `user_ids` — absent, a string, a number — falls through
    // to the single-id shape, which is why `{"user_ids": "abc"}` reports `user_id or user_ids`.
    match user_id_value.as_deref() {
        Some(id) if is_valid_id(id) => Ok(AddMemberRequest {
            user_ids: vec![id.to_owned()],
            had_user_id_key,
            user_id_value,
        }),
        _ => Err(ApiError::invalid_param("user_id or user_ids")),
    }
}

/// Port of `setChannelMembers` (api4/channel.go:2585) —
/// `PUT /api/v4/channels/{channel_id}/members`.
///
/// # System admin only, and the check is first
///
/// `PermissionManageSystem` before the query parameters are even parsed, so a non-admin sending a
/// bad `batch_size` gets a 403 rather than a 400.
///
/// # The response is NDJSON, streamed, and its content type appears only with the first line
///
/// `Content-Type: application/x-ndjson` is set inside the per-batch callback, so a run that
/// produces **no** batches never sets it — and `App::set_channel_members` always calls the callback
/// at least once (with an empty response) when there is nothing to do, so in practice the header is
/// always there. `added` and `removed` are forced from nil to `[]` in the callback, which is why
/// every line has both keys as arrays while `promoted`, `demoted` and `errors` are `omitempty`.
///
/// # An error after streaming has begun is a **line**, not a status
///
/// Go cannot set `c.Err` once bytes are on the wire, so it appends `{"error":"…"}` and logs. Before
/// the first line it is an ordinary error response. Both reproduced.
///
/// # Forwarded rather than answered
///
/// This port streams synchronously with **no batch delay**: `batch_delay_ms` is accepted and
/// validated, and then honoured, because a client that asked for a 500 ms gap between batches is
/// asking for backpressure and getting it wrong changes how long the request takes and nothing
/// else. Any single member operation that the add or remove path would forward makes the whole
/// request forward instead — which can only happen before the first line is written, because the
/// batching is resolved up front.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, forwarded))]
pub async fn set_channel_members(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&channel_id) {
        return ApiError::invalid_url_param("channel_id").into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let (parts, body) = request.into_parts();
    let query = parts.uri.query().unwrap_or_default().to_owned();

    let batch_size = match bounded_query_int(&query, "batch_size", 100, 1, 1000) {
        Ok(value) => value,
        Err(name) => return ApiError::invalid_url_param(name).into_response(),
    };
    let batch_delay_ms = match bounded_query_int(&query, "batch_delay_ms", 500, 0, 10_000) {
        Ok(value) => value,
        Err(name) => return ApiError::invalid_url_param(name).into_response(),
    };

    // `http.MaxBytesReader(w, r.Body, 12 MB)`. Go's overrun surfaces as a decode error, which the
    // handler reports as `SetInvalidParamWithErr("body", err)` — *not* as the global
    // `request_body_too_large`, because `handleContextError` only rewrites a `MaxBytesError` that
    // reaches it unwrapped.
    const MAX_BODY: usize = 12 * 1024 * 1024;
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "set_channel_members body was unreadable or too large");
            return ApiError::invalid_param("body").into_response();
        }
    };

    let req: mm_model::channel_member::SetChannelMembersRequest =
        match serde_json::from_slice(&bytes) {
            Ok(req) => req,
            Err(err) => {
                tracing::debug!(error = %err, "set_channel_members body did not decode");
                return ApiError::invalid_param("body").into_response();
            }
        };
    let Some(members) = req.members else {
        return ApiError::invalid_param("members").into_response();
    };

    // Validate and deduplicate, preserving first-seen order — the order the batches go out in.
    let mut desired: Vec<String> = Vec::with_capacity(members.len());
    for id in &members {
        if !is_valid_id(id) {
            return ApiError::invalid_param("members").into_response();
        }
        if !desired.contains(id) {
            desired.push(id.clone());
        }
    }

    // `ChannelAdmins` nil versus empty is the whole difference between "leave admin roles alone"
    // and "demote every admin", so it stays an `Option`.
    let mut admin_set: Option<Vec<String>> = None;
    if let Some(admins) = &req.channel_admins {
        let mut set = Vec::with_capacity(admins.len());
        for id in admins {
            if !is_valid_id(id) {
                return ApiError::invalid_param("channel_admins").into_response();
            }
            if !set.contains(id) {
                set.push(id.clone());
            }
            if !desired.contains(id) {
                desired.push(id.clone());
            }
        }
        admin_set = Some(set);
    }

    let channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP {
        return set_members_error(&channel, "type").into_response();
    }
    if channel.is_group_constrained() {
        return set_members_error(&channel, "group_constrained").into_response();
    }
    if channel.has_membership_policy_action() {
        return set_members_error(&channel, "policy_enforced").into_response();
    }

    match state
        .app
        .set_channel_members(
            &channel,
            &desired,
            admin_set.as_deref(),
            &session.0.user_id,
            batch_size,
            batch_delay_ms,
        )
        .await
    {
        Ok(MemberWrite::Done(lines)) => {
            tracing::Span::current().record("forwarded", false);
            (
                StatusCode::OK,
                [
                    ("Content-Type", "application/x-ndjson"),
                    ("x-mmrs-served-by", "rust"),
                ],
                lines,
            )
                .into_response()
        }
        Ok(MemberWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the bulk member set to Go");
            forward(state, parts, bytes).await
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// The three `setChannelMembers` refusals, which share everything but one word of the id.
fn set_members_error(_channel: &Channel, suffix: &str) -> ApiError {
    ApiError::from(AppError::new(
        "setChannelMembers",
        format!("api.channel.set_members.{suffix}.app_error"),
        None,
        String::new(),
        400,
    ))
}

/// `strconv.Atoi` on a query parameter with Go's inclusive bounds, or the parameter's name on
/// failure.
///
/// An **empty** value is not an error: Go's `if v := …Get(k); v != ""` skips the parse entirely, so
/// `?batch_size=` uses the default. A non-numeric value, or one outside the range, is
/// `SetInvalidURLParam` — a 400 whose id says *url* param even though the value came from the query
/// string.
fn bounded_query_int(
    query: &str,
    name: &'static str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, &'static str> {
    let raw = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value);
    match raw {
        None | Some("") => Ok(default),
        Some(value) => match value.parse::<usize>() {
            Ok(n) if n >= min && n <= max => Ok(n),
            _ => Err(name),
        },
    }
}

async fn forward(
    state: AppState,
    parts: axum::http::request::Parts,
    bytes: axum::body::Bytes,
) -> Response {
    let request = Request::from_parts(parts, Body::from(bytes));
    proxy::forward_to_go(State(state), request).await
}

/// Go's `read_channel`/`invite_user` pair, referenced only from the guest branch's doc comment.
/// Named here so a reader grepping for the permissions this file checks finds them all.
#[allow(dead_code)]
const GUEST_BRANCH_PERMISSIONS: [&mm_model::permission::Permission; 2] =
    [&PERMISSION_READ_CHANNEL, &PERMISSION_INVITE_USER];

#[cfg(test)]
mod tests {
    use super::*;

    /// The swallow-everything decode is the behaviour three routes depend on, and it is the one a
    /// reader is most likely to "fix" into a 400.
    #[test]
    fn map_from_json_turns_every_failure_into_an_empty_map() {
        assert!(map_from_json(b"").is_empty());
        assert!(map_from_json(b"[]").is_empty());
        assert!(map_from_json(b"\"x\"").is_empty());
        assert!(map_from_json(b"{\"roles\": 5}").is_empty());
        assert_eq!(
            map_from_json(b"{\"roles\":\"channel_user\"}").get("roles"),
            Some(&"channel_user".to_owned())
        );
    }

    #[test]
    fn user_ids_wins_over_user_id_and_reports_its_own_parameter_name() {
        let both = serde_json::json!({
            "user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "user_ids": ["bbbbbbbbbbbbbbbbbbbbbbbbbb"],
        });
        let parsed = parse_add_member_body(both.as_object().expect("object")).expect("parses");
        assert_eq!(parsed.user_ids, vec!["bbbbbbbbbbbbbbbbbbbbbbbbbb"]);
        // The key is still recorded, which is what makes a one-member result answer an object.
        assert!(parsed.had_user_id_key);

        // A non-array `user_ids` falls through to the single-id shape.
        let wrong = serde_json::json!({"user_ids": "aaaaaaaaaaaaaaaaaaaaaaaaaa"});
        let err = parse_add_member_body(wrong.as_object().expect("object")).expect_err("refused");
        assert_eq!(
            err.0.params.as_ref().and_then(|p| p.get("Name")),
            Some(&serde_json::json!("user_id or user_ids"))
        );

        // A bad id *inside* the array reports the other name.
        let bad = serde_json::json!({"user_ids": ["short"]});
        let err = parse_add_member_body(bad.as_object().expect("object")).expect_err("refused");
        assert_eq!(
            err.0.params.as_ref().and_then(|p| p.get("Name")),
            Some(&serde_json::json!("user_id in user_ids"))
        );
    }

    #[test]
    fn a_user_ids_list_over_the_cap_is_refused_before_any_id_is_read() {
        let ids: Vec<String> = (0..MAX_LIST_SIZE + 1).map(|_| "short".to_owned()).collect();
        let body = serde_json::json!({"user_ids": ids});
        let err = parse_add_member_body(body.as_object().expect("object")).expect_err("refused");
        // `user_ids`, not `user_id in user_ids` — the length check comes first, so the invalid ids
        // are never reached.
        assert_eq!(
            err.0.params.as_ref().and_then(|p| p.get("Name")),
            Some(&serde_json::json!("user_ids"))
        );
    }

    /// An empty value uses the default; the bounds are **inclusive** on both ends.
    #[test]
    fn bounded_query_int_matches_gos_atoi_and_bounds() {
        assert_eq!(bounded_query_int("", "batch_size", 100, 1, 1000), Ok(100));
        assert_eq!(
            bounded_query_int("batch_size=", "batch_size", 100, 1, 1000),
            Ok(100)
        );
        assert_eq!(
            bounded_query_int("batch_size=1", "batch_size", 100, 1, 1000),
            Ok(1)
        );
        assert_eq!(
            bounded_query_int("batch_size=1000", "batch_size", 100, 1, 1000),
            Ok(1000)
        );
        assert_eq!(
            bounded_query_int("batch_size=0", "batch_size", 100, 1, 1000),
            Err("batch_size")
        );
        assert_eq!(
            bounded_query_int("batch_size=1001", "batch_size", 100, 1, 1000),
            Err("batch_size")
        );
        assert_eq!(
            bounded_query_int("batch_size=x", "batch_size", 100, 1, 1000),
            Err("batch_size")
        );
        // `batch_delay_ms` allows zero, `batch_size` does not — the two minima differ.
        assert_eq!(
            bounded_query_int("batch_delay_ms=0", "batch_delay_ms", 500, 0, 10_000),
            Ok(0)
        );
        // Another parameter's value must not be picked up.
        assert_eq!(
            bounded_query_int("other=7&batch_size=3", "batch_size", 100, 1, 1000),
            Ok(3)
        );
    }

    /// The `where` on the DM/GM refusal is `addUserToChannel`, not the handler's own name. It is
    /// invisible on the wire and visible in Go's log, so nothing but a unit test can pin it.
    #[test]
    fn the_dm_refusal_borrows_another_functions_where() {
        let err = AppError::new(
            "addUserToChannel",
            "api.channel.add_user_to_channel.type.app_error",
            None,
            String::new(),
            400,
        );
        assert_eq!(err.where_, "addUserToChannel");
    }
}
