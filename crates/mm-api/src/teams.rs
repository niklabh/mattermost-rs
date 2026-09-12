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
//! - `getAllTeams` — `GET /api/v4/teams`
//! - `getTeamMembersByIds` — `POST /api/v4/teams/{team_id}/members/ids`
//! - `updateTeamPrivacy` — `PUT /api/v4/teams/{team_id}/privacy`
//! - `searchTeams` — `POST /api/v4/teams/search`
//! - `createTeam` — `POST /api/v4/teams`
//! - `invalidateAllEmailInvites` — `DELETE /api/v4/teams/invites/email`

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::team::TeamWrite;
use mm_model::permission::{
    PERMISSION_CREATE_TEAM, PERMISSION_INVALIDATE_EMAIL_INVITE, PERMISSION_INVITE_USER,
    PERMISSION_MANAGE_TEAM, PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_PERMISSIONS,
};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_LIST_PRIVATE_TEAMS, PERMISSION_LIST_PUBLIC_TEAMS,
    PERMISSION_MANAGE_SYSTEM, PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS, PERMISSION_VIEW_TEAM, make_permission_error,
};
use mm_model::team::{Team, TeamPatch};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;
use mm_model::utils::decode_one_from_json;
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
        ApiError::from(mm_model::utils::AppError::new(
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
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS],
        )));
    }

    let mut teams = state.app.get_teams_for_user(&user_id).await?;
    tracing::Span::current().record("count", teams.len());

    state.app.sanitize_teams(&session.0, &mut teams).await;

    let body = serde_json::to_vec(&teams).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the team list");
        ApiError::from(mm_model::utils::AppError::new(
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
    ApiError::from(make_permission_error(
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
        Err(err) => return ApiError::from(err).into_response(),
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
            return ApiError::from(mm_model::utils::AppError::new(
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
        Err(err) => return ApiError::from(err).into_response(),
    };

    let mut body = match serde_json::to_vec(&stats) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise TeamStats");
            return ApiError::from(mm_model::utils::AppError::new(
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
pub(crate) fn segment_matches_team_name_mux(value: &str) -> bool {
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

/// Port of `teamExists` (api4/team.go:376) — `GET /api/v4/teams/name/{team_name}/exists`.
///
/// The join and signup flows ask this before offering a team, and it is the one team read that
/// **never 404s**: a name that matches nothing, and a team the caller may not see, are the same
/// answer — `{"exists":false}` with a 200.
///
/// # Three ways to be visible, and they are not `getTeamByName`'s
///
/// ```go
/// (teamMember != nil && teamMember.DeleteAt == 0) ||
/// (team.AllowOpenInvite && SessionHasPermissionTo(list_public_teams)) ||
/// (!team.AllowOpenInvite && SessionHasPermissionTo(list_private_teams))
/// ```
///
/// Note what is **not** there. `getTeamByName` guards on `AllowOpenInvite || Type != TeamOpen`
/// and falls back to `view_team` *on the team*; this one ignores `Type` entirely and asks for a
/// **system-level** list permission instead. So a private team the caller is not in exists for an
/// admin (who holds `list_private_teams`) and does not exist for anybody else — and a public
/// team with open invite off is equally invisible, because the second branch reads
/// `AllowOpenInvite`, not the type.
///
/// A **left** membership does not count: `DeleteAt == 0` is checked on the member row, so a user
/// who left a private team stops being able to see that it exists.
///
/// # Two errors that are swallowed and one that is not
///
/// Both lookups propagate only when `StatusCode != 404`. A missing team and a missing membership
/// are ordinary, so a broken query is the only thing that reaches the client — as a 500 with the
/// store's own id.
///
/// # Wire format
///
/// `w.Write([]byte(model.MapBoolToJSON(resp)))` — `json.Marshal` of a one-key `map[string]bool`,
/// so **no trailing newline** ([D-086] again). Measured on the running server.
#[tracing::instrument(skip_all, fields(team_name = %team_name, forwarded))]
pub async fn team_exists(
    State(state): State<AppState>,
    Path(team_name): Path<String>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    // `BaseRoutes.TeamByName` is `{team_name:[A-Za-z0-9_-]+}`; anything else is gorilla's own
    // 404 rather than this handler's 400. Unlike `getTeamByName` there is nothing to shadow —
    // the `Team` subrouter registers no `/{x}/exists`.
    if !segment_matches_team_name_mux(&team_name) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    // `params.TeamName = strings.ToLower(props["team_name"])` (web/params.go:178) — **every**
    // route with a `{team_name}` segment gets it lowercased before any handler sees it, so
    // `/teams/name/MMRS-PARITY-X` is the same request as the lowercase one. Rust's
    // `to_lowercase` is the full Unicode mapping where Go's is the simple one, but the mux check
    // above has already restricted this segment to `[A-Za-z0-9_-]`, where the two agree. The
    // same treatment `get_channel_by_name_for_team_name` already gave it.
    let team_name = team_name.to_lowercase();

    if !mm_model::team::is_valid_team_name(&team_name) {
        return ApiError::invalid_url_param("team_name").into_response();
    }

    let team = match state.app.get_team_by_name(&team_name).await {
        Ok(team) => Some(team),
        Err(err) if err.status_code == 404 => None,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let mut exists = false;
    if let Some(team) = team {
        let member = match state
            .app
            .get_team_member(&team.id, &session.0.user_id)
            .await
        {
            Ok(member) => Some(member),
            Err(err) if err.status_code == 404 => None,
            Err(err) => return ApiError::from(err).into_response(),
        };

        let is_current_member = member.is_some_and(|member| member.delete_at == 0);
        exists = is_current_member
            || if team.allow_open_invite {
                state
                    .app
                    .session_has_permission_to(&session.0, &PERMISSION_LIST_PUBLIC_TEAMS)
                    .await
            } else {
                state
                    .app
                    .session_has_permission_to(&session.0, &PERMISSION_LIST_PRIVATE_TEAMS)
                    .await
            };
    }

    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        format!("{{\"exists\":{exists}}}").into_bytes(),
    )
        .into_response()
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

    // `params.TeamName = strings.ToLower(props["team_name"])` (web/params.go:178) — **every**
    // route with a `{team_name}` segment gets it lowercased before any handler sees it, so
    // `/teams/name/MMRS-PARITY-X` is the same request as the lowercase one. Rust's
    // `to_lowercase` is the full Unicode mapping where Go's is the simple one, but the mux check
    // above has already restricted this segment to `[A-Za-z0-9_-]`, where the two agree. The
    // same treatment `get_channel_by_name_for_team_name` already gave it.
    let team_name = team_name.to_lowercase();

    if !mm_model::team::is_valid_team_name(&team_name) {
        return ApiError::invalid_url_param("team_name").into_response();
    }

    let mut team = match state.app.get_team_by_name(&team_name).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(err).into_response(),
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
            return ApiError::from(mm_model::utils::AppError::new(
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
        Err(err) => return ApiError::from(err).into_response(),
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
            return ApiError::from(mm_model::utils::AppError::new(
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
        Err(err) => return ApiError::from(err).into_response(),
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
            return ApiError::from(mm_model::utils::AppError::new(
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
        return ApiError::from(make_permission_error(
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
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("count", unreads.len());

    let body = match serde_json::to_vec(&unreads) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the team unread list");
            return ApiError::from(mm_model::utils::AppError::new(
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
        return ApiError::from(make_permission_error(&session.0, &[permission])).into_response();
    }

    let unread = match state.app.get_team_unread(&team_id, &user_id).await {
        Ok(unread) => unread,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let mut body = match serde_json::to_vec(&unread) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the team unread");
            return ApiError::from(mm_model::utils::AppError::new(
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

/// The two ways [`get_all_teams`] refuses, which are **not** the same 403.
///
/// Go writes one of them with `c.SetPermissionError` and the other by hand, and the difference is
/// visible to a client: `api.context.permissions.app_error` versus
/// `api.team.get_all_teams.insufficient_permissions`. Both measured against the running server.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AllTeamsDenial {
    /// `exclude_policy_constrained` without `sysconsole_read_compliance_data_retention_policy`.
    /// The ordinary `SetPermissionError` shape, naming that permission.
    RetentionPolicyRead,
    /// Neither `list_private_teams` nor `list_public_teams`. Go builds this `AppError` inline
    /// (api4/team.go:1477) with its own id and an empty detail — the one refusal on this route
    /// that does **not** go through `SetPermissionError`.
    NeitherListPermission,
}

/// Go's permission-to-query-options block for [`get_all_teams`] (api4/team.go:1447-1478), lifted
/// out whole so all four cells of the private/public matrix and both refusals are unit-testable
/// without a database.
///
/// The four combinations, and what a reader would plausibly get wrong about each:
///
/// | `list_private` | `list_public` | `allow_open_invite` | note |
/// |---|---|---|---|
/// | yes | yes | unset | no filter at all — **archived teams included**, because the store has no `DeleteAt` predicate |
/// | yes | no | `Some(false)` | private-only. A team whose `allowopeninvite` column is NULL matches neither value and is absent from *both* single-permission listings |
/// | no | yes | `Some(true)` | public-only, and the only branch that can set `include_policy_enforced` |
/// | no | no | — | [`AllTeamsDenial::NeitherListPermission`] |
///
/// Two orderings carry meaning and are pinned by tests:
///
/// - **`exclude_policy_constrained` is checked before anything else**, so a caller lacking the
///   retention-policy read gets that refusal even when it also lacks both list permissions. The
///   ids differ, so swapping these two blocks is visible on the wire.
/// - **`include_policy_id` is set from the same permission, unconditionally.** Go evaluates
///   `SessionHasPermissionTo(...ComplianceDataRetentionPolicy)` *twice* — once inside the
///   `exclude_policy_constrained` branch and once after it. The call is a pure role lookup, so
///   this port polls once and reuses the answer; the only observable difference would be the
///   number of role reads.
///
/// `include_policy_enforced` is the ABAC widening, reachable only when
/// [`mm_app::App::team_membership_access_control_enabled`] is true — which it never is on this
/// Team Edition deployment. The parameter is threaded through anyway so the branch is exercised
/// by a unit test rather than left unwritten.
pub(crate) fn all_teams_opts(
    exclude_policy_constrained: bool,
    can_read_retention_policy: bool,
    list_private: bool,
    list_public: bool,
    membership_access_control_enabled: bool,
) -> Result<mm_model::team_search::TeamSearch, AllTeamsDenial> {
    let mut opts = mm_model::team_search::TeamSearch::default();

    if exclude_policy_constrained {
        if !can_read_retention_policy {
            return Err(AllTeamsDenial::RetentionPolicyRead);
        }
        opts.exclude_policy_constrained = Some(true);
    }

    if can_read_retention_policy {
        opts.include_policy_id = Some(true);
    }

    match (list_private, list_public) {
        (true, true) => {}
        (true, false) => opts.allow_open_invite = Some(false),
        (false, true) => {
            opts.allow_open_invite = Some(true);
            if membership_access_control_enabled {
                opts.include_policy_enforced = Some(true);
            }
        }
        (false, false) => return Err(AllTeamsDenial::NeitherListPermission),
    }

    Ok(opts)
}

/// Port of `getAllTeams` (api4/team.go:1443), reached as `GET /api/v4/teams`.
///
/// The webapp's "join another team" page calls this, and the System Console calls it with
/// `include_total_count=true`.
///
/// # `per_page=0` means an empty page on this route
///
/// `web.ParamsFromRequest` treats `0` as a legitimate value (params.go:234) and squirrel renders
/// `Limit(0)` as a literal `LIMIT 0`, so `?per_page=0` returns `[]`. The *same* parameter on
/// `getChannelMembers` means "no limit" because that store guards with `Limit > 0`. Both
/// measured against the running Go server; see [`mm_store::team_store::get_all_page`].
///
/// # The response shape switches on a query flag
///
/// `include_total_count=true` produces `{"teams": [...], "total_count": N}`; anything else
/// produces a bare array. `total_count` comes from a **different query** with a different filter
/// set — it ignores `exclude_policy_constrained` entirely — so the two halves of that object can
/// legitimately disagree. Go's asymmetry, reproduced.
///
/// # What is deliberately not here
///
/// Go follows the store read with `FilterNonQualifyingTeamsForUser` and
/// `AnnotateRecommendedTeamsForUser`, gated behind `for_directory` / `manage_system`. Both
/// functions return immediately unless `TeamMembershipAccessControlEnabled()` — which is
/// **false** on this deployment for the reasons set out on
/// [`mm_app::App::team_membership_access_control_enabled`] — so the whole block is a no-op and is
/// omitted rather than written blind. `for_directory=true` is therefore accepted and ignored,
/// exactly as Go does with ABAC off; that equivalence is measured over HTTP, the ABAC-on
/// behaviour is not, and nothing here should be taken as a claim about it.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:1530), so **no trailing newline** — measured byte for
/// byte, same call-site rule as [`get_teams_for_user`] and unlike the channel lists ([D-086]).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count))]
pub async fn get_all_teams(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Response, ApiError> {
    let query = query.as_deref();

    let can_read_retention_policy = state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
        )
        .await;
    let list_private = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_LIST_PRIVATE_TEAMS)
        .await;
    let list_public = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_LIST_PUBLIC_TEAMS)
        .await;

    let opts = all_teams_opts(
        crate::channels::query_flag_is_true(query, "exclude_policy_constrained"),
        can_read_retention_policy,
        list_private,
        list_public,
        state.app.team_membership_access_control_enabled(),
    )
    .map_err(|denial| match denial {
        AllTeamsDenial::RetentionPolicyRead => ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY],
        )),
        AllTeamsDenial::NeitherListPermission => ApiError::from(mm_model::utils::AppError::new(
            "getAllTeams",
            "api.team.get_all_teams.insufficient_permissions",
            None,
            String::new(),
            403,
        )),
    })?;

    // `limit := c.Params.PerPage; offset := limit * c.Params.Page` (team.go:1462) — the
    // multiplication wraps in Go too, and both servers then hand the database a value it
    // rejects, so an absurd `page` is a 500 on both sides rather than a divergence.
    let limit = crate::channels::parse_per_page(query);
    let offset = crate::channels::page_offset(crate::channels::parse_page(query), limit);

    let body = if crate::channels::query_flag_is_true(query, "include_total_count") {
        let mut result = state
            .app
            .get_all_teams_page_with_count(offset, limit, &opts)
            .await?;
        tracing::Span::current().record("count", result.teams.len());
        state
            .app
            .sanitize_teams(&session.0, &mut result.teams)
            .await;
        serialised_team_listing("getAllTeams", &result)?
    } else {
        let mut teams = state.app.get_all_teams_page(offset, limit, &opts).await?;
        tracing::Span::current().record("count", teams.len());
        state.app.sanitize_teams(&session.0, &mut teams).await;
        serialised_team_listing("getAllTeams", &teams)?
    };

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

/// `json.Marshal` for either shape [`get_all_teams`] or [`search_teams`] can return, with Go's
/// own failure id. `where_` is the handler name, which differs between the two call sites.
///
/// Neither `[]*model.Team` nor `model.TeamsWithCount` can fail to marshal, so the 500 is the
/// branch that keeps this crate free of `unwrap` rather than a behaviour claim — the same shape
/// as `encoded_channel_list`.
#[allow(clippy::result_large_err)]
fn serialised_team_listing<T: serde::Serialize>(
    where_: &'static str,
    value: &T,
) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the team listing");
        ApiError::from(mm_model::utils::AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })
}

/// Port of `getTeamMembersByIds` (api4/team.go:926), reached as
/// `POST /api/v4/teams/{team_id}/members/ids`.
///
/// [`get_team_members`] with the page swapped for a body, and the same three gates in the same
/// order — but two of its answers differ, both in the store:
///
/// - **`DeleteAt = 0` is applied**, as it is for the paginated list, so a departed member is
///   absent rather than present-and-blanked.
/// - **No `LIMIT`, no `ORDER BY`** — the `per_page=0`-is-empty trap that separates this route's
///   paginated sibling from `getChannelMembers` has no analogue here, because there is no limit
///   clause to guard.
///
/// # Order of operations
///
/// 1. `RequireTeamId`, then `SortedArrayFromJSON` and the empty check naming `user_ids`.
/// 2. `SessionHasPermissionToTeam(view_team)` → 403 naming `view_team`, **after** both 400s.
/// 3. `GetViewUsersRestrictions` — nil iff the caller holds user-based `view_members`; a
///    restricted caller is forwarded whole, the same rule as `getTeamStats`, `getTeamMembers`
///    and `getUsersByIds`. The forward happens **first** here, before the body is read, because
///    forwarding needs the body intact; Go re-runs both 400s and the gate itself, so nothing
///    observable moves.
/// 4. `SanitizeRoleData` over every element unless the caller holds `manage_team_roles` — the
///    same mid-list blanking as the paginated sibling, `delete_at: -1` on every row but the
///    caller's own.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:966) — **no trailing newline**, matching
/// [`get_team_members`] and diverging from its channel counterpart
/// `mm_api::channels::get_channel_members_by_ids`, which encodes. Two handlers with the same
/// request shape and a one-byte difference in the reply; [D-086].
#[tracing::instrument(skip_all, fields(team_id = %team_id, asked, forwarded))]
pub async fn get_team_members_by_ids(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
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

    match serve_team_members_by_ids(&state, &team_id, &session, request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_team_members_by_ids(
    state: &AppState,
    team_id: &str,
    session: &AuthenticatedSession,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    require_id(team_id, "team_id")?;

    let bytes = crate::channels::read_body(request, "getTeamMembersByIds").await?;
    let user_ids = crate::channels::ids_from_body(&bytes, "user_ids", "getTeamMembersByIds")?;
    tracing::Span::current().record("asked", user_ids.len());

    if !state
        .app
        .session_has_permission_to_team(&session.0, team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return Err(get_team_denial(&session.0));
    }

    let mut members = state
        .app
        .get_team_members_by_ids(team_id, &user_ids)
        .await?;

    let can_manage_roles = state
        .app
        .session_has_permission_to_team(
            &session.0,
            team_id,
            &mm_model::permission::PERMISSION_MANAGE_TEAM_ROLES,
        )
        .await;
    if !can_manage_roles {
        for member in &mut members {
            member.sanitize_role_data(&session.0.user_id);
        }
    }

    let body = serde_json::to_vec(&members).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the member list");
        ApiError::from(mm_model::utils::AppError::new(
            "getTeamMembersByIds",
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

/// Port of `updateTeam` (api4/team.go) — `PUT /api/v4/teams/{team_id}`.
///
/// # Two permissions, and the second depends on what changed
///
/// `manage_team` always. Then **`invite_user`, but only if `AllowOpenInvite` or `AllowedDomains`
/// differ from the stored team** — so the same request is allowed or refused depending on a value
/// the caller may not have meant to change. A client that round-trips a fetched team and edits
/// only its display name never trips it; one that omits `allow_open_invite` from the body sends
/// `false` and may.
///
/// # The email is lower-cased before the id comparison
///
/// `team.Email = strings.ToLower(team.Email)` is the handler's first statement after the decode,
/// and it happens even though `Email` is one of the fields the app layer then **discards**.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn update_team(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(team_id): Path<String>,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("team").into_response();
        }
    };
    let mut team: Team = match serde_json::from_slice(&bytes) {
        Ok(team) => team,
        Err(err) => {
            tracing::debug!(error = %err, "team body did not decode");
            return ApiError::invalid_param("team").into_response();
        }
    };

    // `strings.ToLower`, which is the *simple* case mapping — see `go_to_lower`.
    team.email = mm_model::utils::go_to_lower(&team.email);

    // **`SetInvalidParam("id")`, not `"team_id"`.** The parameter Go names here is the body's
    // field, not the path's segment, and a client branching on `params.Name` sees the difference.
    if team.id != team_id {
        return ApiError::invalid_param("id").into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM],
        ))
        .into_response();
    }

    let old_team = match state.app.get_team(&team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if (team.allow_open_invite != old_team.allow_open_invite
        || team.allowed_domains != old_team.allowed_domains)
        && !state
            .app
            .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_INVITE_USER)
            .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_INVITE_USER],
        ))
        .into_response();
    }

    match state.app.update_team(&team).await {
        Ok(updated) => sanitized_team_response(&state, &session, updated).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `patchTeam` (api4/team.go) — `PUT /api/v4/teams/{team_id}/patch`.
///
/// The same two permissions, but the second is decided by **presence** rather than by change:
/// `patch.AllowOpenInvite != nil || patch.AllowedDomains != nil`. So sending
/// `{"allow_open_invite": <the value it already has>}` needs `invite_user` where the update route
/// would not.
///
/// Both permission checks run **before** the team is fetched, which is the opposite order from
/// `updateTeam` — so a patch naming a nonexistent team answers 403 rather than 404 for a caller
/// without the permission.
#[tracing::instrument(skip_all, fields(team_id = %team_id, forwarded))]
pub async fn patch_team(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(team_id): Path<String>,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("team").into_response();
        }
    };
    let patch: TeamPatch = match serde_json::from_slice(&bytes) {
        Ok(patch) => patch,
        Err(err) => {
            tracing::debug!(error = %err, "team patch body did not decode");
            return ApiError::invalid_param("team").into_response();
        }
    };

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM],
        ))
        .into_response();
    }

    if (patch.allow_open_invite.is_some() || patch.allowed_domains.is_some())
        && !state
            .app
            .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_INVITE_USER)
            .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_INVITE_USER],
        ))
        .into_response();
    }

    match state.app.patch_team(&team_id, &patch).await {
        Ok(TeamWrite::Done(patched)) => {
            tracing::Span::current().record("forwarded", false);
            sanitized_team_response(&state, &session, *patched).await
        }
        Ok(TeamWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(?why, "handing the team patch to Go");
            let request = Request::from_parts(parts, Body::from(bytes));
            crate::proxy::forward_to_go(State(state), request).await
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `restoreTeam` (api4/team.go) — `POST /api/v4/teams/{team_id}/restore`.
///
/// The body is a **second fetch**, not the struct that was written — Go re-reads the team "to be
/// consistent with RestoreChannel". So a concurrent update between the two is visible in the
/// answer.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn restore_team(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(team_id): Path<String>,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM],
        ))
        .into_response();
    }

    if let Err(err) = state.app.restore_team(&team_id).await {
        return ApiError::from(err).into_response();
    }

    match state.app.get_team(&team_id).await {
        Ok(team) => sanitized_team_response(&state, &session, team).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `invalidateAllEmailInvites` (api4/team.go:2016) —
/// `DELETE /api/v4/teams/invites/email`.
///
/// One permission, `invalidate_email_invite`, and no parameters at all — the route is
/// server-wide, not team-scoped, despite living under `/teams`. Every outstanding team and guest
/// invitation on the installation is voided and every pending resend job is cancelled.
///
/// # Wire format
///
/// `ReturnStatusOK` — `{"status":"OK"}`, no trailing newline.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn invalidate_all_email_invites(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_INVALIDATE_EMAIL_INVITE)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_INVALIDATE_EMAIL_INVITE],
        ))
        .into_response();
    }

    match state.app.invalidate_all_email_invites().await {
        Ok(()) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            r#"{"status":"OK"}"#,
        )
            .into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `creatorCanInviteUsersOnTeam` (api4/team.go:158).
///
/// "Will the creator hold `invite_user` on the team once it exists?" — asked **twice** by
/// `createTeam`, once as a gate and once to decide whether the reply carries an invite id, and
/// the second call sees the *stored* team rather than the submitted one.
///
/// Three arms, in order:
///
/// 1. the session already holds `invite_user` at **system** scope → yes, without reading anything;
/// 2. the team names a non-empty scheme → the scheme's `DefaultTeamUserRole` and
///    `DefaultTeamAdminRole` are asked. **A scheme that cannot be read is a `false`, not an
///    error** — Go logs and carries on, so an unreadable scheme silently strips the invite id
///    from the reply rather than failing the create;
/// 3. otherwise the built-in `team_user` and `team_admin`.
///
/// The last arm is why a create normally succeeds with an invite id: `team_user` grants
/// `invite_user` on a stock server, and the creator is about to become a team member. The
/// creator's *session* does not reflect that yet, which is exactly why Go asks the roles rather
/// than the session.
async fn creator_can_invite_users_on_team(
    state: &AppState,
    session: &AuthenticatedSession,
    team: &Team,
) -> bool {
    if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_INVITE_USER)
        .await
    {
        return true;
    }

    let roles = match team.scheme_id.as_deref().filter(|id| !id.is_empty()) {
        Some(scheme_id) => match state.app.get_scheme(scheme_id).await {
            Ok(scheme) => vec![
                scheme.default_team_user_role.clone(),
                scheme.default_team_admin_role.clone(),
            ],
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    scheme_id,
                    "Failed to fetch scheme while checking invite permission for new team"
                );
                return false;
            }
        },
        None => vec![
            mm_model::role::TEAM_USER_ROLE_ID.to_owned(),
            mm_model::role::TEAM_ADMIN_ROLE_ID.to_owned(),
        ],
    };

    state
        .app
        .roles_grant_permission(&roles, &PERMISSION_INVITE_USER.id)
        .await
}

/// Port of `createTeam` (api4/team.go:80) — `POST /api/v4/teams`.
///
/// # The refusal for "you may not create teams" is **not** a permission error
///
/// `model.NewAppError("createTeam", "api.team.is_team_creation_allowed.disabled.app_error", nil,
/// "", http.StatusForbidden)` — a 403 with an id that names team *creation being disabled*, where
/// every other gate on this route calls `SetPermissionError`. A client branching on the id sees
/// the difference, and `SetPermissionError` would also fill `params` with a permission name this
/// answer does not carry.
///
/// # The submitted `email` is discarded
///
/// The handler lower-cases it, and then `CreateTeamWithUser` overwrites the field with the
/// creator's own address before anything is written. So the lower-casing is dead on this route —
/// reproduced anyway, because it is one `strings.ToLower` away from mattering if the assignment
/// ever moves.
///
/// # The reply's invite id is decided a second time
///
/// After the team exists, `creatorCanInviteUsersOnTeam(rteam)` runs **again** and blanks
/// `InviteId` when it says no. So the 201 body of a create by a caller who cannot invite carries
/// `"invite_id": ""` — the team has one, this caller is simply not shown it. Dropping that second
/// call hands out a working invite link.
///
/// # What is deliberately not here
///
/// - `PrivacySettings.UseAnonymousURLs`, which randomises the team's name. Go ANDs it with
///   `MinimumEnterpriseAdvancedLicense`, false on this Team Edition deployment for the reasons
///   set out on [`mm_app::App::team_membership_access_control_enabled`], so the branch is dark.
/// - The cloud team-limit check, gated on `License().IsCloud()` — likewise false.
///
/// Both are read from the Go source and never exercised; nothing here is a claim about them.
///
/// # Wire format
///
/// **201**, and `json.NewEncoder` leaves a trailing newline.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, team_id))]
pub async fn create_team(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("team").into_response();
        }
    };
    let mut team: Team = match decode_one_from_json::<Option<Team>>(&bytes) {
        Ok(decoded) => decoded.unwrap_or_default(),
        Err(err) => {
            tracing::debug!(error = %err, "team body did not decode");
            return ApiError::invalid_param("team").into_response();
        }
    };

    // `strings.ToLower`, the *simple* mapping — see `go_to_lower`. Overwritten downstream; see
    // the note above.
    team.email = mm_model::utils::go_to_lower(&team.email);

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_CREATE_TEAM)
        .await
    {
        return ApiError::from(mm_model::utils::AppError::new(
            "createTeam",
            "api.team.is_team_creation_allowed.disabled.app_error",
            None,
            String::new(),
            403,
        ))
        .into_response();
    }

    // `team.SchemeId != nil` — **presence**, so `{"scheme_id": ""}` needs the permission too.
    if team.scheme_id.is_some()
        && !state
            .app
            .session_has_permission_to(
                &session.0,
                &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_PERMISSIONS,
            )
            .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_PERMISSIONS],
        ))
        .into_response();
    }

    // Matching `updateTeam`/`patchTeam`: asking for open invitations or an allowed-domains list
    // needs `invite_user`. `AllowedDomains != ""` is a value check where `SchemeId` above is a
    // presence check, one line apart.
    if (team.allow_open_invite || !team.allowed_domains.is_empty())
        && !creator_can_invite_users_on_team(&state, &session, &team).await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_INVITE_USER],
        ))
        .into_response();
    }

    let mut created = match state
        .app
        .create_team_with_user(&mut team, &session.0.user_id)
        .await
    {
        Ok(created) => created,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("team_id", &created.id);

    if !creator_can_invite_users_on_team(&state, &session, &created).await {
        created.invite_id = String::new();
    }

    match mm_model::utils::go_json_marshal(&created) {
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

/// Which store search `searchTeams` runs, once the permissions have been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeamSearchPlan {
    /// Both list permissions — the only arm that can paginate.
    All,
    /// `list_private` alone.
    Private,
    /// `list_public` alone.
    Public,
    /// Neither: Go does not call the store at all and answers with an empty list.
    Neither,
}

/// The three ways `searchTeams` refuses before it searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeamSearchDenial {
    /// `exclude_policy_constrained` **present** without the retention-policy read permission.
    RetentionPolicyRead,
    /// A paginated request on the private-only branch — 501, not 400.
    PaginationPrivate,
    /// A paginated request on the public-only branch — 501, with a different id.
    PaginationPublic,
}

/// The decision half of `searchTeams` (api4/team.go:1539), lifted so every branch is testable
/// without a database. `opts` is **mutated**, exactly as Go mutates `props`.
///
/// # The order of the five steps is on the wire
///
/// 1. **`exclude_policy_constrained` is checked for presence, not truth.** Go writes
///    `props.ExcludePolicyConstrained != nil`, so `{"exclude_policy_constrained": false}` needs
///    the retention-policy permission just as much as `true` does. A port that checks the value
///    lets an unprivileged caller send the field.
/// 2. **`policy_id` is cleared unconditionally**, whatever the caller sent. Go's comment: it "may
///    only be used through the /data_retention/policies endpoint". Dropping this line lets any
///    caller filter the team directory by a retention policy id.
/// 3. `include_policy_id` is set from the same permission — so a compliance reader gets
///    `policy_id` projected onto every team in the answer.
/// 4. The list-permission matrix picks the store call. **Only the both-permissions arm may
///    paginate**; the two single-permission arms answer **501** for a request carrying `page` or
///    `per_page` — either one alone is enough, where the response *shape* later needs both.
///    That asymmetry is Go's and is the easiest thing here to get wrong.
/// 5. Neither permission is not an error: Go sets `teams = []` and falls through to the same
///    sanitise-and-encode path, so the answer is `[]` with a 200.
///
/// `include_policy_enforced` is the ABAC widening on the public arm, gated behind
/// [`mm_app::App::team_membership_access_control_enabled`] — a constant `false` here, so the
/// parameter exists to keep the branch exercised by a unit test rather than unwritten.
pub(crate) fn team_search_plan(
    opts: &mut mm_model::team_search::TeamSearch,
    can_read_retention_policy: bool,
    list_private: bool,
    list_public: bool,
    membership_access_control_enabled: bool,
) -> Result<TeamSearchPlan, TeamSearchDenial> {
    if opts.exclude_policy_constrained.is_some() && !can_read_retention_policy {
        return Err(TeamSearchDenial::RetentionPolicyRead);
    }

    opts.policy_id = None;

    if can_read_retention_policy {
        opts.include_policy_id = Some(true);
    }

    // Go's `props.Page != nil || props.PerPage != nil` — **or**, not the `IsPaginated()` **and**
    // that decides the response shape further down.
    let any_pagination_field = opts.page.is_some() || opts.per_page.is_some();

    match (list_private, list_public) {
        (true, true) => Ok(TeamSearchPlan::All),
        (true, false) => {
            if any_pagination_field {
                return Err(TeamSearchDenial::PaginationPrivate);
            }
            Ok(TeamSearchPlan::Private)
        }
        (false, true) => {
            if any_pagination_field {
                return Err(TeamSearchDenial::PaginationPublic);
            }
            if membership_access_control_enabled {
                opts.include_policy_enforced = Some(true);
            }
            Ok(TeamSearchPlan::Public)
        }
        (false, false) => Ok(TeamSearchPlan::Neither),
    }
}

/// Port of `searchTeams` (api4/team.go:1539) — `POST /api/v4/teams/search`.
///
/// The decision table is [`team_search_plan`]; what is left here is the store call, the
/// sanitiser and the two response shapes.
///
/// # The response shape and the pagination refusal disagree on purpose
///
/// The body is `{"teams": [...], "total_count": N}` only when **both** `page` and `per_page` are
/// present, and a bare array otherwise — but the 501 on the single-permission arms fires when
/// **either** is present. So `{"page": 0}` from a caller holding only `list_public` is a 501,
/// while the same body from a caller holding both is a 200 carrying a bare array.
///
/// # `total_count` on the unpaginated path is the page length
///
/// `SearchAllTeams` returns `int64(len(results))` when it did not page, so the `total_count` a
/// caller sees without `per_page` describes the slice it already has. Unreachable through the
/// shape branch above, which needs both fields; kept because the app layer is shared.
///
/// # What is deliberately not here
///
/// `FilterNonQualifyingTeamsForUser` and `AnnotateRecommendedTeamsForUser`, both of which return
/// immediately unless `TeamMembershipAccessControlEnabled()` — false on this deployment, same
/// reasoning as [`get_all_teams`]. The `manage_system` permission that gates them is therefore
/// not read at all; with ABAC on it would have to be.
///
/// # Wire format
///
/// `w.Write(payload)` (team.go:1619) — **no trailing newline**, on either shape.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, plan, count))]
pub async fn search_teams(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("team_search").into_response();
        }
    };
    // `json.Decoder.Decode` reads one value and stops, and a `null` body leaves the struct at its
    // zero value rather than erroring — both of which `decode_one_from_json` into an `Option`
    // reproduce. An outright syntax error is `SetInvalidParamWithErr("team_search")`.
    let mut opts: mm_model::team_search::TeamSearch =
        match decode_one_from_json::<Option<mm_model::team_search::TeamSearch>>(&bytes) {
            Ok(decoded) => decoded.unwrap_or_default(),
            Err(err) => {
                tracing::debug!(error = %err, "team search body did not decode");
                return ApiError::invalid_param("team_search").into_response();
            }
        };

    // Go polls this permission twice — once inside the `exclude_policy_constrained` branch and
    // once after it. A pure role lookup, so one poll here; only the number of reads differs.
    let can_read_retention_policy = state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
        )
        .await;
    let list_private = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_LIST_PRIVATE_TEAMS)
        .await;
    let list_public = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_LIST_PUBLIC_TEAMS)
        .await;

    let plan = match team_search_plan(
        &mut opts,
        can_read_retention_policy,
        list_private,
        list_public,
        state.app.team_membership_access_control_enabled(),
    ) {
        Ok(plan) => plan,
        Err(TeamSearchDenial::RetentionPolicyRead) => {
            return ApiError::from(*make_permission_error(
                &session.0,
                &[&PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY],
            ))
            .into_response();
        }
        Err(denial) => {
            let id = if denial == TeamSearchDenial::PaginationPrivate {
                "api.team.search_teams.pagination_not_implemented.private_team_search"
            } else {
                "api.team.search_teams.pagination_not_implemented.public_team_search"
            };
            return ApiError::from(mm_model::utils::AppError::new(
                "searchTeams",
                id,
                None,
                String::new(),
                501,
            ))
            .into_response();
        }
    };
    tracing::Span::current().record("plan", format!("{plan:?}"));

    let searched = match plan {
        TeamSearchPlan::All => state.app.search_all_teams(&opts).await,
        TeamSearchPlan::Private => state
            .app
            .search_private_teams(&opts)
            .await
            .map(|teams| (teams, 0)),
        TeamSearchPlan::Public => state
            .app
            .search_public_teams(&opts)
            .await
            .map(|teams| (teams, 0)),
        // Go never calls the store here; `totalCount` stays at its zero value.
        TeamSearchPlan::Neither => Ok((Vec::new(), 0)),
    };

    let (mut teams, total_count) = match searched {
        Ok(result) => result,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("count", teams.len());

    state.app.sanitize_teams(&session.0, &mut teams).await;

    // The shape needs **both** fields, unlike the 501 above, which needs either.
    let body = if opts.is_paginated() {
        serialised_team_listing(
            "searchTeams",
            &mm_model::team::TeamsWithCount { teams, total_count },
        )
    } else {
        serialised_team_listing("searchTeams", &teams)
    };

    match body {
        Ok(body) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
        Err(err) => err.into_response(),
    }
}

/// The body of `updateTeamPrivacy` (api4/team.go:593), lifted so every rejection branch is
/// testable without a server.
///
/// Go reads the body with `model.StringInterfaceFromJSON`, which **swallows every decode
/// failure**: a malformed body, a JSON array, a bare `null` all become an empty map and then fall
/// out of the `props["privacy"].(string)` type assertion as `SetInvalidParam("privacy")`. There
/// is no "malformed body" answer on this route — one 400 covers all of it, naming `privacy`.
///
/// `json.Decoder.Decode` also reads **one** value and ignores whatever follows, so
/// `{"privacy":"O"} junk` is accepted; hence [`decode_one_from_json`] rather than `from_slice`.
///
/// The two accepted strings each fix *both* outputs — `O` is open with `allow_open_invite = true`
/// and `I` is invite-only with `false` — which is why the pair is returned together rather than
/// recomputed downstream. Anything else, including `"o"`, `"P"` or a non-string, is the same 400.
fn team_privacy_from_body(bytes: &[u8]) -> Option<(&'static str, bool)> {
    let value = decode_one_from_json::<Option<serde_json::Value>>(bytes)
        .ok()
        .flatten()?;
    match value.as_object()?.get("privacy")?.as_str()? {
        mm_model::team::TEAM_OPEN => Some((mm_model::team::TEAM_OPEN, true)),
        mm_model::team::TEAM_INVITE => Some((mm_model::team::TEAM_INVITE, false)),
        _ => None,
    }
}

/// Port of `updateTeamPrivacy` (api4/team.go:588) — `PUT /api/v4/teams/{team_id}/privacy`.
///
/// # Order: the body is parsed **before** either permission is checked
///
/// So a caller with neither permission sending `{"privacy":"X"}` gets the 400 naming `privacy`,
/// not a 403 — the opposite of [`patch_team`], where both permissions precede the fetch. Swapping
/// the parse and the permission check is invisible to a well-formed request and visible to every
/// malformed one.
///
/// Then `manage_team`, then `invite_user`, both team-scoped and both unconditional — the same
/// pair [`regenerate_team_invite_id`] requires, and for the same reason: this route can mint a
/// new invite id as a side effect.
///
/// # The reply is a second read
///
/// `UpdateTeamPrivacy` returns nothing; Go re-fetches "to be consistent with
/// UpdateChannelPrivacy" and sanitises *that*. The body is therefore not the struct that was
/// written, and `json.NewEncoder` gives it a trailing newline.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn update_team_privacy(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(team_id): Path<String>,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        // Go's decoder sees a truncated stream and `StringInterfaceFromJSON` swallows it into an
        // empty map, so a read failure lands on the same 400 as a malformed body.
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("privacy").into_response();
        }
    };

    let Some((team_type, allow_open_invite)) = team_privacy_from_body(&bytes) else {
        return ApiError::invalid_param("privacy").into_response();
    };

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM],
        ))
        .into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_INVITE_USER)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_INVITE_USER],
        ))
        .into_response();
    }

    if let Err(err) = state
        .app
        .update_team_privacy(&team_id, team_type, allow_open_invite)
        .await
    {
        return ApiError::from(err).into_response();
    }

    match state.app.get_team(&team_id).await {
        Ok(team) => sanitized_team_response(&state, &session, team).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `regenerateTeamInviteId` (api4/team.go) —
/// `POST /api/v4/teams/{team_id}/regenerate_invite_id`.
///
/// **Both** `manage_team` and `invite_user`, unconditionally — the only team write that requires
/// the second without looking at what changed.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn regenerate_team_invite_id(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(team_id): Path<String>,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM],
        ))
        .into_response();
    }
    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_INVITE_USER)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_INVITE_USER],
        ))
        .into_response();
    }

    match state.app.regenerate_team_invite_id(&team_id).await {
        Ok(team) => sanitized_team_response(&state, &session, team).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `SanitizeTeam` then `json.NewEncoder(w).Encode` — the trailing newline every team write shares.
///
/// The sanitiser is the **session-aware** one, not `Team::sanitize`: it clears `Email` and
/// `InviteId` only for a caller without `manage_system` on the team, which is why an admin's
/// answer carries them and a member's does not. The websocket event uses the *unconditional*
/// sanitiser instead, so the two disagree by design.
async fn sanitized_team_response(
    state: &AppState,
    session: &AuthenticatedSession,
    team: Team,
) -> Response {
    let mut team = team;
    state.app.sanitize_team(&session.0, &mut team).await;
    match mm_model::utils::go_json_marshal(&team) {
        Ok(json) => (
            StatusCode::OK,
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

/// Port of `getInviteInfo` (api4/team.go:1981) — `GET /api/v4/teams/invite/{invite_id}`.
///
/// # No session, and that is the point of the route
///
/// `APIHandler`, not `APISessionRequired` (team.go:75): the join-by-invite-link page shows a team's
/// name to someone who has not signed in yet. It is the third unauthenticated route this server
/// serves, and the only one that returns data rather than a refusal.
///
/// # A non-open team is a 403, and that answer confirms the invite is real
///
/// `team.Type != TeamOpen` gives `api.team.get_invite_info.not_open_team` with the invite id in
/// `detailed_error` — where an id that matches nothing gives a 404. So the pair distinguishes "no
/// such invite" from "that invite is for a closed team", which is Go's choice and is reproduced.
/// `WipeDetailed` removes the id from the body before a client sees it.
///
/// # The body is four fields, not a `Team`
///
/// An anonymous struct — `display_name`, `description`, `name`, `id`, **in that order** — so none
/// of `Team`'s twenty other fields, and none of its sanitisation, is involved. `email`,
/// `allowed_domains` and the invite id itself never leave the server through this route.
///
/// `json.NewEncoder(w).Encode`, so a trailing newline.
#[tracing::instrument(skip_all, fields(invite_id = %invite_id, team_type))]
pub async fn get_invite_info(
    State(state): State<AppState>,
    Path(invite_id): Path<String>,
) -> Result<Response, ApiError> {
    // `RequireInviteId` (web/context.go:344) tests **emptiness only** — not `IsValidId`. The mux
    // charset already rejects an empty segment, so this cannot fire through the router; it is
    // ported because the check is Go's and the next caller may not be a route.
    if invite_id.is_empty() {
        return Err(ApiError::invalid_url_param("invite_id"));
    }

    let team = state.app.get_team_by_invite_id(&invite_id).await?;
    tracing::Span::current().record("team_type", &team.team_type);

    if team.team_type != mm_model::team::TEAM_OPEN {
        return Err(ApiError::from(mm_model::utils::AppError::new(
            "getInviteInfo",
            "api.team.get_invite_info.not_open_team",
            None,
            format!("id={invite_id}"),
            403,
        )));
    }

    // The anonymous struct Go declares inline. Field order is the wire order.
    #[derive(serde::Serialize)]
    struct InviteInfo<'a> {
        display_name: &'a str,
        description: &'a str,
        name: &'a str,
        id: &'a str,
    }

    let mut body = serde_json::to_vec(&InviteInfo {
        display_name: &team.display_name,
        description: &team.description,
        name: &team.name,
        id: &team.id,
    })
    .map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the invite info");
        ApiError::from(mm_model::utils::AppError::new(
            "getInviteInfo",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok((
        axum::http::StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use mm_model::team_member::TeamMember;

    use super::team_privacy_from_body;
    use super::{
        AllTeamsDenial, TeamMembersGetOptions, Visibility, all_teams_opts, get_team_denial,
        segment_matches_team_name_mux, team_by_name_denied, team_is_public, team_members_options,
        team_name_is_shadowed_by_team_id_route, team_unread_denied, team_view_denied,
        user_visibility, validate_team_and_user_ids, wants_collapsed_threads,
    };
    use super::{TeamSearchDenial, TeamSearchPlan, team_search_plan};

    fn plan(
        opts: &mut mm_model::team_search::TeamSearch,
        retention: bool,
        private: bool,
        public: bool,
    ) -> Result<TeamSearchPlan, TeamSearchDenial> {
        team_search_plan(opts, retention, private, public, false)
    }

    /// The whole list-permission matrix. `Neither` is a **200 with an empty list**, not a 403 —
    /// the one cell a reader is most likely to turn into a refusal.
    #[test]
    fn the_list_matrix_picks_the_search_and_no_cell_refuses() {
        let cases = [
            (true, true, TeamSearchPlan::All),
            (true, false, TeamSearchPlan::Private),
            (false, true, TeamSearchPlan::Public),
            (false, false, TeamSearchPlan::Neither),
        ];
        for (private, public, expected) in cases {
            let mut opts = mm_model::team_search::TeamSearch::default();
            assert_eq!(plan(&mut opts, false, private, public), Ok(expected));
        }
    }

    /// Only the both-permissions arm may paginate, and **either** field alone trips the 501 —
    /// where the response shape further down needs both. Each single-permission arm has its own
    /// error id, so the two cannot be collapsed.
    #[test]
    fn either_pagination_field_alone_is_a_501_on_the_single_permission_arms() {
        for (page, per_page) in [(Some(0), None), (None, Some(10)), (Some(2), Some(10))] {
            let mut opts = mm_model::team_search::TeamSearch {
                page,
                per_page,
                ..Default::default()
            };
            assert_eq!(
                plan(&mut opts, false, true, false),
                Err(TeamSearchDenial::PaginationPrivate)
            );

            let mut opts = mm_model::team_search::TeamSearch {
                page,
                per_page,
                ..Default::default()
            };
            assert_eq!(
                plan(&mut opts, false, false, true),
                Err(TeamSearchDenial::PaginationPublic)
            );

            // The same body on the both-permissions arm is fine.
            let mut opts = mm_model::team_search::TeamSearch {
                page,
                per_page,
                ..Default::default()
            };
            assert_eq!(plan(&mut opts, false, true, true), Ok(TeamSearchPlan::All));

            // And on the no-permission arm, which never looks at pagination at all.
            let mut opts = mm_model::team_search::TeamSearch {
                page,
                per_page,
                ..Default::default()
            };
            assert_eq!(
                plan(&mut opts, false, false, false),
                Ok(TeamSearchPlan::Neither)
            );
        }
    }

    /// `ExcludePolicyConstrained != nil` — **presence**, so `false` is refused too. The refusal
    /// also precedes the pagination check, so a caller sending both gets the 403.
    #[test]
    fn exclude_policy_constrained_is_checked_for_presence_and_runs_first() {
        for value in [Some(true), Some(false)] {
            let mut opts = mm_model::team_search::TeamSearch {
                exclude_policy_constrained: value,
                page: Some(1),
                per_page: Some(10),
                ..Default::default()
            };
            assert_eq!(
                plan(&mut opts, false, true, false),
                Err(TeamSearchDenial::RetentionPolicyRead),
                "the 403 precedes the 501"
            );

            let mut opts = mm_model::team_search::TeamSearch {
                exclude_policy_constrained: value,
                ..Default::default()
            };
            assert_eq!(plan(&mut opts, true, true, true), Ok(TeamSearchPlan::All));
            assert_eq!(opts.exclude_policy_constrained, value, "carried through");
        }

        // Absent: no permission needed.
        let mut opts = mm_model::team_search::TeamSearch::default();
        assert_eq!(plan(&mut opts, false, true, true), Ok(TeamSearchPlan::All));
    }

    /// `props.PolicyID = nil` runs for everyone, permission or not — the field is reachable only
    /// through `/data_retention/policies`. And `include_policy_id` is set from the retention
    /// permission alone, on every arm including the one that never calls the store.
    #[test]
    fn policy_id_is_always_cleared_and_include_policy_id_follows_the_permission() {
        for retention in [false, true] {
            let mut opts = mm_model::team_search::TeamSearch {
                policy_id: Some("skjy5tackbqes3cwbzdoawkhtc".to_owned()),
                exclude_policy_constrained: None,
                ..Default::default()
            };
            let _ = plan(&mut opts, retention, true, true);
            assert_eq!(opts.policy_id, None, "cleared whatever the caller sent");
            assert_eq!(opts.include_policy_id, retention.then_some(true));
        }
    }

    /// The ABAC widening lands on the public arm only, and only with the licence gate on — which
    /// it never is on this deployment. Both halves asserted so neither can drift unnoticed.
    #[test]
    fn include_policy_enforced_is_the_public_arm_with_abac_on() {
        let mut opts = mm_model::team_search::TeamSearch::default();
        assert_eq!(
            team_search_plan(&mut opts, false, false, true, true),
            Ok(TeamSearchPlan::Public)
        );
        assert_eq!(opts.include_policy_enforced, Some(true));

        for (private, public) in [(true, true), (true, false), (false, false)] {
            let mut opts = mm_model::team_search::TeamSearch::default();
            let _ = team_search_plan(&mut opts, false, private, public, true);
            assert_eq!(
                opts.include_policy_enforced, None,
                "only the public-only arm widens"
            );
        }

        let mut opts = mm_model::team_search::TeamSearch::default();
        let _ = team_search_plan(&mut opts, false, false, true, false);
        assert_eq!(
            opts.include_policy_enforced, None,
            "the licence gate is off on this deployment"
        );
    }

    /// Go's own answer for every input the `switch privacy` sees, including the three near-misses
    /// a reader is most likely to accept by accident: lower case, the *channel* privacy letter
    /// `P`, and a leading space.
    #[test]
    fn go_parity_the_privacy_switch() {
        let raw = include_str!("../../../fixtures/behaviour_team_privacy.json");
        let corpus: serde_json::Value = serde_json::from_str(raw).unwrap();
        for row in corpus["privacy_switch"].as_array().unwrap() {
            let privacy = row["privacy"].as_str().unwrap();
            let body = serde_json::json!({ "privacy": privacy }).to_string();
            let got = team_privacy_from_body(body.as_bytes());
            if row["accepted"].as_bool().unwrap() {
                assert_eq!(
                    got,
                    Some((
                        row["team_type"].as_str().unwrap(),
                        row["open_invite"].as_bool().unwrap()
                    )),
                    "privacy {privacy:?}"
                );
            } else {
                assert_eq!(got, None, "privacy {privacy:?} must be a 400");
            }
        }
    }

    /// `StringInterfaceFromJSON` swallows every decode failure into an empty map, so all of these
    /// land on one 400 naming `privacy` — there is no distinct malformed-body answer.
    #[test]
    fn every_unusable_body_is_the_same_rejection() {
        for body in [
            &b""[..],
            b"not json",
            b"[]",
            b"null",
            b"{}",
            br#"{"privacy":null}"#,
            br#"{"privacy":1}"#,
            br#"{"privacy":true}"#,
            br#"{"privacy":["O"]}"#,
            br#"{"Privacy":"O"}"#,
            br#"{"privacy":"O""#,
        ] {
            assert_eq!(
                team_privacy_from_body(body),
                None,
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// `json.Decoder.Decode` reads one value and ignores the rest, so a body with trailing junk
    /// is accepted. `serde_json::from_slice` would reject it — the reason this uses
    /// `decode_one_from_json`.
    #[test]
    fn trailing_junk_after_the_first_value_is_ignored() {
        assert_eq!(
            team_privacy_from_body(br#"{"privacy":"I"} and then some"#),
            Some(("I", false))
        );
        assert_eq!(
            team_privacy_from_body(br#"{"privacy":"O"}{"privacy":"I"}"#),
            Some(("O", true)),
            "the second document is never read"
        );
    }

    /// `list_private && list_public` is Go's empty branch: **no** filter, so the listing spans
    /// public, private and archived teams alike.
    #[test]
    fn both_list_permissions_filter_nothing() {
        let opts = all_teams_opts(false, false, true, true, false).expect("granted");
        assert_eq!(opts.allow_open_invite, None);
        assert_eq!(opts.include_policy_enforced, None);
        assert_eq!(opts.exclude_policy_constrained, None);
        assert_eq!(opts.include_policy_id, None);
        assert_eq!(opts.include_deleted, None, "no DeleteAt filter is ever set");
    }

    /// Private-only asks for `AllowOpenInvite = false`, public-only for `true`. Crossing these
    /// two hands every private team to a caller holding only the *public* listing permission,
    /// which is the whole point of the pair.
    #[test]
    fn one_list_permission_picks_the_matching_open_invite_value() {
        assert_eq!(
            all_teams_opts(false, false, true, false, false)
                .expect("granted")
                .allow_open_invite,
            Some(false),
            "list_private_teams alone means the non-open-invite teams"
        );
        assert_eq!(
            all_teams_opts(false, false, false, true, false)
                .expect("granted")
                .allow_open_invite,
            Some(true),
            "list_public_teams alone means the open-invite teams"
        );
    }

    /// The ABAC widening is reachable **only** from the public-only branch, and only with team
    /// membership access control switched on. Everywhere else it stays unset — including the
    /// private-only branch, where setting it would surface governed teams to a caller Go never
    /// shows them to.
    #[test]
    fn include_policy_enforced_is_the_public_only_branch_with_abac_on() {
        assert_eq!(
            all_teams_opts(false, false, false, true, true)
                .expect("granted")
                .include_policy_enforced,
            Some(true)
        );
        for (private, public) in [(true, true), (true, false)] {
            assert_eq!(
                all_teams_opts(false, false, private, public, true)
                    .expect("granted")
                    .include_policy_enforced,
                None,
                "abac must not widen the ({private}, {public}) branch"
            );
        }
        assert_eq!(
            all_teams_opts(false, false, false, true, false)
                .expect("granted")
                .include_policy_enforced,
            None,
            "and with abac off the public-only branch does not widen either"
        );
    }

    /// Neither permission is the route's own 403, not `SetPermissionError`. The two ids are
    /// different strings on the wire and the webapp branches on `id`.
    #[test]
    fn neither_list_permission_is_its_own_refusal() {
        assert_eq!(
            all_teams_opts(false, true, false, false, false),
            Err(AllTeamsDenial::NeitherListPermission)
        );
    }

    /// `exclude_policy_constrained` is gated **before** the list-permission matrix, so a caller
    /// with neither list permission and no retention read gets the *retention* refusal. Moving
    /// the block below the matrix changes which error id a client sees.
    #[test]
    fn the_retention_gate_is_checked_before_the_list_matrix() {
        assert_eq!(
            all_teams_opts(true, false, false, false, false),
            Err(AllTeamsDenial::RetentionPolicyRead),
            "the retention refusal wins over the neither-permission one"
        );
        assert_eq!(
            all_teams_opts(true, false, true, true, false),
            Err(AllTeamsDenial::RetentionPolicyRead),
            "and it refuses a caller who could otherwise list everything"
        );
    }

    /// The same permission drives two independent options: it *gates* the exclusion and it
    /// *enables* `include_policy_id` unconditionally. Holding it without asking for the exclusion
    /// still sets the id, and that is the branch that puts a non-null `policy_id` on the wire.
    #[test]
    fn the_retention_permission_sets_include_policy_id_on_its_own() {
        let opts = all_teams_opts(false, true, true, true, false).expect("granted");
        assert_eq!(opts.include_policy_id, Some(true));
        assert_eq!(
            opts.exclude_policy_constrained, None,
            "the flag was not asked for, so the exclusion stays off"
        );

        let opts = all_teams_opts(true, true, true, true, false).expect("granted");
        assert_eq!(opts.exclude_policy_constrained, Some(true));
        assert_eq!(opts.include_policy_id, Some(true), "both, not either");

        let opts = all_teams_opts(false, false, true, true, false).expect("granted");
        assert_eq!(
            opts.include_policy_id, None,
            "without the permission there is no policy id column at all"
        );
    }

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
