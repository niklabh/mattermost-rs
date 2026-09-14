//! `channel_local.go`, `post_local.go` and `group_local.go` on the unix socket: 23 pairs.
//!
//! Go registers these in `InitChannelLocal` (channel_local.go:15), `InitPostLocal`
//! (post_local.go:13) and `InitGroupLocal` (group_local.go:12). Eleven of the handlers are the
//! **HTTP ones** — `getAllChannels`, `getChannel`, `getChannelByName` (registered twice in Go on
//! the same path, once here), `getChannelMember`, `getChannelMembers`, the three per-team lists,
//! `getChannelByNameForTeamName`, `getPost`, `getPostsForChannel` — called with
//! [`local_session`], every `SessionHasPermissionTo*` on their path short-circuiting on
//! `Session.Local`. The other twelve are **different functions**: each `local*` handler in
//! `channel_local.go` and `post_local.go` drops the permission checks *and* the user, and the two
//! group handlers drop `requireLicense`. Those are ported here, branch for branch.
//!
//! # What "no user" changes, route by route
//!
//! The socket's session has an empty `UserId`, and the `local*` handlers pass that emptiness on
//! rather than resolving it to an administrator:
//!
//! | route | HTTP | local |
//! |---|---|---|
//! | `POST /channels` | `CreateChannelWithUser` — the caller becomes creator and first member | `CreateChannel(channel, false)`: **no creator, no member**, `creator_id` is `""` |
//! | `DELETE /channels/{id}` | archive notice posted **by the caller**; `?permanent` gated on `EnableAPIChannelDeletion` | notice posted by the **system bot**; `?permanent` ungated |
//! | `POST …/restore` | unarchive notice by the caller | by the system bot, from a goroutine |
//! | `PUT …/privacy` | conversion notice by the caller | by the system bot |
//! | `POST …/move` | `MoveChannel(team, channel, user)` posts "moved from" | `user == nil`: **no notice** |
//! | `POST …/members` | `UserRequestorID` set: "X added to the channel by Y" | unset: "**X joined the channel**", posted by X |
//! | `DELETE …/members/{uid}` | "@X removed from the channel" by the caller | by the system bot |
//! | `DELETE /posts/{id}` | `delete_post`/`delete_others_posts` gates, `?permanent` gated | no gates; `DeleteBy` is `""` |
//!
//! The system bot is `mm_app::App::get_system_bot` — `GetSystemBot`, created on first use and
//! owned by the first administrator by username. It existed in Go and not here until this
//! family needed it; see the doc on that function for what it writes.
//!
//! # Forwarding, and why every forward here is over the socket
//!
//! The reused read handlers each carry a forward branch — `getChannel` for `?as_content_reviewer`,
//! the two by-name reads for a segment outside the mux class, the two post reads for the shapes
//! `mm_app::post` does not reproduce — and every one of them forwards over the **port**, which on
//! this router would answer `APISessionRequired`'s 401 instead of Go's local answer. So each
//! wrapper below decides *before* the handler (the by-name mux checks, the reviewer flag) or takes
//! the handler's decision as a value (`posts::get_post_outcome`) and forwards through
//! [`forward_over_unix`] itself. The write handlers are this module's own and forward the same
//! way. Nothing in this file calls `proxy::forward_to_go`, and the module test pins that.
//!
//! # What still goes to Go, and why
//!
//! - `DELETE /channels/{id}?permanent=true` — `PermanentDeleteChannel` is unported (six store
//!   deletes across posts, members, hooks and the channel row): [D-610].
//! - a member add on a **group-constrained** channel — `FilterNonGroupChannelMembers` needs the
//!   group syncable store, as on the HTTP router.
//! - a permanent post delete of a post **with files**, or a burn-on-read post — the file backend.
//! - a patch or privacy change on a **licensed** installation, and the two group lists when
//!   licensed — the same handovers the HTTP handlers make, for the same app-layer reasons.
//!
//! [D-610]: ../../../../docs/TECH_DEBT.md

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, RawQuery, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use mm_app::channel_member::{ChannelMemberOpts, MemberWrite};
use mm_app::channel_write::ChannelWrite;
use mm_app::post::PrepareError;
use mm_model::channel::{
    CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE, Channel,
    ChannelPatch, DEFAULT_CHANNEL_NAME,
};
use mm_model::utils::{AppError, decode_one_from_json, go_json_marshal, is_valid_id};

use crate::error::ApiError;
use crate::local::{
    GoLocalSocket, forward_over_unix, local_session, partially_migrated,
    partially_migrated_with_ids,
};
use crate::{AppState, channel_creates, channel_move, channel_writes, channels, posts};

/// The 23 registrations, merged into [`crate::local::router`].
///
/// `api.BaseRoutes.ChannelByName.Handle("", …getChannelByName)` appears **twice** in
/// `InitChannelLocal` (channel_local.go:19 and :34) — gorilla registers two identical routes and
/// the first wins, so it is one route with one handler and is registered once here; axum would
/// refuse the duplicate at startup.
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/v4/channels",
            partially_migrated(get(local_get_all_channels).post(local_create_channel)),
        )
        .route(
            "/api/v4/channels/{channel_id}",
            partially_migrated_with_ids(state, get(local_get_channel).delete(local_delete_channel)),
        )
        .route(
            "/api/v4/channels/{channel_id}/patch",
            partially_migrated_with_ids(state, put(local_patch_channel)),
        )
        .route(
            "/api/v4/channels/{channel_id}/move",
            partially_migrated_with_ids(state, post(local_move_channel)),
        )
        .route(
            "/api/v4/channels/{channel_id}/privacy",
            partially_migrated_with_ids(state, put(local_update_channel_privacy)),
        )
        .route(
            "/api/v4/channels/{channel_id}/restore",
            partially_migrated_with_ids(state, post(local_restore_channel)),
        )
        .route(
            "/api/v4/channels/{channel_id}/members",
            partially_migrated_with_ids(
                state,
                get(local_get_channel_members).post(local_add_channel_member),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/members/{user_id}",
            partially_migrated_with_ids(
                state,
                get(local_get_channel_member).delete(local_remove_channel_member),
            ),
        )
        // `group_local.go:13`, on the `Channels` base route with its own id class.
        .route(
            "/api/v4/channels/{channel_id}/groups",
            partially_migrated_with_ids(state, get(local_get_groups_by_channel)),
        )
        // `post_local.go:15`.
        .route(
            "/api/v4/channels/{channel_id}/posts",
            partially_migrated_with_ids(state, get(local_get_posts_for_channel)),
        )
        .route(
            "/api/v4/teams/{team_id}/channels",
            partially_migrated_with_ids(state, get(local_get_public_channels_for_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/channels/deleted",
            partially_migrated_with_ids(state, get(local_get_deleted_channels_for_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/channels/private",
            partially_migrated_with_ids(state, get(local_get_private_channels_for_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/channels/name/{channel_name}",
            partially_migrated_with_ids(state, get(local_get_channel_by_name)),
        )
        .route(
            "/api/v4/teams/name/{team_name}/channels/name/{channel_name}",
            partially_migrated_with_ids(state, get(local_get_channel_by_name_for_team_name)),
        )
        // `group_local.go:14`.
        .route(
            "/api/v4/teams/{team_id}/groups",
            partially_migrated_with_ids(state, get(local_get_groups_by_team)),
        )
        // `post_local.go:14` and `:16`.
        .route(
            "/api/v4/posts/{post_id}",
            partially_migrated_with_ids(state, get(local_get_post).delete(local_delete_post)),
        )
}

// ---------------------------------------------------------------------------------------------
// The eleven shared handlers
// ---------------------------------------------------------------------------------------------

/// `getAllChannels` through `APILocal` (channel_local.go:16).
async fn local_get_all_channels(
    state: State<AppState>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    channels::get_all_channels(state, query, local_session()).await
}

/// `getChannel` through `APILocal` (channel_local.go:18).
///
/// The handler forwards `?as_content_reviewer=true` over the port. Decided here first, so that
/// branch is dead on this router and the forward is over the socket.
async fn local_get_channel(
    state: State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    if channels::is_content_reviewer_request(request.uri().query()) {
        return forward_over_unix(&go.0, request).await;
    }
    channels::get_channel(state, path, local_session(), request).await
}

/// `getChannelByName` through `APILocal` (channel_local.go:19 and :34).
///
/// `{channel_name:[A-Za-z0-9_-]+}` is not an id class, so [`partially_migrated_with_ids`] does
/// not test it; the handler does, with a port forward behind it. Tested here first — a mux miss
/// is Go's own 404, over the socket.
async fn local_get_channel_by_name(
    state: State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    path: UrlPath<(String, String)>,
    query: RawQuery,
    request: Request,
) -> Response {
    if !channels::segment_matches_channel_name_mux(&path.0.1) {
        return forward_over_unix(&go.0, request).await;
    }
    channels::get_channel_by_name(state, path, query, local_session(), request).await
}

/// `getChannelByNameForTeamName` through `APILocal` (channel_local.go:35). Two name segments,
/// both outside the id class — see [`local_get_channel_by_name`].
async fn local_get_channel_by_name_for_team_name(
    state: State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    path: UrlPath<(String, String)>,
    query: RawQuery,
    request: Request,
) -> Response {
    if !crate::teams::segment_matches_team_name_mux(&path.0.0)
        || !channels::segment_matches_channel_name_mux(&path.0.1)
    {
        return forward_over_unix(&go.0, request).await;
    }
    channels::get_channel_by_name_for_team_name(state, path, query, local_session(), request).await
}

/// `getChannelMember` through `APILocal` (channel_local.go:27).
///
/// `me` in `{user_id}` rewrites to the session's empty user id and is the 400 naming `user_id`,
/// as on the other local families.
async fn local_get_channel_member(
    state: State<AppState>,
    path: UrlPath<(String, String)>,
) -> Result<Response, ApiError> {
    channels::get_channel_member(state, path, local_session()).await
}

/// `getChannelMembers` through `APILocal` (channel_local.go:29).
///
/// `SanitizeForCurrentUser` compares every row against the empty user id, so **every** member's
/// two timestamps are blanked — there is no caller's own row to leave intact.
async fn local_get_channel_members(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    channels::get_channel_members(state, path, query, local_session()).await
}

/// `getPublicChannelsForTeam` through `APILocal` (channel_local.go:31).
async fn local_get_public_channels_for_team(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    channels::get_public_channels_for_team(state, path, query, local_session()).await
}

/// `getDeletedChannelsForTeam` through `APILocal` (channel_local.go:32).
///
/// `manage_system` is asked as a question here, and the local session answers yes — so the
/// store's team-membership filter is skipped and the socket sees every archived channel on the
/// team, private ones included.
async fn local_get_deleted_channels_for_team(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    channels::get_deleted_channels_for_team(state, path, query, local_session()).await
}

/// `getPrivateChannelsForTeam` through `APILocal` (channel_local.go:33).
async fn local_get_private_channels_for_team(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    channels::get_private_channels_for_team(state, path, query, local_session()).await
}

/// `getPost` through `APILocal` (post_local.go:14).
///
/// The handler's decision is taken as a value so that its `Forward` can go over the socket.
async fn local_get_post(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(post_id): UrlPath<String>,
    request: Request,
) -> Response {
    match posts::get_post_outcome(
        &state,
        &post_id,
        &local_session(),
        request.uri().query(),
        request.headers(),
    )
    .await
    {
        posts::Outcome::Served(response) => response,
        posts::Outcome::Failed(err) => err.into_response(),
        posts::Outcome::Forward => forward_over_unix(&go.0, request).await,
    }
}

/// `getPostsForChannel` through `APILocal` (post_local.go:15) — see [`local_get_post`].
async fn local_get_posts_for_channel(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(channel_id): UrlPath<String>,
    request: Request,
) -> Response {
    match posts::get_posts_for_channel_outcome(
        &state,
        &channel_id,
        &local_session(),
        request.uri().query(),
        request.headers(),
    )
    .await
    {
        posts::Outcome::Served(response) => response,
        posts::Outcome::Failed(err) => err.into_response(),
        posts::Outcome::Forward => forward_over_unix(&go.0, request).await,
    }
}

// ---------------------------------------------------------------------------------------------
// group_local.go
// ---------------------------------------------------------------------------------------------

/// Port of `getGroupsByChannelLocal` (group_local.go:17).
///
/// Its HTTP twin runs `requireLicense` **first** — 501 `api.license_error` before the id is
/// looked at. The local one does not: `RequireChannelId` is the first check, and an unlicensed
/// installation is then `getGroupsByChannelCommon`'s own refusal, **403
/// `api.ldap_groups.license_error`**. So the same unlicensed socket call is a 403 here and a 501
/// over the port. A licensed installation is handed to Go whole, as on the HTTP router.
async fn local_get_groups_by_channel(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(channel_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = channels::require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    local_groups_common(&state, &go, request, "Api4.getGroupsByChannel").await
}

/// Port of `getGroupsByTeamLocal` (group_local.go:32) — see [`local_get_groups_by_channel`].
async fn local_get_groups_by_team(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(team_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = channels::require_id(&team_id, "team_id") {
        return err.into_response();
    }
    local_groups_common(&state, &go, request, "Api4.getGroupsByTeam").await
}

/// The licence line shared by `getGroupsByChannelCommon` and `getGroupsByTeamCommon`
/// (group.go:923, :991): `License() == nil || !Features.LDAPGroups` is the 403.
async fn local_groups_common(
    state: &AppState,
    go: &GoLocalSocket,
    request: Request,
    where_: &'static str,
) -> Response {
    match channel_writes::licensed(state).await {
        Ok(true) => forward_over_unix(&go.0, request).await,
        Ok(false) => ApiError::from(AppError::new(
            where_,
            "api.ldap_groups.license_error",
            None,
            String::new(),
            403,
        ))
        .into_response(),
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// channel_local.go — the eight handlers with their own logic
// ---------------------------------------------------------------------------------------------

/// Port of `localCreateChannel` (channel_local.go:38).
///
/// Decode (400 naming `channel`, a `null` included) and `CreateChannel(channel, false)`. None
/// of `createChannel`'s own checks run — no board/space refusal at this layer, no `team_id` /
/// `display_name` 400s, no create permission, no discoverability block — so an empty `team_id`
/// is the **store's** `model.channel.is_valid.team_id.app_error`, not the handler's
/// `invalid_body_param`. `addMember` is false: the channel has no creator and no members.
async fn local_create_channel(State(state): State<AppState>, request: Request) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("channel").into_response();
        }
    };
    let mut channel: Channel = match decode_one_from_json::<Option<Channel>>(&bytes) {
        Ok(Some(channel)) => channel,
        Ok(None) => return ApiError::invalid_param("channel").into_response(),
        Err(err) => {
            tracing::debug!(error = %err, "channel body did not decode");
            return ApiError::invalid_param("channel").into_response();
        }
    };

    if let Err(err) = state.app.create_channel(&mut channel, false).await {
        return ApiError::from(err).into_response();
    }
    match channel_creates::created("localCreateChannel", &channel) {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localDeleteChannel` (channel_local.go:404).
///
/// The channel first (404), then the DM/GM refusal (400 `api.channel.delete_channel.type.invalid`),
/// then the delete — with no permission between them and no `EnableAPIChannelDeletion` gate on
/// `?permanent`. `DeleteChannel(channel, "")`: the archive notice is the system bot's.
/// `?permanent=true` is forwarded once the two checks have passed ([D-610]).
///
/// [D-610]: ../../../../docs/TECH_DEBT.md
async fn local_delete_channel(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(channel_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = channels::require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    let permanent = channels::query_flag_is_true(request.uri().query(), "permanent");

    let channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP {
        return ApiError::from(AppError::new(
            "localDeleteChannel",
            "api.channel.delete_channel.type.invalid",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    if permanent {
        tracing::debug!("handing a permanent channel deletion to Go over the socket");
        return forward_over_unix(&go.0, request).await;
    }
    match state.app.delete_channel(&channel, "").await {
        Ok(_) => channel_writes::status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `localPatchChannel` (channel_local.go:296).
///
/// Id, body (400 naming `channel`), channel (404), then `channel.Patch(patch)` and
/// `UpdateChannel` — no "no changes" 400, no per-type permission, no group-constrained sweep,
/// no managed-category or banner handling. An empty patch is therefore a **200** that rewrites
/// the row unchanged. The store's `IsValid` is the only refusal after the fetch: an archived
/// channel is `app.channel.update.bad_id`, a blank display name the model's own error.
///
/// A licensed installation is forwarded, as `patchChannel` is: `UpdateChannel`'s ABAC
/// type-conversion block is decided by the licence.
async fn local_patch_channel(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(channel_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = channels::require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("channel").into_response();
        }
    };
    let patch: ChannelPatch = match decode_one_from_json::<Option<ChannelPatch>>(&bytes) {
        Ok(Some(patch)) => patch,
        Ok(None) => return ApiError::invalid_param("channel").into_response(),
        Err(err) => {
            tracing::debug!(error = %err, "channel patch body did not decode");
            return ApiError::invalid_param("channel").into_response();
        }
    };

    let mut channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };

    match channel_writes::licensed(&state).await {
        Ok(true) => {
            let request = Request::from_parts(parts, Body::from(bytes));
            return forward_over_unix(&go.0, request).await;
        }
        Ok(false) => {}
        Err(err) => return err.into_response(),
    }

    channel.patch(&patch);
    if let Err(err) = state.app.update_channel(&mut channel).await {
        return ApiError::from(err).into_response();
    }
    if let Err(err) = state.app.fill_in_channel_props(&mut channel).await {
        return ApiError::from(err).into_response();
    }
    match channel_writes::channel_response("localPatchChannel", &channel) {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localMoveChannel` (channel_local.go:339).
///
/// `moveChannel`'s order without its two user steps: channel (404), `team_id` (400), `force`
/// (400), team (404), DM/GM (**403** `api.channel.move_channel.type.invalid`), then the
/// deactivated-member sweep, the forced sweep with a **nil remover**, and `MoveChannel(team,
/// channel, nil)` — which posts nothing. A member the sweep cannot remove here (a guest, a shared
/// channel) forwards the whole request over the socket.
async fn local_move_channel(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(channel_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = channels::require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    let mut channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let props = channel_move::string_interface_from_json(&bytes);
    let Some(team_id) = props.get("team_id").and_then(|v| v.as_str()) else {
        return ApiError::invalid_param("team_id").into_response();
    };
    let Some(force) = props.get("force").and_then(|v| v.as_bool()) else {
        return ApiError::invalid_param("force").into_response();
    };

    let team = match state.app.get_team(team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP {
        return ApiError::from(AppError::new(
            "moveChannel",
            "api.channel.move_channel.type.invalid",
            None,
            String::new(),
            403,
        ))
        .into_response();
    }

    if let Err(err) = state
        .app
        .remove_all_deactivated_members_from_channel(&channel)
        .await
    {
        return ApiError::from(err).into_response();
    }

    let forward = |why: &'static str| {
        tracing::debug!(
            reason = why,
            "handing a local channel move to Go over the socket"
        );
        Request::from_parts(parts, Body::from(bytes))
    };

    if force {
        match state
            .app
            .remove_users_from_channel_not_member_of_team(None, &channel, &team)
            .await
        {
            Ok(MemberWrite::Done(())) => {}
            Ok(MemberWrite::Forward(why)) => return forward_over_unix(&go.0, forward(why)).await,
            Err(err) => return ApiError::from(err).into_response(),
        }
    }

    match state.app.move_channel(&team, &mut channel, None).await {
        Ok(MemberWrite::Done(())) => {}
        Ok(MemberWrite::Forward(why)) => return forward_over_unix(&go.0, forward(why)).await,
        Err(err) => return ApiError::from(err).into_response(),
    }

    match channel_writes::channel_response("moveChannel", &channel) {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localUpdateChannelPrivacy` (channel_local.go:71).
///
/// `privacy` (400) before the channel (404), the default channel's refusal, then
/// `UpdateChannelPrivacy(channel, nil)`: the conversion notice is posted by the **system bot**,
/// and a bot that cannot be fetched fails the write the way a failed post does — the type is
/// reverted and the answer is 500 `api.channel.post_channel_privacy_message.error`. No
/// convert permission is consulted. Licensed installations are forwarded, as
/// `updateChannelPrivacy` forwards them.
async fn local_update_channel_privacy(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(channel_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = channels::require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("privacy").into_response();
        }
    };
    let Some(privacy) = channel_writes::requested_privacy(&bytes) else {
        return ApiError::invalid_param("privacy").into_response();
    };

    let mut channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };

    match channel_writes::licensed(&state).await {
        Ok(true) => {
            let request = Request::from_parts(parts, Body::from(bytes));
            return forward_over_unix(&go.0, request).await;
        }
        Ok(false) => {}
        Err(err) => return err.into_response(),
    }

    if channel.name == DEFAULT_CHANNEL_NAME && privacy == CHANNEL_TYPE_PRIVATE {
        return ApiError::from(AppError::new(
            "updateChannelPrivacy",
            "api.channel.update_channel_privacy.default_channel_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }
    channel.channel_type = privacy.to_owned();

    match state.app.update_channel_privacy(&mut channel, None).await {
        Ok(ChannelWrite::Done) => {
            match channel_writes::channel_response("updateChannelPrivacy", &channel) {
                Ok(response) => response,
                Err(err) => err.into_response(),
            }
        }
        Ok(ChannelWrite::Forward(why)) => {
            tracing::debug!(
                reason = why,
                "handing a local privacy change to Go over the socket"
            );
            forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `localRestoreChannel` (channel_local.go:115).
///
/// Channel (404), `RestoreChannel(channel, "")`, the channel back. No team or console
/// permission; the unarchive notice is the system bot's.
async fn local_restore_channel(
    State(state): State<AppState>,
    UrlPath(channel_id): UrlPath<String>,
) -> Response {
    if let Err(err) = channels::require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    let mut channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Err(err) = state.app.restore_channel(&mut channel, "").await {
        return ApiError::from(err).into_response();
    }
    match channel_writes::channel_response("restoreChannel", &channel) {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localRemoveChannelMember` (channel_local.go:246).
///
/// `removeChannelMember` minus the self-removal cases: channel (404), user (404), the type
/// refusal (400), and the group-constrained refusal for any non-bot — there is no "removing
/// myself" on a session with no user. `RemoveUserFromChannel(userId, "", channel)`: the removal
/// notice is the system bot's, and the websocket events carry an empty `remover_id`.
async fn local_remove_channel_member(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath((channel_id, user_id)): UrlPath<(String, String)>,
    request: Request,
) -> Response {
    if let Err(err) = crate::channel_member_writes::require_channel_and_user(&channel_id, &user_id)
    {
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
    if channel.is_group_constrained() && !user.is_bot {
        return ApiError::from(AppError::new(
            "removeChannelMember",
            "api.channel.remove_member.group_constrained.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    match state
        .app
        .remove_user_from_channel(&user_id, "", &channel)
        .await
    {
        Ok(MemberWrite::Done(())) => channel_writes::status_ok(),
        Ok(MemberWrite::Forward(why)) => {
            tracing::debug!(
                reason = why,
                "handing a local member removal to Go over the socket"
            );
            forward_over_unix(&go.0, request).await
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `localAddChannelMember` (channel_local.go:145).
///
/// One user, not a list: `user_id` (400 naming **`user_id`**, where the HTTP handler names
/// `user_id or user_ids`), then `post_root_id` — a non-empty invalid one is its 400, a valid one
/// is fetched (`GetSinglePost`'s 404) and must belong to the channel (the same 400) — then the
/// channel (404), the DM/GM refusal, and the group-constrained filter, which is forwarded here
/// as on the HTTP router. `AddChannelMember` runs with **no requestor**, so the notice is "X
/// joined the channel" posted by X, and `post_root_id` reaches nothing further: Go only uses it
/// for the requestor's "added by" post. A user who is already a member is answered with the
/// existing row, 201.
///
/// The member is encoded **unsanitised** — `SanitizeForCurrentUser` is the HTTP handler's, not
/// this one's — so `last_viewed_at` and `last_update_at` are the row's real values.
async fn local_add_channel_member(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(channel_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = channels::require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let props = channel_move::string_interface_from_json(&bytes);

    let Some(user_id) = props
        .get("user_id")
        .and_then(|v| v.as_str())
        .filter(|id| is_valid_id(id))
    else {
        return ApiError::invalid_param("user_id").into_response();
    };

    // `props["post_root_id"].(string)`: absent or not a string is `ok == false`.
    let post_root_id = props
        .get("post_root_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !post_root_id.is_empty() && !is_valid_id(post_root_id) {
        return ApiError::invalid_param("post_root_id").into_response();
    }
    if post_root_id.len() == 26 {
        let root = match state.app.get_single_post(post_root_id, false).await {
            Ok(root) => root,
            Err(err) => return ApiError::from(err).into_response(),
        };
        if root.channel_id != channel_id {
            return ApiError::invalid_param("post_root_id").into_response();
        }
    }

    let channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP {
        return ApiError::from(AppError::new(
            "localAddChannelMember",
            "api.channel.add_user_to_channel.type.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    let forward = |why: &'static str| {
        tracing::debug!(
            reason = why,
            "handing a local member add to Go over the socket"
        );
        Request::from_parts(parts, Body::from(bytes))
    };
    if channel.is_group_constrained() {
        return forward_over_unix(
            &go.0,
            forward("FilterNonGroupChannelMembers needs the group syncable store"),
        )
        .await;
    }

    let opts = ChannelMemberOpts {
        user_requestor_id: String::new(),
        post_root_id: post_root_id.to_owned(),
        skip_team_member_integrity_check: false,
    };
    let member = match state.app.add_channel_member(user_id, &channel, &opts).await {
        Ok(MemberWrite::Done(member)) => member,
        Ok(MemberWrite::Forward(why)) => return forward_over_unix(&go.0, forward(why)).await,
        Err(err) => return ApiError::from(err).into_response(),
    };

    // `json.NewEncoder(w).Encode(cm)` — Go's marshal and its trailing newline.
    match go_json_marshal(&member) {
        Ok(json) => (
            StatusCode::CREATED,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            json + "\n",
        )
            .into_response(),
        Err(err) => {
            tracing::warn!(error = %err, "Error while writing response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// post_local.go
// ---------------------------------------------------------------------------------------------

/// Port of `localDeletePost` (post_local.go:19).
///
/// `deletePost` with its three gates removed — `EnableAPIPostDeletion`, `manage_system` for a
/// permanent delete, and the own/others `delete_post` permission — so the post is fetched
/// (with `includeDeleted = permanent`: an archived post can be removed for good, and is the 404
/// `app.post.get.app_error` otherwise) and deleted with `DeleteBy == ""`. The shapes the app
/// layer does not reproduce — a card, a restricted DM, a post with files under `?permanent` —
/// are forwarded over the socket.
async fn local_delete_post(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(post_id): UrlPath<String>,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }
    let permanent = channels::query_flag_is_true(request.uri().query(), "permanent");

    if let Err(err) = state.app.get_single_post(&post_id, permanent).await {
        return ApiError::from(err).into_response();
    }

    let outcome = if permanent {
        state.app.permanent_delete_post(&post_id, "").await
    } else {
        state.app.delete_post(&post_id, "").await.map(|_deleted| ())
    };
    match outcome {
        Ok(()) => channel_writes::status_ok(),
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::debug!(
                reason = why,
                "handing a local post delete to Go over the socket"
            );
            forward_over_unix(&go.0, request).await
        }
    }
}

#[cfg(test)]
mod tests {
    /// Every forward in this module must be over the socket. A `proxy::forward_to_go` here would
    /// answer a local request through `APISessionRequired`, which is the one wrong transport this
    /// router exists to avoid — see the module docs.
    #[test]
    fn nothing_in_this_module_forwards_over_the_port() {
        let source = include_str!("local_channels.rs");
        let (docs, code) = source.split_at(source.find("\nuse ").expect("a use line"));
        assert!(
            docs.contains("proxy::forward_to_go"),
            "the docs name the thing"
        );
        // Assembled so this literal is not itself the match.
        let port_forward = concat!("forward_to_", "go(");
        assert!(
            !code.contains(port_forward),
            "a local handler forwarded over the port"
        );
    }
}
