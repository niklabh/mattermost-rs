//! The local-mode registrations of `team_local.go`, `webhook_local.go` and `command_local.go`:
//! thirty route+method pairs on the unix socket, behind [`crate::local::router`].
//!
//! Twenty-four of the thirty are the HTTP handlers unchanged, handed [`local_session`] — the same
//! wiring as the families already in `local.rs`. The other six are the `local*`-named Go
//! functions, each a *different* function from its HTTP namesake, ported here line by line:
//!
//! | Go | differs from the HTTP handler in |
//! |---|---|
//! | `localCreateTeam` | no creator: `App.CreateTeam`, not `CreateTeamWithUser`; the invite id is never blanked |
//! | `localDeleteTeam` | no permission check, no `EnableAPITeamDeletion` gate — `?permanent=true` **deletes** |
//! | `localInviteUsersToTeam` | no permissions; its own email validation; the sender is `"mmctl <id>"` |
//! | `localCreateIncomingHook` | `user_id` comes from the body and is **required**; no channel-lock forcing |
//! | `localCreateOutgoingHook` | `creator_id` comes from the body and is required |
//! | `localCreateCommand` | `creator_id` is whatever the body says, permission-free |
//!
//! # A shared handler that would forward over the port is pre-empted here
//!
//! Several HTTP handlers hand a case to Go through [`crate::proxy::forward_to_go`] — the *port*,
//! whose handler chain is `APISessionRequired`. On this router that answer would be a 401 where
//! Go's socket answers the request. So every such condition is tested **before** the shared
//! handler runs, and forwarded over the socket instead: the ABAC listing (`getAllTeams`,
//! `searchTeams`), the content-reviewer flag (`getTeam`), a `team_name` outside the mux class
//! (`getTeamByName`), a patch that turns on group-constraint (`patchTeam`), a member add on a
//! group-constrained team (`addTeamMember`), the built-in command list (`listCommands`) and the
//! permanent team delete. Each pre-check is a superset of the handler's own condition, which is
//! safe because nothing has been written when it fires.
//!
//! # What the empty user id changes
//!
//! `Session().UserId` is `""` on the socket. Three handlers read it in a way that matters:
//!
//! - `getTeamMembersByIds` asks `GetViewUsersRestrictions("")`, which is *restricted* to the
//!   teams and channels of user `""` — none — so the store adds `WHERE 1 = 0` and the answer is
//!   **`[]` for any ids**, measured. See [`local_get_team_members_by_ids`].
//! - `removeTeamMember` and `addTeamMember` compare the body's or path's user id to `""`; both
//!   validate that id first, so the self-service arms are unreachable and `me` is a 400.
//! - The hook and command writes compare the owner to `""` and then consult a
//!   `manage_others_*` permission, which the local session holds — so ownership never refuses.
//!
//! # `GET /teams/name/{team_name}` is not shadowed on this router
//!
//! On the HTTP router `stats`, `image` and `members` under `/teams/name/` are answered by the
//! `{team_id}` subrouter (`teams::TEAM_BY_NAME_SHADOWED_LITERALS`). `InitTeamLocal` registers no
//! `GET` literal under `BaseRoutes.Team`, so on the socket those three reach `getTeamByName`
//! like any other name — a 404 `app.team.get_by_name.missing` for each, measured. That is why
//! the local wrapper calls `teams::serve_team_by_name` rather than the HTTP handler.

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, RawQuery, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use mm_model::command::Command;
use mm_model::incoming_webhook::IncomingWebhook;
use mm_model::member_invite::MemberInvite;
use mm_model::outgoing_webhook::OutgoingWebhook;
use mm_model::team::{Team, TeamPatch};
use mm_model::utils::{AppError, decode_one_from_json, go_to_lower, is_valid_email, is_valid_id};

use crate::channels::{
    ids_from_body, is_content_reviewer_request, query_first, query_flag_is_true, read_body,
    require_id,
};
use crate::error::ApiError;
use crate::local::{
    GoLocalSocket, forward_over_unix, local_session, partially_migrated,
    partially_migrated_with_ids,
};
use crate::{AppState, commands, team_member_writes, teams, webhooks};

/// The thirty pairs, as one router to `.merge` into [`crate::local::router`].
///
/// Literal segments beside a parameter on the same router — `/teams/search` and
/// `/teams/name/{team_name}` beside `/teams/{team_id}`, `/members/ids` beside
/// `/members/{user_id}` — resolve the axum way (the literal wins). Every case where gorilla would
/// have resolved differently is a method the literal does not carry, which the method fallback
/// forwards to Go: `GET /teams/search` is Go's own `getTeam("search")` 400, exactly as on the
/// HTTP router.
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        // ---- `team_local.go` (14 pairs).
        .route(
            "/api/v4/teams",
            partially_migrated(get(local_get_all_teams).post(local_create_team)),
        )
        .route(
            "/api/v4/teams/search",
            partially_migrated(post(local_search_teams)),
        )
        // `team_name` is not id-shaped, so the id middleware leaves it alone and the wrapper
        // carries the `[A-Za-z0-9_-]+` class itself.
        .route(
            "/api/v4/teams/name/{team_name}",
            partially_migrated(get(local_get_team_by_name)),
        )
        .route(
            "/api/v4/teams/{team_id}",
            partially_migrated_with_ids(
                state,
                get(local_get_team)
                    .put(local_update_team)
                    .delete(local_delete_team),
            ),
        )
        .route(
            "/api/v4/teams/{team_id}/invite/email",
            partially_migrated_with_ids(state, post(local_invite_users_to_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/patch",
            partially_migrated_with_ids(state, put(local_patch_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/privacy",
            partially_migrated_with_ids(state, put(local_update_team_privacy)),
        )
        .route(
            "/api/v4/teams/{team_id}/restore",
            partially_migrated_with_ids(state, post(local_restore_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/members",
            partially_migrated_with_ids(state, post(local_add_team_member)),
        )
        .route(
            "/api/v4/teams/{team_id}/members/ids",
            partially_migrated_with_ids(state, post(local_get_team_members_by_ids)),
        )
        .route(
            "/api/v4/teams/{team_id}/members/{user_id}",
            partially_migrated_with_ids(state, delete(local_remove_team_member)),
        )
        // ---- `webhook_local.go` (10 pairs).
        .route(
            "/api/v4/hooks/incoming",
            partially_migrated(get(local_get_incoming_hooks).post(local_create_incoming_hook)),
        )
        .route(
            "/api/v4/hooks/incoming/{hook_id}",
            partially_migrated_with_ids(
                state,
                get(local_get_incoming_hook)
                    .put(local_update_incoming_hook)
                    .delete(local_delete_incoming_hook),
            ),
        )
        .route(
            "/api/v4/hooks/outgoing",
            partially_migrated(get(local_get_outgoing_hooks).post(local_create_outgoing_hook)),
        )
        .route(
            "/api/v4/hooks/outgoing/{hook_id}",
            partially_migrated_with_ids(
                state,
                get(local_get_outgoing_hook)
                    .put(local_update_outgoing_hook)
                    .delete(local_delete_outgoing_hook),
            ),
        )
        // ---- `command_local.go` (6 pairs).
        .route(
            "/api/v4/commands",
            partially_migrated(get(local_list_commands).post(local_create_command)),
        )
        .route(
            "/api/v4/commands/{command_id}",
            partially_migrated_with_ids(
                state,
                get(local_get_command)
                    .put(local_update_command)
                    .delete(local_delete_command),
            ),
        )
        .route(
            "/api/v4/commands/{command_id}/move",
            partially_migrated_with_ids(state, put(local_move_command)),
        )
}

/// `ReturnStatusOK` — `{"status":"OK"}`, `w.Write`, no trailing newline.
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

/// `w.WriteHeader(http.StatusCreated)` then `json.NewEncoder(w).Encode` — the 201 with the
/// encoder's trailing newline, through Go's escaping (`<`, `>`, `&`).
fn created_json<T: serde::Serialize>(value: &T, where_: &'static str) -> Response {
    match mm_model::utils::go_json_marshal(value) {
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
            ApiError::from(AppError::new(
                where_,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// `json.NewDecoder(r.Body).Decode(&v)` into a zero-valued struct: one value read off the
/// body, `null` a no-op — so `null` is the **zero struct**, not a decode error — and trailing
/// bytes ignored. Every `local*` create in these three files decodes this way, and the failure is
/// `SetInvalidParamWithErr(parameter)`.
async fn decode_body<T: serde::de::DeserializeOwned + Default>(
    request: Request,
    parameter: &str,
) -> Result<T, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            ApiError::invalid_param(parameter)
        })?;
    decode_one_from_json::<Option<T>>(&bytes)
        .map(Option::unwrap_or_default)
        .map_err(|err| {
            tracing::debug!(error = %err, "{parameter} body did not decode");
            ApiError::invalid_param(parameter)
        })
}

// ---------------------------------------------------------------------------------------------
// team_local.go
// ---------------------------------------------------------------------------------------------

/// `getAllTeams` through `APILocal` (team_local.go:22).
///
/// The handler's ABAC arm forwards over the port; it is decided here first so the forward goes
/// over the socket. Both list permissions pass for the local session, so this is the
/// both-permissions arm of `getAllTeams`'s matrix — unsanitised, with `invite_id` and `email`.
async fn local_get_all_teams(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Result<Response, ApiError> {
    if state.app.team_membership_access_control_enabled().await? {
        tracing::debug!("handing an attribute-based local team listing to Go over the socket");
        return Ok(forward_over_unix(&go.0, request).await);
    }
    teams::get_all_teams(State(state), local_session(), request).await
}

/// `searchTeams` through `APILocal` (team_local.go:23).
///
/// `APILocal` carries no `DisableWhenBusy`, but the HTTP handler's busy check is the first thing
/// it does, and it *is* observable: the local session is unrestricted, and Go's busy refusal is
/// keyed on `IsBusy()` alone. The ABAC forward is pre-empted as in [`local_get_all_teams`].
async fn local_search_teams(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    if let Err(err) = crate::system::refuse_when_busy() {
        return err.into_response();
    }
    match state.app.team_membership_access_control_enabled().await {
        Ok(true) => return forward_over_unix(&go.0, request).await,
        Ok(false) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }
    teams::search_teams(State(state), local_session(), request).await
}

/// `getTeam` through `APILocal` (team_local.go:25). The content-reviewer flag is the handler's
/// port forward, pre-empted here.
async fn local_get_team(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    if is_content_reviewer_request(request.uri().query()) {
        return forward_over_unix(&go.0, request).await;
    }
    teams::get_team(State(state), path, local_session(), request).await
}

/// `updateTeam` through `APILocal` (team_local.go:26).
async fn local_update_team(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    teams::update_team(state, local_session(), path, request).await
}

/// `patchTeam` through `APILocal` (team_local.go:29).
///
/// A patch that turns on `group_constrained` is the handler's port forward (`TeamWriteBlocked::
/// GroupConstrained`); the body is decoded once here to route it over the socket, then handed
/// on intact. A body that does not decode falls through to the handler's own 400.
async fn local_patch_team(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("team").into_response();
        }
    };
    let request = Request::from_parts(parts, Body::from(bytes.clone()));
    if serde_json::from_slice::<TeamPatch>(&bytes)
        .is_ok_and(|patch| patch.group_constrained == Some(true))
    {
        return forward_over_unix(&go.0, request).await;
    }
    teams::patch_team(State(state), local_session(), path, request).await
}

/// `updateTeamPrivacy` through `APILocal` (team_local.go:30).
async fn local_update_team_privacy(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    teams::update_team_privacy(state, local_session(), path, request).await
}

/// `restoreTeam` through `APILocal` (team_local.go:31).
async fn local_restore_team(state: State<AppState>, path: UrlPath<String>) -> Response {
    teams::restore_team(state, local_session(), path).await
}

/// `getTeamByName` through `APILocal` (team_local.go:33) — see the module docs for why this is
/// `serve_team_by_name` and not the HTTP handler. A segment outside `[A-Za-z0-9_-]+` is Go's
/// mux 404, over the socket.
async fn local_get_team_by_name(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(team_name): UrlPath<String>,
    request: Request,
) -> Response {
    if !teams::segment_matches_team_name_mux(&team_name) {
        return forward_over_unix(&go.0, request).await;
    }
    teams::serve_team_by_name(&state, team_name, &local_session()).await
}

/// `addTeamMember` through `APILocal` (team_local.go:34).
///
/// A group-constrained team is the handler's port forward (`FilterNonGroupTeamMembers` is
/// unported); the team is read here first so that case goes over the socket. A team that cannot
/// be read is left to the handler, whose own lookup produces the 404 in Go's order.
async fn local_add_team_member(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(team_id): UrlPath<String>,
    request: Request,
) -> Response {
    if is_valid_id(&team_id)
        && state
            .app
            .get_team(&team_id)
            .await
            .is_ok_and(|team| team.is_group_constrained())
    {
        return forward_over_unix(&go.0, request).await;
    }
    team_member_writes::add_team_member(State(state), UrlPath(team_id), local_session(), request)
        .await
}

/// `getTeamMembersByIds` through `APILocal` (team_local.go:35) — and the one read in these
/// three files whose local answer is **not** the HTTP handler's.
///
/// The handler runs `GetViewUsersRestrictions(session.UserId)`. For the empty local user id
/// that is: `HasPermissionTo("")` false (there is no such user), then the teams of user `""`
/// and the channels of user `""` — both empty — so the restriction is `{Teams: [], Channels:
/// []}`, which `applyTeamMemberViewRestrictionsFilter` turns into `WHERE 1 = 0`. Measured: the
/// Go socket answers `[]` for a real member's id. The HTTP port of that handler forwards a
/// restricted caller because the lists are unknown; here they are known, and empty, so the
/// query is not worth running. Go's three refusals before it are reproduced in Go's order.
///
/// `json.Marshal` + `w.Write` — no trailing newline.
async fn local_get_team_members_by_ids(
    UrlPath(team_id): UrlPath<String>,
    request: Request,
) -> Result<Response, ApiError> {
    require_id(&team_id, "team_id")?;
    let bytes = read_body(request, "getTeamMembersByIds").await?;
    let user_ids = ids_from_body(&bytes, "user_ids", "getTeamMembersByIds")?;
    tracing::debug!(
        asked = user_ids.len(),
        "the local session's view restrictions are empty; answering no members"
    );
    // `view_team` passes for the local session; the restrictions are the empty pair.
    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        "[]",
    )
        .into_response())
}

/// `removeTeamMember` through `APILocal` (team_local.go:36). `me` is a 400 here — the handler
/// validates the path id before comparing it to the session's, which is `""`.
async fn local_remove_team_member(
    state: State<AppState>,
    path: UrlPath<(String, String)>,
) -> Response {
    team_member_writes::remove_team_member(state, path, local_session()).await
}

/// Port of `localCreateTeam` (team_local.go:293) — `POST /api/v4/teams` over the socket.
///
/// Three lines shorter than `createTeam` and each missing line is a difference on the wire:
///
/// - **No permission checks** — not `create_team`, not the `scheme_id` sysconsole permission,
///   not the `invite_user` check on `allow_open_invite`/`allowed_domains`.
/// - **`App.CreateTeam`, not `CreateTeamWithUser`.** Nobody joins the team, nobody becomes its
///   admin, and the body's `email` is *kept* (lower-cased) rather than overwritten with the
///   creator's — there is no creator. `IsTeamEmailAllowed` is not consulted; only
///   `Team.IsValid`'s own email rule applies.
/// - **The invite id is never blanked.** `createTeam` re-asks `creatorCanInviteUsersOnTeam` and
///   strips `invite_id` from the reply when it says no; here the 201 always carries it.
///
/// `w.WriteHeader(201)` then `json.NewEncoder` — a trailing newline.
#[tracing::instrument(skip_all, fields(team_id))]
async fn local_create_team(State(state): State<AppState>, request: Request) -> Response {
    let mut team: Team = match decode_body(request, "team").await {
        Ok(team) => team,
        Err(err) => return err.into_response(),
    };

    // `team.Email = strings.ToLower(team.Email)` — the simple mapping.
    team.email = go_to_lower(&team.email);

    match state.app.create_team(&mut team).await {
        Ok(created) => {
            tracing::Span::current().record("team_id", &created.id);
            created_json(&created, "localCreateTeam")
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `localDeleteTeam` (team_local.go:41) — `DELETE /api/v4/teams/{team_id}` over the
/// socket.
///
/// **No permission check and no `EnableAPITeamDeletion` gate.** `?permanent=true` calls
/// `PermanentDeleteTeamId` on any server, which is what `mmctl team delete --confirm` relies
/// on; the HTTP handler refuses that with a 401 unless the flag is on. `PermanentDeleteTeam`
/// itself — every channel's posts, members and hooks, then the memberships, the commands and
/// the row — is unported ([D-370]), so the permanent arm is forwarded over the socket **before
/// anything is written**; the archive arm is `SoftDeleteTeam`, shared with the HTTP route.
///
/// `strconv.ParseBool` with the error discarded, as on the HTTP route: `?permanent=yes`
/// archives. `ReturnStatusOK`, no newline.
#[tracing::instrument(skip_all, fields(team_id = %team_id, permanent))]
async fn local_delete_team(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(team_id): UrlPath<String>,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }
    let permanent = query_flag_is_true(request.uri().query(), "permanent");
    tracing::Span::current().record("permanent", permanent);

    if permanent {
        tracing::debug!("handing a permanent local team deletion to Go over the socket");
        return forward_over_unix(&go.0, request).await;
    }

    match state.app.soft_delete_team(&team_id).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// The refusals `localInviteUsersToTeam` makes on the decoded body, in Go's order
/// (team_local.go:76-93), lifted so each branch has a unit test.
///
/// 1. no emails — `SetInvalidParam("user_email")`;
/// 2. each email, **normalised first** (`strings.ToLower`), must pass `IsValidEmail` — the 400
///    `api.team.invite_members.invalid_email.app_error`, so `A@B.C` is accepted and `a@b`
///    refused, and the first bad address ends the loop;
/// 3. profiles without `?graceful=` — the 400 `profiles_graceful`. `len(Profiles) > 0`, so
///    `[null]` counts.
///
/// `graceful` is `r.URL.Query().Get("graceful") != ""`, the same presence-and-non-empty rule as
/// the HTTP route.
#[allow(clippy::result_large_err)]
fn local_invite_refusal(invite: &MemberInvite, graceful: bool) -> Result<(), ApiError> {
    if invite.emails.is_empty() {
        return Err(ApiError::invalid_param("user_email"));
    }
    for email in &invite.emails {
        let email = mm_model::user::normalize_email(email);
        if !is_valid_email(&email) {
            let mut params = std::collections::HashMap::new();
            params.insert("Address".to_owned(), serde_json::Value::String(email));
            return Err(ApiError::from(AppError::new(
                "localInviteUsersToTeam",
                "api.team.invite_members.invalid_email.app_error",
                Some(params),
                String::new(),
                400,
            )));
        }
    }
    if !graceful && invite.profiles.iter().flatten().next().is_some() {
        return Err(ApiError::from(AppError::new(
            "Api4.localInviteUsersToTeam",
            "api.team.invite_members.profiles_graceful.app_error",
            None,
            String::new(),
            400,
        )));
    }
    Ok(())
}

/// Port of `localInviteUsersToTeam` (team_local.go:66) — `POST /api/v4/teams/{team_id}/invite
/// /email` over the socket — up to the send.
///
/// Not `inviteUsersToTeam` with a local session: that one checks two team permissions, decodes,
/// and hands *everything else* to `InviteNewUsersToTeam`. This one checks no permission, has
/// **its own** gates, and then does the domain check, the deactivated-account check and the
/// send inline, as `"Administrator"` from `"mmctl <id>"`. The gates in order:
///
/// | # | check | answer |
/// |---|---|---|
/// | 1 | `team_id` is not an id | 400 `invalid_url_param` |
/// | 2 | `EnableEmailInvitations` off | **501** `api.team.invite_members.disabled.app_error` |
/// | 3 | the body does not decode | 400 `invite_members_to_team_and_channels.invalid_body` |
/// | 4-6 | [`local_invite_refusal`] | |
/// | 7 | the team does not exist | 404 `app.team.get.find.app_error` — a store read, not `GetTeam`, same id |
///
/// Everything past row 7 — `isEmailAddressAllowed` against the team's and the server's allowed
/// domains, `IsDeactivatedUserEmail`, `InviteNewUsersToTeamGracefullyForLocal` for profiles,
/// and the email service — is the send, which the HTTP route forwards too ([D-490]). It is
/// forwarded here over the socket, with the original body, and nothing has been written when
/// that happens. Row 2 is not observable on the stack (Go runs with the flag on), so its
/// parity rests on the config fixture rather than a request.
#[tracing::instrument(skip_all, fields(team_id = %team_id, graceful, forwarded = false))]
async fn local_invite_users_to_team(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(team_id): UrlPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    if !state.app.config().enable_email_invitations {
        return ApiError::from(AppError::new(
            "localInviteUsersToTeam",
            "api.team.invite_members.disabled.app_error",
            None,
            String::new(),
            501,
        ))
        .into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return invalid_local_invite_body().into_response();
        }
    };
    // `StructFromJSONLimited` is `json.NewDecoder(...).Decode(&obj)` — one value, `null` the
    // zero struct (the decoder nils its own copy of the pointer, not the caller's).
    let invite: MemberInvite = match decode_one_from_json::<Option<MemberInvite>>(&bytes) {
        Ok(invite) => invite.unwrap_or_default(),
        Err(err) => {
            tracing::debug!(error = %err, "member invite body did not decode");
            return invalid_local_invite_body().into_response();
        }
    };

    let graceful = query_first(query.as_deref(), "graceful").is_some_and(|v| !v.is_empty());
    tracing::Span::current().record("graceful", graceful);
    if let Err(err) = local_invite_refusal(&invite, graceful) {
        return err.into_response();
    }

    if let Err(err) = state.app.get_team(&team_id).await {
        return ApiError::from(err).into_response();
    }

    tracing::Span::current().record("forwarded", true);
    tracing::debug!("handing the invitation send to Go over the socket");
    forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await
}

/// Row 3 of [`local_invite_users_to_team`]'s table. `Api4.localInviteUsersToTeam`, singular —
/// the HTTP route's twin is `Api4.inviteUsersToTeams`.
fn invalid_local_invite_body() -> ApiError {
    ApiError::from(AppError::new(
        "Api4.localInviteUsersToTeam",
        "api.team.invite_members_to_team_and_channels.invalid_body.app_error",
        None,
        String::new(),
        400,
    ))
}

// ---------------------------------------------------------------------------------------------
// webhook_local.go
// ---------------------------------------------------------------------------------------------

/// `getIncomingHooks` through `APILocal` (webhook_local.go:16). The user filter is `""`
/// cleared by `manage_others_incoming_webhooks`, so every hook is listed.
async fn local_get_incoming_hooks(
    state: State<AppState>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    webhooks::get_incoming_hooks(state, query, local_session()).await
}

/// `getIncomingHook` through `APILocal` (webhook_local.go:17).
async fn local_get_incoming_hook(
    state: State<AppState>,
    path: UrlPath<String>,
) -> Result<Response, ApiError> {
    webhooks::get_incoming_hook(state, path, local_session()).await
}

/// `updateIncomingHook` through `APILocal` (webhook_local.go:18).
async fn local_update_incoming_hook(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    webhooks::update_incoming_hook(state, local_session(), path, request).await
}

/// `deleteIncomingHook` through `APILocal` (webhook_local.go:19).
async fn local_delete_incoming_hook(state: State<AppState>, path: UrlPath<String>) -> Response {
    webhooks::delete_incoming_hook(state, local_session(), path).await
}

/// `getOutgoingHooks` through `APILocal` (webhook_local.go:22).
async fn local_get_outgoing_hooks(
    state: State<AppState>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    webhooks::get_outgoing_hooks(state, query, local_session()).await
}

/// `getOutgoingHook` through `APILocal` (webhook_local.go:23).
async fn local_get_outgoing_hook(
    state: State<AppState>,
    path: UrlPath<String>,
) -> Result<Response, ApiError> {
    webhooks::get_outgoing_hook(state, path, local_session()).await
}

/// `updateOutgoingHook` through `APILocal` (webhook_local.go:24).
async fn local_update_outgoing_hook(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    webhooks::update_outgoing_hook(state, local_session(), path, request).await
}

/// `deleteOutgoingHook` through `APILocal` (webhook_local.go:25).
async fn local_delete_outgoing_hook(state: State<AppState>, path: UrlPath<String>) -> Response {
    webhooks::delete_outgoing_hook(state, local_session(), path).await
}

/// Port of `localCreateIncomingHook` (webhook_local.go:28) — `POST /api/v4/hooks/incoming`
/// over the socket.
///
/// `createIncomingHook` takes the owner from the session and needs a permission to name anyone
/// else; here there is no session user, so **`user_id` is required** — empty is the 400 naming
/// it, before any lookup — and it is checked only for existence (`GetUser`'s 404), not through
/// `validateIncomingWebhookUser`. Two more things the HTTP handler does are absent: the four
/// permission checks, and the channel-lock forcing — the hook is created exactly as posted,
/// `channel_locked` and all. The channel is still fetched **before** the user, so a bad
/// `channel_id` beats a bad `user_id`.
#[tracing::instrument(skip_all, fields(channel_id, user_id))]
async fn local_create_incoming_hook(State(state): State<AppState>, request: Request) -> Response {
    let hook: IncomingWebhook = match decode_body(request, "incoming_webhook").await {
        Ok(hook) => hook,
        Err(err) => return err.into_response(),
    };
    tracing::Span::current().record("channel_id", &hook.channel_id);
    tracing::Span::current().record("user_id", &hook.user_id);

    if hook.user_id.is_empty() {
        return ApiError::invalid_param("user_id").into_response();
    }

    let channel = match state.app.get_channel(&hook.channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if let Err(err) = state.app.get_user(&hook.user_id).await {
        return ApiError::from(err).into_response();
    }

    match state
        .app
        .create_incoming_webhook_for_channel(&hook.user_id, &channel, &hook)
        .await
    {
        Ok(saved) => webhooks::created_json(&saved),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `localCreateOutgoingHook` (webhook_local.go:66) — `POST /api/v4/hooks/outgoing`
/// over the socket.
///
/// The same shape as its incoming twin: **`creator_id` is required** (the 400 names it), the
/// user must exist (`GetUser`'s 404), and nothing is checked for permission. `App
/// .CreateOutgoingWebhook` then applies `IsValid`, the channel and team checks and the
/// trigger-uniqueness rule exactly as for the HTTP route.
#[tracing::instrument(skip_all, fields(team_id, creator_id))]
async fn local_create_outgoing_hook(State(state): State<AppState>, request: Request) -> Response {
    let hook: OutgoingWebhook = match decode_body(request, "outgoing_webhook").await {
        Ok(hook) => hook,
        Err(err) => return err.into_response(),
    };
    tracing::Span::current().record("team_id", &hook.team_id);
    tracing::Span::current().record("creator_id", &hook.creator_id);

    if hook.creator_id.is_empty() {
        return ApiError::invalid_param("creator_id").into_response();
    }

    if let Err(err) = state.app.get_user(&hook.creator_id).await {
        return ApiError::from(err).into_response();
    }

    match state.app.create_outgoing_webhook(&hook).await {
        Ok(saved) => webhooks::created_json(&saved),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// command_local.go
// ---------------------------------------------------------------------------------------------

/// `listCommands` through `APILocal` (command_local.go:17).
///
/// Without `custom_only` the handler answers from the built-in provider registry, which this
/// port does not have, and forwards over the port; that case is decided here first so it goes
/// over the socket. An empty `team_id` is left to the handler's own 400, as in Go's order.
async fn local_list_commands(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    let team_id = query_first(query.as_deref(), "team_id").unwrap_or_default();
    if !team_id.is_empty() && !query_flag_is_true(query.as_deref(), "custom_only") {
        return forward_over_unix(&go.0, request).await;
    }
    commands::list_commands(State(state), RawQuery(query), local_session(), request).await
}

/// `getCommand` through `APILocal` (command_local.go:19).
async fn local_get_command(
    state: State<AppState>,
    path: UrlPath<String>,
) -> Result<Response, ApiError> {
    commands::get_command(state, path, local_session()).await
}

/// `updateCommand` through `APILocal` (command_local.go:20).
async fn local_update_command(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    commands::update_command(state, path, local_session(), request).await
}

/// `moveCommand` through `APILocal` (command_local.go:21).
async fn local_move_command(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    commands::move_command(state, path, local_session(), request).await
}

/// `deleteCommand` through `APILocal` (command_local.go:22).
async fn local_delete_command(state: State<AppState>, path: UrlPath<String>) -> Response {
    commands::delete_command(state, path, local_session()).await
}

/// Port of `localCreateCommand` (command_local.go:25) — `POST /api/v4/commands` over the
/// socket.
///
/// `createCommand` checks `manage_own_slash_commands` on the body's team, then either takes the
/// session user as the creator or spends `manage_others_slash_commands` plus a `GetUser` to
/// honour the body's. Here **the body's `creator_id` is taken as-is** with no lookup at all, so
/// a made-up id reaches `App.CreateCommand` and is refused by `IsValid`
/// (`model.command.is_valid.user_id.app_error`) rather than by a 404 — and an empty one the
/// same way, where the HTTP route would have silently filled it.
#[tracing::instrument(skip_all, fields(team_id, trigger))]
async fn local_create_command(State(state): State<AppState>, request: Request) -> Response {
    let command: Command = match decode_body(request, "command").await {
        Ok(command) => command,
        Err(err) => return err.into_response(),
    };
    tracing::Span::current().record("team_id", &command.team_id);
    tracing::Span::current().record("trigger", &command.trigger);

    match state.app.create_command(command).await {
        Ok(created) => match commands::encoded(StatusCode::CREATED, &created, "localCreateCommand")
        {
            Ok(response) => response,
            Err(err) => err.into_response(),
        },
        Err(err) => ApiError::from(err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(emails: &[&str]) -> MemberInvite {
        MemberInvite {
            emails: emails.iter().map(|e| (*e).to_owned()).collect(),
            ..MemberInvite::default()
        }
    }

    fn id_of(result: Result<(), ApiError>) -> Option<(String, i32)> {
        result
            .err()
            .map(|err| (err.0.id.clone(), err.0.status_code))
    }

    /// Row 4: no emails is the body-param 400 naming `user_email`, graceful or not.
    #[test]
    fn no_emails_is_the_user_email_param_error() {
        for graceful in [false, true] {
            assert_eq!(
                id_of(local_invite_refusal(&invite(&[]), graceful)),
                Some(("api.context.invalid_body_param.app_error".to_owned(), 400))
            );
        }
    }

    /// Row 5: each address is lower-cased **before** `IsValidEmail`, so an upper-case address
    /// passes and a malformed one is the `invalid_email` 400 — the first bad one, in order.
    #[test]
    fn addresses_are_normalised_then_validated_in_order() {
        assert_eq!(
            id_of(local_invite_refusal(&invite(&["A@B.CO"]), false)),
            None
        );
        let refused = local_invite_refusal(&invite(&["ok@example.com", "nope", "x@y.z"]), false)
            .expect_err("the second address is not an email");
        assert_eq!(
            refused.0.id,
            "api.team.invite_members.invalid_email.app_error"
        );
        assert_eq!(refused.0.status_code, 400);
        assert_eq!(
            refused
                .0
                .params
                .as_ref()
                .and_then(|p| p.get("Address"))
                .and_then(|v| v.as_str()),
            Some("nope"),
            "the refused address is the one Go reports, normalised"
        );
    }

    /// Row 6: profiles need `?graceful=`; a `[null]` entry counts, and graceful admits them.
    #[test]
    fn profiles_without_graceful_are_refused_and_null_entries_count() {
        let mut with_profiles = invite(&["a@b.co"]);
        with_profiles.profiles = Some(vec![None]);
        assert_eq!(
            id_of(local_invite_refusal(&with_profiles, false)),
            Some((
                "api.team.invite_members.profiles_graceful.app_error".to_owned(),
                400
            ))
        );
        assert_eq!(id_of(local_invite_refusal(&with_profiles, true)), None);

        let mut empty_profiles = invite(&["a@b.co"]);
        empty_profiles.profiles = Some(vec![]);
        assert_eq!(
            id_of(local_invite_refusal(&empty_profiles, false)),
            None,
            "`len(Profiles) > 0` is false for an empty list"
        );
    }

    /// The email check precedes the profiles check: a body that fails both reports the email.
    #[test]
    fn the_email_check_runs_before_the_profiles_check() {
        let mut both = invite(&["nope"]);
        both.profiles = Some(vec![None]);
        assert_eq!(
            id_of(local_invite_refusal(&both, false)),
            Some((
                "api.team.invite_members.invalid_email.app_error".to_owned(),
                400
            ))
        );
    }

    /// `decode_body`: `null` is the zero struct, trailing bytes are ignored, and a non-object is
    /// the body-param 400 — `json.NewDecoder(...).Decode` into a struct.
    #[tokio::test]
    async fn decode_body_is_gos_decoder_into_a_zero_struct() {
        let request = |body: &'static str| {
            Request::builder()
                .body(Body::from(body))
                .expect("request builds")
        };
        let hook: IncomingWebhook = decode_body(request("null"), "incoming_webhook")
            .await
            .expect("null is a no-op");
        assert_eq!(hook, IncomingWebhook::default());

        let hook: IncomingWebhook = decode_body(request(r#"{"user_id":"u"} trailing"#), "x")
            .await
            .expect("one value is read");
        assert_eq!(hook.user_id, "u");

        let err = decode_body::<IncomingWebhook>(request("[1]"), "incoming_webhook")
            .await
            .expect_err("an array is not a struct");
        assert_eq!(err.0.id, "api.context.invalid_body_param.app_error");
    }
}
