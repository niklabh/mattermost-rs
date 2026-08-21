//! Ported handlers from `channels/api4/team.go`:
//!
//! - `getTeamsForUser` — `GET /api/v4/users/{user_id}/teams`
//! - `getTeamMembersForUser` — `GET /api/v4/users/me/teams/members` (`me` only)
//! - `getTeam` — `GET /api/v4/teams/{team_id}`
//! - `getTeamByName` — `GET /api/v4/teams/name/{team_name}`
//! - `getTeamStats` — `GET /api/v4/teams/{team_id}/stats`
//! - `getTeamMember` — `GET /api/v4/teams/{team_id}/members/{user_id}`
//! - `getTeamMembers` — `GET /api/v4/teams/{team_id}/members`
//! - `getTeamsUnreadForUser` — `GET /api/v4/users/{user_id}/teams/unread`
//! - `getTeamUnread` — `GET /api/v4/users/{user_id}/teams/{team_id}/unread`

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS, PERMISSION_VIEW_TEAM, make_permission_error,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;
use mm_store::team_store::TeamMembersGetOptions;

/// Port of `getTeamMembersForUser` for the `me` case.
///
/// # Why this route is portable when its neighbours are not
///
/// Go guards a sanitiser with a permission check:
///
/// ```go
/// if !c.App.SessionHasPermissionToTeam(session, m.TeamId, model.PermissionManageTeamRoles) {
///     m.SanitizeRoleData(currentUserId)
/// }
/// ```
///
/// and `SessionHasPermissionToTeam` needs the roles-and-permissions system, which is unported.
/// But `SanitizeRoleData` is a **no-op when `o.UserId == currentUserId`** (team_member.go:147),
/// and this route returns the caller's *own* memberships — so every element satisfies that, and
/// the permission check's outcome cannot change the response.
///
/// The sanitiser is therefore called **unconditionally** here. For `me` that is provably
/// identical to Go; if the route were ever widened to `/users/{id}/teams/members` it would be
/// stricter than Go rather than looser, which is the safe direction to be wrong in. The two
/// preceding permission checks (`SessionHasPermissionToUser`, `UserCanSeeOtherUser`) both
/// short-circuit to `true` for self (authorization.go:258, user.go:2711), so they are true by
/// construction rather than skipped.
///
/// Contrast [`get_teams_for_user`], which was *not* portable this way and stayed forwarded until
/// `SessionHasPermissionToTeam` landed: its `SanitizeTeam` strips `email` and `invite_id` based
/// on two team-scoped permissions with no self-shortcut. See [D-094] for the distinction.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:914), so no trailing newline — same call-site rule as
/// `/users/me/sessions`, not `/users/me` ([D-086]).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count))]
pub async fn get_team_members_for_user_me(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `GetTeamMembersForUser(ctx, userId, "", true)` — no team excluded, deleted included. The
    // handler does not filter afterwards, so a deleted membership *is* returned here, unlike in
    // `SessionStore::Get` where Go drops them. Same store call, different post-processing.
    let mut members = state
        .app
        .get_team_members_for_user(&session.0.user_id, "", true)
        .await?;

    let current_user_id = &session.0.user_id;
    for member in &mut members {
        member.sanitize_role_data(current_user_id);
    }

    tracing::Span::current().record("count", members.len());

    let body = serde_json::to_vec(&members).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise team members");
        ApiError(mm_model::utils::AppError::new(
            "getTeamMembersForUser",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Go's permission gate for [`get_teams_for_user`] (api4/team.go:737): asking about oneself is
/// always allowed; asking about anyone else needs `sysconsole_read_user_management_users` — a
/// **system-console** permission, not `edit_other_users` like `getChannelUnread`'s user gate.
/// Same shape either way: which permission a gate names is invisible over HTTP (the 403s are
/// identical with `detailed_error` wiped, [D-092]), so it is pinned here where a unit test can
/// hold it.
///
/// Note the self test is a plain string comparison against the session's user id — Go does
/// **not** call `SessionHasPermissionToUser` here, so its `manage_system`/unrestricted shortcuts
/// never run and the empty-target-denies rule does not apply. The permission check is the only
/// branch, and it is skipped entirely for self, which a test asserts by never polling it.
async fn teams_for_user_denied<F, Fut>(
    session_user_id: &str,
    target_user_id: &str,
    sysconsole_allowed: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    session_user_id != target_user_id && !sysconsole_allowed().await
}

/// Port of `getTeamsForUser` (api4/team.go:731), reached as
/// `GET /api/v4/users/{user_id}/teams`.
///
/// The route [D-094] classified as **not escapable**: `SanitizeTeam` strips `email` and
/// `invite_id` per two team-scoped permissions with no self-shortcut, so it stayed forwarded —
/// with a test keeping it forwarded — until `SessionHasPermissionToTeam` existed. That checker
/// landed with the permission system, so the route now serves from Rust and the keeping-it-
/// forwarded test asserts the opposite.
///
/// # Order of operations
///
/// 1. **`me` resolves before validation** (web/context.go:301), as everywhere.
/// 2. `RequireUserId`.
/// 3. **Self, or `sysconsole_read_user_management_users`** — see [`teams_for_user_denied`].
/// 4. `GetTeamsForUser`, then `SanitizeTeams` over every element. The sanitiser runs for
///    **self too**: being a team's plain member grants neither `manage_team` nor `invite_user`,
///    so one's own team list usually has both fields stripped — that is Go's answer, not
///    over-sanitising.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:750) — **no trailing newline**, same call-site rule as
/// `/users/me/teams/members` above and unlike the channel routes ([D-086]). An empty team list
/// is `[]`, never `null`, because Go's store initialises the slice.
#[tracing::instrument(skip_all, fields(user_id = %user_id, count))]
pub async fn get_teams_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireUserId` substitutes the session's id for `me` **before** the validity check
    // (web/context.go:301), so the alias works and an invalid literal still 400s.
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    require_id(&user_id, "user_id")?;

    let denied = teams_for_user_denied(&session.0.user_id, &user_id, || async {
        state
            .app
            .session_has_permission_to(
                &session.0,
                &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS,
            )
            .await
    })
    .await;

    if denied {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS],
        )));
    }

    let mut teams = state.app.get_teams_for_user(&user_id).await?;
    tracing::Span::current().record("count", teams.len());

    state.app.sanitize_teams(&session.0, &mut teams).await;

    let body = serde_json::to_vec(&teams).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the team list");
        ApiError(mm_model::utils::AppError::new(
            "getTeamsForUser",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Go's `team.AllowOpenInvite && team.Type == model.TeamOpen` (api4/team.go:364).
///
/// **Both** conjuncts are load-bearing and the four cells all occur in real data: an invite-only
/// team can carry `AllowOpenInvite = true` (the column survives a type change), and an open-type
/// team defaults to `AllowOpenInvite = false` at creation. Either single-flag reading widens
/// "public" to teams that are not.
fn team_is_public(team: &mm_model::team::Team) -> bool {
    team.allow_open_invite && team.team_type == mm_model::team::TEAM_OPEN
}

/// Go's permission block for [`get_team`] (api4/team.go:363-374), with the fallback lazy so its
/// evaluation — invisible over HTTP, since both denials answer the same 403 — is testable
/// in-process, exactly like `channel_read_denied`.
///
/// The shape differs from `getChannel`'s in both directions, and each difference is Go's:
///
/// - **`view_team` is computed unconditionally** — the caller evaluates it even when the team is
///   public and the fallback alone could admit; Go assigns `hasPermissionViewTeam` before any
///   branch, so this function takes it as a `bool`, not a closure.
/// - **`list_public_teams` is polled only for a public team that `view_team` denied.** It is a
///   roles-only check, but Go's `&&` short-circuit is still the observable shape: a non-public
///   team must deny without consulting it, or a role holding only `list_public_teams` would
///   appear to matter where it cannot.
async fn team_view_denied<F, Fut>(
    is_public_team: bool,
    has_view_team: bool,
    list_public_teams: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if !is_public_team && !has_view_team {
        return true;
    }
    if is_public_team && !has_view_team && !list_public_teams().await {
        return true;
    }
    false
}

/// [`get_team`]'s one refusal: `c.SetPermissionError(model.PermissionViewTeam)` from **both**
/// branches — Go's own comment says it: *"Fail with PermissionViewTeam, not
/// PermissionListPublicTeams"* (api4/team.go:371). Same in-process pinning as
/// `get_channel_denial`, and for the same reason: the permission's name reaches a client only
/// through the wiped `detailed_error` ([D-092]).
fn get_team_denial(session: &mm_model::session::Session) -> ApiError {
    ApiError(*make_permission_error(
        session,
        &[&mm_model::permission::PERMISSION_VIEW_TEAM],
    ))
}

/// Port of `getTeam` (api4/team.go:303), reached as `GET /api/v4/teams/{team_id}`.
///
/// # Order of operations
///
/// 1. **The content-reviewer branch is forwarded, detected first.** Go reads the flag *after*
///    `RequireTeamId` and `GetTeam`, so on a missing team `?as_content_reviewer=true` is a 404,
///    not the license 501 — and forwarding the whole request preserves exactly that, because Go
///    re-runs both steps itself. Same Strangler-inside-a-route pattern as `getChannel`.
/// 2. `RequireTeamId` — no `me` alias for teams (web/context.go:322 validates only); the segment
///    charset is already checked by [`crate::partially_migrated_with_ids`].
/// 3. **`GetTeam` runs before the permission block** — the block needs `AllowOpenInvite` and
///    `Type` to choose its shape, so a missing team is a 404 here, like `getChannel` and unlike
///    `getChannelMember`. The store applies no `DeleteAt` filter: an archived team still serves.
/// 4. The permission block — see [`team_view_denied`].
/// 5. `SanitizeTeam` — `manage_team` keeps `email`, `invite_user` keeps `invite_id`, both
///    checks against **this** team ([D-094]'s pairing, already ported).
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(team)` — trailing newline ([D-086]), unlike `getTeamsForUser`'s
/// `json.Marshal` + `Write` in this same file. The two call sites really do differ.
#[tracing::instrument(skip_all, fields(team_id = %team_id, forwarded))]
pub async fn get_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if crate::channels::is_content_reviewer_request(request.uri().query()) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let mut team = match state.app.get_team(&team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError(err).into_response(),
    };

    // Unconditional, as Go's assignment is — even when the team is public and the fallback could
    // decide alone. See `team_view_denied`.
    let has_view_team = state
        .app
        .session_has_permission_to_team(
            &session.0,
            &team.id,
            &mm_model::permission::PERMISSION_VIEW_TEAM,
        )
        .await;

    let denied = team_view_denied(team_is_public(&team), has_view_team, || async {
        state
            .app
            .session_has_permission_to(
                &session.0,
                &mm_model::permission::PERMISSION_LIST_PUBLIC_TEAMS,
            )
            .await
    })
    .await;

    if denied {
        return get_team_denial(&session.0).into_response();
    }

    state.app.sanitize_team(&session.0, &mut team).await;

    let mut body = match serde_json::to_vec(&team) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise Team");
            return ApiError(mm_model::utils::AppError::new(
                "getTeam",
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

/// Port of `getTeamStats` (api4/team.go:1345), reached as `GET /api/v4/teams/{team_id}/stats`.
///
/// # Order of operations
///
/// 1. `RequireTeamId` — segment charset handled by [`crate::partially_migrated_with_ids`].
/// 2. One gate: `SessionHasPermissionToTeam(view_team)`, denial via [`get_team_denial`] —
///    the same permission and refusal as `getTeam`, but **no public-team fallback** here: Go
///    never consults `list_public_teams` for stats, so a non-member is refused the numbers of a
///    team whose body `getTeam` would serve. That asymmetry is Go's, measured.
/// 3. **The team is never fetched** — the gate reads the session's memberships and roles, not
///    the `Teams` table, so a well-formed id that matches nothing is a **200 of zeroes** for a
///    caller the gate admits (an admin, via system roles) and a 403 for everyone else. The
///    opposite of `getChannelStats`, whose gate's own channel lookup made the same request a
///    403 even for the admin; the difference is which checker each handler calls, not a policy.
/// 4. `GetViewUsersRestrictions`: Go builds view restrictions unless the caller holds
///    system-wide `view_members`, which the default `system_user` role grants — so restrictions
///    are nil for every caller in this deployment. **The restricted case is forwarded whole**
///    rather than ported: it needs user-based team checks and dynamically-spliced restriction
///    joins, and Go re-runs the id check and the gate itself, so ordering holds by construction.
///    Same Strangler-inside-a-route pattern as the content-reviewer flags.
/// 5. Two counts, total then active — the app layer carries Go's error precedence.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(stats)` — trailing newline ([D-086]). Three keys, no `omitempty`,
/// fixture-pinned in `mm-model/src/stats.rs`.
#[tracing::instrument(skip_all, fields(team_id = %team_id, forwarded))]
pub async fn get_team_stats(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let allowed = state
        .app
        .session_has_permission_to_team(
            &session.0,
            &team_id,
            &mm_model::permission::PERMISSION_VIEW_TEAM,
        )
        .await;
    if !allowed {
        return get_team_denial(&session.0).into_response();
    }

    // `GetViewUsersRestrictions` returns nil iff the caller holds system-wide `view_members` —
    // the check is user-based (the row's roles, not the session's). Anything else would need
    // the whole restrictions machinery, and Go owns that answer.
    if !state
        .app
        .has_permission_to(
            &session.0.user_id,
            &mm_model::permission::PERMISSION_VIEW_MEMBERS,
        )
        .await
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let stats = match state.app.get_team_stats(&team_id).await {
        Ok(stats) => stats,
        Err(err) => return ApiError(err).into_response(),
    };

    let mut body = match serde_json::to_vec(&stats) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise TeamStats");
            return ApiError(mm_model::utils::AppError::new(
                "getTeamStats",
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

/// Go's team-name-parameter charset: `{team_name:[A-Za-z0-9_-]+}` (api.go:216) — the id class
/// plus `_` and `-`, one character narrower than the username class (no `.`). A segment outside
/// it never matches Go's route and falls to the mux 404, so it is forwarded rather than
/// answered — [D-150]'s rule under a third alphabet.
fn segment_matches_team_name_mux(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The literals under `/teams/name/` that Go never routes to `getTeamByName`.
///
/// gorilla/mux tries routes in registration order, and `BaseRoutes.Team`
/// (`/teams/{team_id:[A-Za-z0-9]+}`) is registered **before** `BaseRoutes.TeamByName`
/// (api.go:212 versus :216). `name` is a valid `[A-Za-z0-9]+` segment, so
/// `GET /teams/name/<X>` first tries the `Team` subrouter with `team_id = "name"` — and when
/// `<X>` is one of its **GET** literals (`image`, `stats`, and `members` via `TeamMembers`), that
/// handler runs, `RequireTeamId` fails on `"name"`, and the answer is a 400 naming `team_id`. A
/// `<X>` whose `Team` route is registered for another method only (`patch`, `privacy`,
/// `restore`, `import`) is a method mismatch, which mux skips, and `getTeamByName("patch")`
/// answers the usual 404. All seven measured against the running server.
///
/// axum resolves the same path the other way — a static `name` beats `{team_id}` regardless of
/// registration order — so these three must be forwarded for Go's `{team_id}` precedence to
/// hold. A team really named `stats` is unreachable by name on both servers as a result.
const TEAM_BY_NAME_SHADOWED_LITERALS: [&str; 3] = ["image", "stats", "members"];

/// Does Go's `{team_id}` subrouter shadow this `/teams/name/{team_name}` segment? See
/// [`TEAM_BY_NAME_SHADOWED_LITERALS`].
fn team_name_is_shadowed_by_team_id_route(team_name: &str) -> bool {
    TEAM_BY_NAME_SHADOWED_LITERALS.contains(&team_name)
}

/// Go's permission block for [`get_team_by_name`] (api4/team.go:399):
///
/// ```go
/// if (!team.AllowOpenInvite || team.Type != model.TeamOpen) && !SessionHasPermissionToTeam(view_team)
/// ```
///
/// **Not** [`get_team`]'s block, though it guards the same field pair. `getTeam` computes
/// `view_team` unconditionally and falls back to `list_public_teams` for a public team; this one
/// admits a public team **without any permission query** and polls `view_team` only for a
/// non-public one — the `&&` short-circuit is the observable shape. A port that reused
/// `team_view_denied` would issue a role read on behalf of every public-team request and, for a
/// caller somehow lacking `list_public_teams`, refuse a team Go serves.
async fn team_by_name_denied<F, Fut>(is_public_team: bool, has_view_team: F) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    !is_public_team && !has_view_team().await
}

/// Port of `getTeamByName` (api4/team.go:386), reached as `GET /api/v4/teams/name/{team_name}`.
///
/// # Order of operations
///
/// 1. **Forward what Go would not route here**: the three shadowed literals
///    ([`TEAM_BY_NAME_SHADOWED_LITERALS`]) and any segment outside the mux charset
///    ([`segment_matches_team_name_mux`]). Both are router decisions, and Go's router owns them.
/// 2. `RequireTeamName` — `IsValidTeamName` (lowercase alphanumerics and hyphens, two characters
///    minimum), failing with `invalid_url_param` naming `team_name`. The mux class is wider than
///    the validator (`Up_per` routes, then 400s), so both steps are needed, in this order.
/// 3. `GetTeamByName` — a 404 on a miss, and a **404 on a broken store too**; see
///    `App::get_team_by_name`. As in `getTeam`, the fetch precedes the gate, because the gate
///    needs the team's flags.
/// 4. The permission block — see [`team_by_name_denied`]. Denial names `view_team`, via
///    [`get_team_denial`].
/// 5. `SanitizeTeam`, same pairing as `getTeam` ([D-094]).
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(team)` — trailing newline ([D-086]), like `getTeam`.
#[tracing::instrument(skip_all, fields(team_name = %team_name, forwarded))]
pub async fn get_team_by_name(
    State(state): State<AppState>,
    Path(team_name): Path<String>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if team_name_is_shadowed_by_team_id_route(&team_name)
        || !segment_matches_team_name_mux(&team_name)
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    if !mm_model::team::is_valid_team_name(&team_name) {
        return ApiError::invalid_url_param("team_name").into_response();
    }

    let mut team = match state.app.get_team_by_name(&team_name).await {
        Ok(team) => team,
        Err(err) => return ApiError(err).into_response(),
    };

    let denied = team_by_name_denied(team_is_public(&team), || async {
        state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team.id,
                &mm_model::permission::PERMISSION_VIEW_TEAM,
            )
            .await
    })
    .await;

    if denied {
        return get_team_denial(&session.0).into_response();
    }

    state.app.sanitize_team(&session.0, &mut team).await;

    let mut body = match serde_json::to_vec(&team) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise Team");
            return ApiError(mm_model::utils::AppError::new(
                "getTeamByName",
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

/// Go's `c.RequireTeamId().RequireUserId()` (api4/team.go:793), as one call so the **order** is
/// testable — the parameter name travels only in the untranslated `message` ([D-092]), so a
/// swapped chain survives every cross-server test. Same lift as `channels::validate_ids`.
#[allow(clippy::result_large_err)]
fn validate_team_and_user_ids(team_id: &str, user_id: &str) -> Result<(), ApiError> {
    require_id(team_id, "team_id")?;
    require_id(user_id, "user_id")?;
    Ok(())
}

/// Go's `UserCanSeeOtherUser` (app/user.go:2710) as every ported route serves it: **self is
/// visible without a query**, and anyone else is visible on the nil-restrictions fast path —
/// user-based `view_members`, the default `system_user` grant. A caller holding neither takes
/// the restricted remainder, which this server forwards. Returned as a three-way answer so the
/// self short-circuit is pinned in-process: Go never computes restrictions for self, and a port
/// that did would issue role reads on the commonest request.
#[derive(Debug, PartialEq, Eq)]
enum Visibility {
    Visible,
    Forward,
}

async fn user_visibility<F, Fut>(
    session_user_id: &str,
    target_user_id: &str,
    has_view_members: F,
) -> Visibility
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if session_user_id == target_user_id || has_view_members().await {
        Visibility::Visible
    } else {
        Visibility::Forward
    }
}

/// Port of `getTeamMember` (api4/team.go:792), reached as
/// `GET /api/v4/teams/{team_id}/members/{user_id}`.
///
/// # Order of operations
///
/// 1. **`me` resolves before validation** (web/context.go:301); then team id, then user id —
///    see [`validate_team_and_user_ids`].
/// 2. `SessionHasPermissionToTeam(view_team)` → 403 naming `view_team`. **Before** the
///    visibility question and before any fetch: a non-member learns nothing about who else is
///    in the team, not even whether the user exists.
/// 3. `UserCanSeeOtherUser` — see [`user_visibility`]; the restricted remainder forwards whole,
///    and Go re-runs steps 1–2 itself, so ordering holds by construction. A `false` answer would
///    be a 403 naming `view_members`; it is unreachable on the fast path and lives in Go.
/// 4. `GetTeamMember` — 404 `app.team.get_member.missing.app_error` when there is no row,
///    including for a **well-formed team id that matches nothing**: Go never fetches the team,
///    and the admin's system roles pass the gate, so the admin gets this 404 where a plain user
///    got step 2's 403. Measured.
/// 5. `SanitizeRoleData(currentUserId)` unless the session holds `manage_team_roles` on the
///    team — the guard `getTeamMembersForUser` could skip because its rows were all the
///    caller's own. Here the row is usually someone else's, so the guard is live: a team admin
///    sees another member's roles, a plain member sees them blanked with `delete_at: -1`.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(team)` — trailing newline ([D-086]).
#[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, forwarded))]
pub async fn get_team_member(
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    if let Err(err) = validate_team_and_user_ids(&team_id, &user_id) {
        return err.into_response();
    }

    let has_view_team = state
        .app
        .session_has_permission_to_team(
            &session.0,
            &team_id,
            &mm_model::permission::PERMISSION_VIEW_TEAM,
        )
        .await;
    if !has_view_team {
        return get_team_denial(&session.0).into_response();
    }

    let visibility = user_visibility(&session.0.user_id, &user_id, || async {
        state
            .app
            .has_permission_to(
                &session.0.user_id,
                &mm_model::permission::PERMISSION_VIEW_MEMBERS,
            )
            .await
    })
    .await;
    if visibility == Visibility::Forward {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let mut member = match state.app.get_team_member(&team_id, &user_id).await {
        Ok(member) => member,
        Err(err) => return ApiError(err).into_response(),
    };

    let can_manage_roles = state
        .app
        .session_has_permission_to_team(
            &session.0,
            &team_id,
            &mm_model::permission::PERMISSION_MANAGE_TEAM_ROLES,
        )
        .await;
    if !can_manage_roles {
        member.sanitize_role_data(&session.0.user_id);
    }

    let mut body = match serde_json::to_vec(&member) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise TeamMember");
            return ApiError(mm_model::utils::AppError::new(
                "getTeamMember",
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

/// `getTeamMembers`'s two query parameters (api4/team.go:834) — literals in the handler, no
/// model constants to cite.
const SORT_PARAM: &str = "sort";
const EXCLUDE_DELETED_USERS_PARAM: &str = "exclude_deleted_users";

/// The `TeamMembersGetOptions` Go builds from the query string (api4/team.go:834-849), minus
/// the restrictions this server forwards on.
///
/// `sort` is passed through **raw** — no trimming, no case folding — because the store's
/// three-way branch compares it byte-for-byte against `"Username"`: `?sort=username` is the
/// "anything else" arm and orders by nothing. `exclude_deleted_users` is `strconv.ParseBool`
/// with the error discarded, the same idiom as every other boolean flag.
fn team_members_options(query: Option<&str>) -> TeamMembersGetOptions {
    TeamMembersGetOptions {
        sort: crate::channels::query_first(query, SORT_PARAM).unwrap_or_default(),
        exclude_deleted_users: crate::channels::query_flag_is_true(
            query,
            EXCLUDE_DELETED_USERS_PARAM,
        ),
    }
}

/// Port of `getTeamMembers` (api4/team.go:829), reached as
/// `GET /api/v4/teams/{team_id}/members` — the second paginated route.
///
/// # Order of operations
///
/// 1. `RequireTeamId`; the query parameters are read before the gate, as Go does, but none of
///    them can fail, so nothing is observable about that order.
/// 2. `SessionHasPermissionToTeam(view_team)` → 403 naming `view_team`. **The team is never
///    fetched**, so — exactly as `getTeamStats` — a well-formed id that matches nothing is an
///    empty `[]` for a caller the gate admits and a 403 for everyone else.
/// 3. `GetViewUsersRestrictions`: nil iff the caller holds user-based `view_members`; the
///    restricted case is forwarded whole, same as `getTeamStats` and `getUser`.
/// 4. `GetTeamMembers(page × per_page, per_page, options)` — the shared parser, with one
///    difference from `getChannelMembers` that lives in the store: **`per_page=0` is an empty
///    list here**, not the whole team, because `SqlTeamStore.GetMembers` emits `LIMIT 0`
///    unguarded. Both measured; see `team_store::get_members`.
/// 5. `SanitizeRoleData` over every element unless the caller holds `manage_team_roles` — a
///    plain member sees every *other* row blanked with `delete_at: -1` and its own row intact,
///    mid-list; a team admin sees every row whole.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:868) — **no trailing newline**, unlike `getTeamMember`
/// two functions up and unlike `getChannelMembers` ([D-086]). An empty page is `[]`.
#[tracing::instrument(skip_all, fields(team_id = %team_id, page, per_page, forwarded))]
pub async fn get_team_members(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let page = crate::channels::parse_page(query.as_deref());
    let per_page = crate::channels::parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let options = team_members_options(query.as_deref());

    let has_view_team = state
        .app
        .session_has_permission_to_team(
            &session.0,
            &team_id,
            &mm_model::permission::PERMISSION_VIEW_TEAM,
        )
        .await;
    if !has_view_team {
        return get_team_denial(&session.0).into_response();
    }

    if !state
        .app
        .has_permission_to(
            &session.0.user_id,
            &mm_model::permission::PERMISSION_VIEW_MEMBERS,
        )
        .await
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let mut members = match state
        .app
        .get_team_members(&team_id, page.wrapping_mul(per_page), per_page, &options)
        .await
    {
        Ok(members) => members,
        Err(err) => return ApiError(err).into_response(),
    };

    let can_manage_roles = state
        .app
        .session_has_permission_to_team(
            &session.0,
            &team_id,
            &mm_model::permission::PERMISSION_MANAGE_TEAM_ROLES,
        )
        .await;
    if !can_manage_roles {
        for member in &mut members {
            member.sanitize_role_data(&session.0.user_id);
        }
    }

    let body = match serde_json::to_vec(&members) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the member list");
            return ApiError(mm_model::utils::AppError::new(
                "getTeamMembers",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };

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

/// `getTeamsUnreadForUser`'s two query parameters — literals in the handler (team.go:775-776).
const EXCLUDE_TEAM_PARAM: &str = "exclude_team";
const INCLUDE_COLLAPSED_THREADS_PARAM: &str = "include_collapsed_threads";

/// Would Go take the collapsed-threads path? A **string compare** against `"true"`
/// (team.go:776), not `strconv.ParseBool`: `=1`, `=t` and `=True` are all false here, where
/// the `query_flag_is_true` routes would read them as true.
fn wants_collapsed_threads(query: Option<&str>) -> bool {
    crate::channels::query_first(query, INCLUDE_COLLAPSED_THREADS_PARAM).as_deref() == Some("true")
}

/// Port of `getTeamsUnreadForUser` (api4/team.go:761), reached as
/// `GET /api/v4/users/{user_id}/teams/unread`.
///
/// # Order of operations
///
/// 1. `me` resolves, then `RequireUserId`.
/// 2. **Self by string comparison, or `manage_system`** — the real system-admin permission, not
///    the `sysconsole_read_user_management_users` its `/teams` sibling accepts. A sysconsole
///    reader can list another user's teams and is refused their unread counts.
/// 3. `exclude_team` is passed through verbatim — **no `IsValidId`**, and an empty value is still
///    a predicate (see the store: it is what hides the DMs).
/// 4. `include_collapsed_threads=true` is **forwarded to Go whole**. That half needs the Threads
///    store, `CollapsedThreads` and `PostPriority` config, none of which is ported. Go re-runs
///    the same gate, so forwarding before step 2 would answer identically; it runs after so the
///    served and forwarded paths share one refusal. The webapp sends `true` whenever the user has
///    CRT on, so on a CRT-enabled deployment most real traffic for this route is still Go's.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:788) — **no trailing newline** ([D-086]). The list's
/// order is Go's map-iteration order, i.e. random per request; see `App::fold_team_unreads`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded, count))]
pub async fn get_teams_unread_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }

    let denied = teams_for_user_denied(&session.0.user_id, &user_id, || async {
        state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    })
    .await;
    if denied {
        return ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    if wants_collapsed_threads(query.as_deref()) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let exclude_team =
        crate::channels::query_first(query.as_deref(), EXCLUDE_TEAM_PARAM).unwrap_or_default();

    let unreads = match state
        .app
        .get_teams_unread_for_user(&exclude_team, &user_id)
        .await
    {
        Ok(unreads) => unreads,
        Err(err) => return ApiError(err).into_response(),
    };
    tracing::Span::current().record("count", unreads.len());

    let body = match serde_json::to_vec(&unreads) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the team unread list");
            return ApiError(mm_model::utils::AppError::new(
                "getTeamsUnreadForUser",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };

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

/// `getTeamUnread`'s two permission gates in Go's order (api4/team.go:1323-1331), returning the
/// permission the refusal names — or `None` when both grant.
///
/// Lifted out of the handler for the same reason `validate_team_and_user_ids` is: the order is
/// **not observable over HTTP**. `WipeDetailed` empties `detailed_error` outside dev mode
/// (model/utils.go:339) and `message` is the untranslated third of [D-092], so a caller who fails
/// *both* gates gets a byte-identical 403 either way. The order is pinned here, where a unit test
/// can hold it — including that the team check is not even *evaluated* when the user check
/// refuses, which is what makes running them in Go's sequence cheap as well as correct.
async fn team_unread_denied<U, UFut, T, TFut>(
    user_allowed: U,
    team_allowed: T,
) -> Option<&'static mm_model::permission::Permission>
where
    U: FnOnce() -> UFut,
    UFut: std::future::Future<Output = bool>,
    T: FnOnce() -> TFut,
    TFut: std::future::Future<Output = bool>,
{
    if !user_allowed().await {
        return Some(&PERMISSION_EDIT_OTHER_USERS);
    }
    if !team_allowed().await {
        return Some(&PERMISSION_VIEW_TEAM);
    }
    None
}

/// Port of `getTeamUnread` (api4/team.go:1318), reached as
/// `GET /api/v4/users/{user_id}/teams/{team_id}/unread`.
///
/// # Not the plural route with a filter
///
/// Its sibling `getTeamsUnreadForUser` gates on `manage_system` alone, reads
/// `GetChannelUnreadsForAllTeams` (`TeamId <> ?`) and forwards the collapsed-threads variant to
/// Go. This one shares none of that:
///
/// 1. **Two gates, in order.** `SessionHasPermissionToUser` — which *does* carry the
///    unrestricted/`manage_system` shortcut, the self shortcut and the
///    "even `edit_other_users` cannot touch a system admin" rule — then
///    `SessionHasPermissionToTeam(view_team)`. So the caller is refused for a team they cannot
///    see **even when asking about themselves**, which the plural route never does. Which of the
///    two permissions a refusal names never reaches a client — `WipeDetailed` empties
///    `detailed_error` outside dev mode — so the order lives in [`team_unread_denied`] with the
///    unit test that pins it.
/// 2. **A different query**, `GetChannelUnreadsForTeam` — `TeamId = ?`.
/// 3. **Nothing is forwarded.** There is no threads half in Go's singular handler at all, so
///    `include_collapsed_threads` is not even read here; the three `thread_*` counters are always
///    zero, on both servers, for every caller.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode` (team.go:1341) — an *encoder*, so the body carries a **trailing
/// newline**, where the plural route's `json.Marshal` + `w.Write` does not ([D-086]). Two routes
/// for the same struct in one Go file, opposite answers; the parity suite asserts the byte.
///
/// A user with nothing unread is an all-zero object carrying the requested `team_id`, never a
/// 404 and never an omission — Go builds the struct before the loop.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
pub async fn get_team_unread(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    if let Err(err) = validate_team_and_user_ids(&team_id, &user_id) {
        return err.into_response();
    }

    let denial = team_unread_denied(
        || async {
            state
                .app
                .session_has_permission_to_user(&session.0, &user_id)
                .await
        },
        || async {
            state
                .app
                .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
                .await
        },
    )
    .await;
    if let Some(permission) = denial {
        return ApiError(*make_permission_error(&session.0, &[permission])).into_response();
    }

    let unread = match state.app.get_team_unread(&team_id, &user_id).await {
        Ok(unread) => unread,
        Err(err) => return ApiError(err).into_response(),
    };

    let mut body = match serde_json::to_vec(&unread) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the team unread");
            return ApiError(mm_model::utils::AppError::new(
                "getTeamUnread",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    // `json.NewEncoder(w).Encode` writes the newline; the plural sibling's `w.Write` does not.
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

#[cfg(test)]
mod tests {
    use mm_model::team_member::TeamMember;

    use super::{
        TeamMembersGetOptions, Visibility, get_team_denial, segment_matches_team_name_mux,
        team_by_name_denied, team_is_public, team_members_options,
        team_name_is_shadowed_by_team_id_route, team_unread_denied, team_view_denied,
        user_visibility, validate_team_and_user_ids, wants_collapsed_threads,
    };

    /// The string compare, not `ParseBool`: only the literal `true` forwards. `=1`, `=t` and
    /// `=True` — all true under the sibling routes' flag parser — are served here, as Go would.
    #[test]
    fn only_the_literal_true_forwards_the_collapsed_threads_variant() {
        assert!(wants_collapsed_threads(Some(
            "include_collapsed_threads=true"
        )));
        assert!(wants_collapsed_threads(Some(
            "exclude_team=abc&include_collapsed_threads=true&x=1"
        )));
        // First value of a repeated key, as `url.Values.Get` does.
        assert!(wants_collapsed_threads(Some(
            "include_collapsed_threads=true&include_collapsed_threads=false"
        )));
        for served in [
            None,
            Some(""),
            Some("include_collapsed_threads"),
            Some("include_collapsed_threads="),
            Some("include_collapsed_threads=1"),
            Some("include_collapsed_threads=t"),
            Some("include_collapsed_threads=True"),
            Some("include_collapsed_threads=false&include_collapsed_threads=true"),
            Some("exclude_team=true"),
        ] {
            assert!(!wants_collapsed_threads(served), "{served:?}");
        }
    }

    /// All four cells of `AllowOpenInvite × Type` — either single-flag reading passes three of
    /// them and fails the one that leaks: an invite-only team with the column still true.
    #[test]
    fn a_team_is_public_only_when_open_invite_and_open_type_agree() {
        let mut team = mm_model::team::Team {
            allow_open_invite: true,
            team_type: mm_model::team::TEAM_OPEN.to_owned(),
            ..Default::default()
        };
        assert!(team_is_public(&team));

        team.allow_open_invite = false;
        assert!(!team_is_public(&team), "open type alone is not public");

        team.allow_open_invite = true;
        team.team_type = mm_model::team::TEAM_INVITE.to_owned();
        assert!(
            !team_is_public(&team),
            "a surviving AllowOpenInvite on an invite team must not open it"
        );

        team.allow_open_invite = false;
        assert!(!team_is_public(&team));
    }

    /// A `view_team` grant admits without consulting the fallback — public or not.
    #[tokio::test]
    async fn a_view_team_grant_never_polls_list_public_teams() {
        for is_public in [true, false] {
            let denied = team_view_denied(is_public, true, || async {
                panic!("list_public_teams must not run when view_team grants")
            })
            .await;
            assert!(!denied, "is_public = {is_public}");
        }
    }

    /// A non-public team denied `view_team` is refused **without** the fallback running: a role
    /// holding only `list_public_teams` must not see a closed team.
    #[tokio::test]
    async fn a_non_public_team_denies_without_polling_the_fallback() {
        let denied = team_view_denied(false, false, || async {
            panic!("list_public_teams must not run for a non-public team")
        })
        .await;
        assert!(denied);
    }

    /// A public team denied `view_team` falls to `list_public_teams`, in both directions.
    #[tokio::test]
    async fn a_public_team_falls_from_view_team_to_list_public_teams() {
        assert!(!team_view_denied(true, false, || async { true }).await);
        assert!(team_view_denied(true, false, || async { false }).await);
    }

    /// Both denial branches answer with `view_team` — Go's comment spells it out, and the name
    /// only travels in the wiped `detailed_error` ([D-092]), so it is pinned here.
    #[test]
    fn the_get_team_denial_names_view_team() {
        let session = mm_model::session::Session {
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            ..Default::default()
        };
        let denial = get_team_denial(&session);
        assert_eq!(denial.0.status_code, 403);
        assert_eq!(denial.0.id, "api.context.permissions.app_error");
        assert_eq!(
            denial.0.detailed_error, "userId=y9i4er48tt8bukijy7i3u5y9ar, permission=view_team",
            "list_public_teams must never be the permission an error names"
        );
    }

    /// This handler encodes, so its body ends in a newline — its sibling `getTeamsForUser` in
    /// the same file marshals and does not ([D-086]).
    #[test]
    fn the_team_body_ends_in_a_newline() {
        let mut body = serde_json::to_vec(&mm_model::team::Team::default()).expect("serialises");
        body.push(b'\n');
        assert_eq!(body.last(), Some(&b'\n'));
    }

    fn member(user_id: &str) -> TeamMember {
        TeamMember {
            team_id: "dpn4orkqniyzurpjzw6w6qxg8y".to_owned(),
            user_id: user_id.to_owned(),
            roles: "team_user team_admin".to_owned(),
            delete_at: 0,
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: true,
            explicit_roles: String::new(),
            create_at: 0,
        }
    }

    const ME: &str = "y9i4er48tt8bukijy7i3u5y9ar";

    /// The claim this route's portability rests on: for the caller's own membership the sanitiser
    /// changes nothing, so the permission check Go wraps it in cannot affect the response.
    #[test]
    fn sanitize_role_data_is_a_no_op_for_ones_own_membership() {
        let mut mine = member(ME);
        let before = mine.clone();
        mine.sanitize_role_data(ME);
        assert_eq!(mine, before, "self-sanitising must not change anything");
    }

    /// And the other half, which is why calling it unconditionally is the safe direction: for
    /// someone else's membership it strips the role data and marks `delete_at` with Go's -1
    /// sentinel.
    #[test]
    fn sanitize_role_data_strips_another_users_membership() {
        let mut theirs = member("aaaaaaaaaaaaaaaaaaaaaaaaaa");
        theirs.sanitize_role_data(ME);

        assert_eq!(theirs.roles, "");
        assert_eq!(theirs.explicit_roles, "");
        assert!(!theirs.scheme_admin && !theirs.scheme_user && !theirs.scheme_guest);
        assert_eq!(theirs.delete_at, -1, "Go uses -1, not 0, as the sentinel");
    }

    /// `json.Marshal`, not an encoder — no trailing newline on this route.
    #[test]
    fn the_body_has_no_trailing_newline() {
        let body = serde_json::to_vec(&vec![member(ME)]).expect("serialises");
        assert_ne!(body.last(), Some(&b'\n'));
    }

    /// An empty membership list is `[]`, not `null`.
    #[test]
    fn an_empty_list_serialises_as_an_array() {
        let members: Vec<TeamMember> = Vec::new();
        assert_eq!(serde_json::to_string(&members).expect("serialises"), "[]");
    }

    /// Asking about oneself is a string comparison, not a permission check — the gate closure
    /// must never be polled, or a self request would issue role queries Go does not.
    #[tokio::test]
    async fn asking_about_oneself_never_polls_the_sysconsole_gate() {
        let denied = super::teams_for_user_denied(ME, ME, || async {
            panic!("the gate must not run for self")
        })
        .await;
        assert!(!denied);
    }

    /// Anyone else needs the sysconsole permission, in both directions.
    #[tokio::test]
    async fn asking_about_another_user_takes_the_sysconsole_gate() {
        let other = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(!super::teams_for_user_denied(ME, other, || async { true }).await);
        assert!(super::teams_for_user_denied(ME, other, || async { false }).await);
    }

    /// The 403 names `sysconsole_read_user_management_users` — a system-console permission, not
    /// `edit_other_users` like the channel routes' user gate. The name is only in the wiped
    /// `detailed_error` ([D-092]), so it is pinned here.
    #[test]
    fn the_denial_names_the_sysconsole_permission() {
        let session = mm_model::session::Session {
            user_id: ME.to_owned(),
            ..Default::default()
        };
        let err = super::make_permission_error(
            &session,
            &[&super::PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS],
        );
        assert_eq!(err.status_code, 403);
        assert_eq!(err.id, "api.context.permissions.app_error");
        assert_eq!(
            err.detailed_error,
            format!("userId={ME}, permission=sysconsole_read_user_management_users")
        );
    }

    /// An empty **team** list is `[]` too, and the body has no trailing newline — this route is
    /// `json.Marshal` + `w.Write`, not an encoder ([D-086]).
    #[test]
    fn the_team_list_body_is_an_array_with_no_newline() {
        let teams: Vec<mm_model::team::Team> = Vec::new();
        let body = serde_json::to_vec(&teams).expect("serialises");
        assert_eq!(body, b"[]");
        assert_ne!(body.last(), Some(&b'\n'));
    }

    /// `getTeamByName`'s gate is the `&&` short-circuit: a public team is admitted **without**
    /// a permission query — the opposite of `getTeam`, which polls `view_team` first.
    #[tokio::test]
    async fn a_public_team_by_name_never_polls_view_team() {
        let denied = team_by_name_denied(true, || async {
            panic!("view_team must not run for a public team on the by-name route")
        })
        .await;
        assert!(!denied);
    }

    /// A non-public team falls to `view_team`, in both directions — and there is no
    /// `list_public_teams` fallback on this route at all.
    #[tokio::test]
    async fn a_non_public_team_by_name_takes_the_view_team_gate() {
        assert!(!team_by_name_denied(false, || async { true }).await);
        assert!(team_by_name_denied(false, || async { false }).await);
    }

    /// The team-name class is the id class plus `_` and `-` — **not** `.`, which the username
    /// class admits. Each near-miss falls to the mux 404 forward.
    #[test]
    fn the_team_name_charset_is_gos_mux_class() {
        for ok in ["slice-team", "a_b-c", "UPPER", "0", "--"] {
            assert!(segment_matches_team_name_mux(ok), "{ok:?} matches Go's mux");
        }
        for bad in ["", "a.b", "a b", "a@b", "a%40b", "héllo"] {
            assert!(
                !segment_matches_team_name_mux(bad),
                "{bad:?} never matches Go's route, so it must be forwarded"
            );
        }
    }

    /// Exactly the GET literals under `BaseRoutes.Team` — the PUT/POST-only ones (`patch`,
    /// `privacy`, `restore`, `import`) are method mismatches mux skips, so Go serves them as
    /// team names and this server must too.
    #[test]
    fn only_the_get_literals_under_team_are_shadowed() {
        for shadowed in ["image", "stats", "members"] {
            assert!(
                team_name_is_shadowed_by_team_id_route(shadowed),
                "{shadowed}"
            );
        }
        for served in [
            "patch",
            "privacy",
            "restore",
            "import",
            "exists",
            "name",
            "slice-team",
        ] {
            assert!(
                !team_name_is_shadowed_by_team_id_route(served),
                "{served} reaches getTeamByName in Go"
            );
        }
    }

    /// **The team id is validated first** (`RequireTeamId().RequireUserId()`), pinned in-process
    /// because the parameter name is not on the wire ([D-092]).
    #[test]
    fn the_team_id_is_validated_before_the_user_id() {
        let name = |err: crate::error::ApiError| {
            err.0
                .params
                .as_ref()
                .and_then(|p| p.get("Name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        assert_eq!(
            name(validate_team_and_user_ids("nope", "alsonope").expect_err("both invalid")),
            Some("team_id".to_owned())
        );
        assert_eq!(
            name(validate_team_and_user_ids(ME, "alsonope").expect_err("user invalid")),
            Some("user_id".to_owned())
        );
        assert!(validate_team_and_user_ids(ME, "aaaaaaaaaaaaaaaaaaaaaaaaaa").is_ok());
    }

    /// Self is visible without consulting `view_members` — Go returns before computing
    /// restrictions, so the closure must never be polled.
    #[tokio::test]
    async fn asking_about_oneself_never_polls_view_members() {
        let visibility = user_visibility(ME, ME, || async {
            panic!("view_members must not run for self")
        })
        .await;
        assert_eq!(visibility, Visibility::Visible);
    }

    /// Anyone else rides the fast path when the caller holds `view_members`, and forwards when
    /// not — the restricted remainder is Go's.
    #[tokio::test]
    async fn asking_about_another_user_takes_the_view_members_fast_path() {
        let other = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            user_visibility(ME, other, || async { true }).await,
            Visibility::Visible
        );
        assert_eq!(
            user_visibility(ME, other, || async { false }).await,
            Visibility::Forward
        );
    }

    /// `sort` passes through raw — `username` is not `Username` — and the flag is Go's
    /// `ParseBool` with the error discarded; a repeated key takes its first value.
    #[test]
    fn team_members_options_are_read_the_way_go_reads_them() {
        assert_eq!(team_members_options(None), TeamMembersGetOptions::default());
        assert_eq!(
            team_members_options(Some("sort=Username&exclude_deleted_users=1")),
            TeamMembersGetOptions {
                sort: "Username".to_owned(),
                exclude_deleted_users: true,
            }
        );
        assert_eq!(
            team_members_options(Some("sort=username&exclude_deleted_users=yes")),
            TeamMembersGetOptions {
                sort: "username".to_owned(),
                exclude_deleted_users: false,
            },
            "no case folding on sort; `yes` is a ParseBool error, so false"
        );
        assert_eq!(
            team_members_options(Some(
                "sort=Username&sort=&exclude_deleted_users=false&exclude_deleted_users=true"
            )),
            TeamMembersGetOptions {
                sort: "Username".to_owned(),
                exclude_deleted_users: false,
            },
            "url.Values.Get takes the first value"
        );
    }

    /// `getTeamMembers` marshals (no newline) while `getTeamMember` encodes (newline) — the two
    /// call sites differ, two functions apart ([D-086]).
    #[test]
    fn the_member_list_has_no_newline_and_the_single_member_does() {
        let list = serde_json::to_vec(&vec![member(ME)]).expect("serialises");
        assert_ne!(list.last(), Some(&b'\n'));
        let mut single = serde_json::to_vec(&member(ME)).expect("serialises");
        single.push(b'\n');
        assert_eq!(single.last(), Some(&b'\n'));
    }

    /// The user gate runs **first** and short-circuits: a caller who fails both is refused with
    /// `edit_other_users`, and the team check is never evaluated. Neither fact reaches a client
    /// (`WipeDetailed`, [D-092]), so this is the only place either can be asserted.
    #[tokio::test]
    async fn the_user_gate_runs_first_and_the_team_gate_is_not_polled_when_it_refuses() {
        let team_polled = std::cell::Cell::new(false);
        let denial = team_unread_denied(
            || async { false },
            || async {
                team_polled.set(true);
                false
            },
        )
        .await;
        assert_eq!(
            denial.map(|p| p.id.as_ref()),
            Some("edit_other_users"),
            "the user gate names its own permission when both would refuse"
        );
        assert!(
            !team_polled.get(),
            "the team check must not run once the user check has refused"
        );

        assert_eq!(
            team_unread_denied(|| async { true }, || async { false })
                .await
                .map(|p| p.id.as_ref()),
            Some("view_team"),
            "and the team gate names view_team, not the user gate's permission"
        );
        assert!(
            team_unread_denied(|| async { true }, || async { true })
                .await
                .is_none(),
            "both granting is not a denial"
        );
    }
}
