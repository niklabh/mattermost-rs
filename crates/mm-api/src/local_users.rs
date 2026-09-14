//! The user family on the local socket: the port of `api4/user_local.go`.
//!
//! `InitUserLocal` registers 26 route+method pairs. Seventeen of them are the **HTTP handlers**
//! reached through `APILocal` — the same Go function with `model.Session{Local: true}` — and are
//! wrapped here with [`local_session`] and nothing else. Nine are functions of their own, all
//! named `local*`, and each one is the HTTP handler with something *removed*:
//!
//! | Go | what the local version does not do |
//! |---|---|
//! | `localGetUsers` | no `ViewUsersRestrictions`, no per-arm permission gate, no `in_group`/`not_in_group`, no `sort=admin`/`display_name`, no `UpdateLastActivityAtIfNeeded` |
//! | `localGetUsersByIds` | no restrictions |
//! | `localGetUser` | no `UserCanSeeOtherUser`, no self branch, no activity touch |
//! | `localDeleteUser` | no permission gate, no self-deactivation guard, no `EnableAPIUserDeletion` check on `?permanent=true` |
//! | `localGetUserByUsername` | no restrictions, no existence-hiding 403 |
//! | `localGetUserByEmail` | no restrictions |
//! | `localGetUserByAuthData` | no `IsSystemAdmin` gate, no `UserCanSeeOtherUser` |
//! | `localGetUploadsForUser` | no `RequireUserId`, no self check — `me` is used as sent |
//! | `localPermanentDeleteAllUsers` | *not served*: see [D-600] |
//!
//! "Removed" is the whole story: none of them adds a branch. Every one reuses the tail of its
//! HTTP twin (`users::respond_with_user`, `users::serve_users`, …) so that the parts the two
//! functions share cannot drift, and the difference is the absence of a call rather than a
//! second copy of the same code.
//!
//! # Where a forward goes
//!
//! Every wrapped HTTP handler that cannot answer forwards through `crate::proxy::forward_to_go`,
//! which — since this family landed — checks for the [`crate::local::GoLocalSocket`] extension
//! the local router installs and forwards over the **socket** when it finds one. Before that,
//! sharing a handler with a forward branch was unsafe here: the branch dialled the port and Go
//! answered as `APISessionRequired`, a 401 for a request the socket answers. The local-only
//! handlers in this module forward through [`forward_over_unix`] directly.
//!
//! # `me` is nobody
//!
//! `RequireUserId` rewrites `me` to `Session().UserId`, which is the empty string here, so every
//! `/users/me…` path that goes through it is a 400 naming `user_id` — `GET`, `PUT` and `DELETE`
//! on `/users/me` alike. The one exception is `localGetUploadsForUser`, which never calls
//! `RequireUserId`: `GET /users/me/uploads` over the socket lists the uploads of a user whose id
//! is the three bytes `me`, which is `[]`.

use axum::Router;
use axum::extract::{Extension, Path as UrlPath, RawQuery, Request, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use mm_model::utils::is_valid_id;

use crate::auth_writes::OptionalSession;
use crate::channels::{ME, query_first};
use crate::error::ApiError;
use crate::local::{
    GoLocalSocket, forward_over_unix, local_session, partially_migrated,
    partially_migrated_with_ids,
};
use crate::users::{ActivityUpdate, Branch, GetUsersVariant};
use crate::{
    AppState, auth_writes, migrate_auth, tokens, uploads, user_auth, user_convert, user_creates,
    user_deletes, user_updates, users,
};

/// The registrations of `InitUserLocal` (api4/user_local.go:20), merged into
/// [`crate::local::router`].
///
/// `DELETE /api/v4/users` is deliberately absent: the `GET`/`POST` method router on that path
/// falls through [`partially_migrated`] to the socket, so Go answers it — see [D-600].
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        // `Users.Handle("", localGetUsers)` GET, `("", createUser)` POST; the DELETE
        // (`localPermanentDeleteAllUsers`) is the method fallback's.
        .route(
            "/api/v4/users",
            partially_migrated(get(local_get_users).post(local_create_user)),
        )
        .route(
            "/api/v4/users/password/reset/send",
            partially_migrated(post(local_send_password_reset)),
        )
        .route(
            "/api/v4/users/ids",
            partially_migrated(post(local_get_users_by_ids)),
        )
        // `BaseRoutes.User` — `/users/{user_id:[A-Za-z0-9]+}` — and its three methods. Every
        // literal registered beside it in this function (`ids`, `auth_data`, `tokens`,
        // `migrate_auth`, `password`, `username`, `email`) is preferred by matchit, exactly as
        // gorilla prefers the literal it registered; a method those literals do not carry falls
        // to the fallback and Go's own local mux answers it through `{user_id}`.
        .route(
            "/api/v4/users/{user_id}",
            partially_migrated_with_ids(
                state,
                get(local_get_user)
                    .put(local_update_user)
                    .delete(local_delete_user),
            ),
        )
        .route(
            "/api/v4/users/{user_id}/roles",
            partially_migrated_with_ids(state, put(local_update_user_roles)),
        )
        .route(
            "/api/v4/users/{user_id}/mfa",
            partially_migrated_with_ids(state, put(local_update_user_mfa)),
        )
        .route(
            "/api/v4/users/{user_id}/active",
            partially_migrated_with_ids(state, put(local_update_user_active)),
        )
        .route(
            "/api/v4/users/{user_id}/password",
            partially_migrated_with_ids(state, put(local_update_password)),
        )
        .route(
            "/api/v4/users/{user_id}/convert_to_bot",
            partially_migrated_with_ids(state, post(local_convert_user_to_bot)),
        )
        .route(
            "/api/v4/users/{user_id}/email/verify/member",
            partially_migrated_with_ids(state, post(local_verify_user_email_without_token)),
        )
        .route(
            "/api/v4/users/{user_id}/promote",
            partially_migrated_with_ids(state, post(local_promote_guest_to_user)),
        )
        .route(
            "/api/v4/users/{user_id}/demote",
            partially_migrated_with_ids(state, post(local_demote_user_to_guest)),
        )
        .route(
            "/api/v4/users/{user_id}/auth",
            partially_migrated_with_ids(state, put(local_update_user_auth)),
        )
        // `{username:[A-Za-z0-9\_\-\.]+}` is not id-shaped, so the id-charset middleware must not
        // apply; the handler carries the username class and its own socket forward.
        .route(
            "/api/v4/users/username/{username}",
            partially_migrated(get(local_get_user_by_username)),
        )
        // `PathPrefix("/email/{email:.+}")` — a catch-all, as on the TCP router.
        .route(
            "/api/v4/users/email/{*email}",
            partially_migrated(get(local_get_user_by_email)),
        )
        .route(
            "/api/v4/users/auth_data",
            partially_migrated(get(local_get_user_by_auth_data)),
        )
        .route(
            "/api/v4/users/tokens/revoke",
            partially_migrated(post(local_revoke_user_access_token)),
        )
        .route(
            "/api/v4/users/{user_id}/tokens",
            partially_migrated_with_ids(
                state,
                get(local_get_user_access_tokens_for_user).post(local_create_user_access_token),
            ),
        )
        .route(
            "/api/v4/users/migrate_auth/ldap",
            partially_migrated(post(local_migrate_auth_to_ldap)),
        )
        .route(
            "/api/v4/users/migrate_auth/saml",
            partially_migrated(post(local_migrate_auth_to_saml)),
        )
        .route(
            "/api/v4/users/{user_id}/uploads",
            partially_migrated_with_ids(state, get(local_get_uploads_for_user)),
        )
}

// ---------------------------------------------------------------------------------------------
// The nine `local*` handlers
// ---------------------------------------------------------------------------------------------

/// The `sort` values `localGetUsers` accepts (api4/user_local.go:145) — three, where the HTTP
/// handler also takes `admin` and `display_name`.
const LOCAL_SORTS: &[&str] = &["last_activity_at", "create_at", "status"];

/// The dispatch of `localGetUsers` (api4/user_local.go:196-231): `without_team`, then the four
/// membership filters in the HTTP handler's order, then everything. **`in_group` and
/// `not_in_group` are not read** — the local function never looks at them, so a request naming
/// only a group lists every user.
fn local_branch(query: &users::GetUsersQuery) -> Branch {
    if query.without_team {
        Branch::WithoutTeam
    } else if !query.not_in_channel.is_empty() {
        Branch::NotInChannel
    } else if !query.not_in_team.is_empty() {
        Branch::NotInTeam
    } else if !query.in_team.is_empty() {
        Branch::InTeam
    } else if !query.in_channel.is_empty() {
        Branch::InChannel
    } else {
        Branch::All
    }
}

/// `localGetUsers`'s own four 400s (api4/user_local.go:140-161), after the role parameters and
/// before the dispatch. Returns the parameter `SetInvalidURLParam` names.
///
/// The combination rule reads the **raw** `without_team` string, not the parsed boolean —
/// `withoutTeam != ""` — so `?sort=create_at&in_team=X&without_team=no` is a 400 even though
/// `no` parses as false everywhere else in the handler.
fn local_get_users_refusal(
    query: &users::GetUsersQuery,
    raw_without_team: &str,
) -> Option<&'static str> {
    if !query.not_in_channel.is_empty() && query.in_team.is_empty() {
        return Some("team_id");
    }
    let sort = query.sort.as_str();
    if !sort.is_empty() && !LOCAL_SORTS.contains(&sort) {
        return Some("sort");
    }
    if (sort == "last_activity_at" || sort == "create_at")
        && (query.in_team.is_empty()
            || !query.not_in_team.is_empty()
            || !query.in_channel.is_empty()
            || !query.not_in_channel.is_empty()
            || !raw_without_team.is_empty())
    {
        return Some("sort");
    }
    if sort == "status" && query.in_channel.is_empty() {
        return Some("sort");
    }
    None
}

/// Why a `localGetUsers` request is handed to Go rather than answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalForward {
    /// `role`, `roles`, `channel_roles` or `team_roles` — validated against `GetAllRoles` with
    /// three distinct 400s and, for the plural three, then **ignored** (the local
    /// `UserGetOptions` carries only `Role`). The HTTP port forwards the same set.
    RoleFilter,
    /// A `sort` that passed validation reaches `GetRecentlyActiveUsersForTeamPage`,
    /// `GetNewUsersForTeamPage` or `GetUsersInChannelPageByStatus`, none of which is ported —
    /// and the first two are called with `c.Params.TeamId`, the *path* parameter this route
    /// does not have, so Go lists the empty team. Forwarded so that stays Go's answer.
    Sort,
    /// `GetUsersWithoutTeamPage` is not ported.
    WithoutTeam,
    /// `group_constrained` on the two `not_in_*` arms reaches a join this port lacks.
    GroupConstrained,
}

fn local_forward_reason(query: &users::GetUsersQuery, branch: Branch) -> Option<LocalForward> {
    if !query.role.is_empty()
        || !query.roles.is_empty()
        || !query.channel_roles.is_empty()
        || !query.team_roles.is_empty()
    {
        return Some(LocalForward::RoleFilter);
    }
    if !query.sort.is_empty() {
        return Some(LocalForward::Sort);
    }
    match branch {
        Branch::WithoutTeam => Some(LocalForward::WithoutTeam),
        Branch::NotInChannel | Branch::NotInTeam if query.group_constrained => {
            Some(LocalForward::GroupConstrained)
        }
        _ => None,
    }
}

/// Port of `localGetUsers` (api4/user_local.go:55).
///
/// The order is the local function's, which is **not** the HTTP one's: the role parameters are
/// validated first (forwarded here, whole), then the `team_id` 400, then the three `sort`
/// rules, then the dispatch. `inactive=true&active=true` is *not* a 400 on this router — the
/// HTTP handler's check is absent — and both flags reach the store.
///
/// The arms this port serves run through [`users::serve_users`] as [`GetUsersVariant::Local`]:
/// no permission gate, so an `in_channel` naming no channel is `[]` rather than the 403 the
/// port gives, and no activity touch. `IsSystemAdmin()` is true for the local session, so every
/// profile is sanitised with the admin flags.
#[tracing::instrument(skip_all, fields(branch, forwarded))]
async fn local_get_users(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    let parsed = users::parse_get_users_request(query.as_deref());
    let branch = local_branch(&parsed);
    tracing::Span::current().record("branch", tracing::field::debug(branch));

    // The role parameters come first in Go, so a request carrying one gets Go's validation of
    // it before any of the checks below — which forwarding first reproduces.
    if matches!(
        local_forward_reason(&parsed, branch),
        Some(LocalForward::RoleFilter)
    ) {
        tracing::Span::current().record("forwarded", "role_filter");
        return forward_over_unix(&go.0, request).await;
    }

    let raw_without_team = query_first(query.as_deref(), "without_team").unwrap_or_default();
    if let Some(parameter) = local_get_users_refusal(&parsed, &raw_without_team) {
        return ApiError::invalid_url_param(parameter).into_response();
    }

    if let Some(reason) = local_forward_reason(&parsed, branch) {
        tracing::Span::current().record("forwarded", tracing::field::debug(reason));
        return forward_over_unix(&go.0, request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match users::serve_users(
        &state,
        &headers,
        &local_session(),
        &parsed,
        branch,
        GetUsersVariant::Local,
    )
    .await
    {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localGetUsersByIds` (api4/user_local.go:245): `getUsersByIds` minus the
/// `GetViewUsersRestrictions` call, with `IsAdmin: c.IsSystemAdmin()` true.
#[tracing::instrument(skip_all, fields(count))]
async fn local_get_users_by_ids(State(state): State<AppState>, request: Request) -> Response {
    match users::serve_users_by_ids(&state, &local_session(), request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// `RequireUserId` (web/context.go:301) for a session whose `UserId` is empty: `me` becomes the
/// empty string, and anything that is not a valid id is a 400 naming `user_id`.
///
/// Unlike the TCP `get_user`, nothing is forwarded for a non-id segment: the literal siblings
/// Go's local router owns are registered in [`routes`] and win by path, so what reaches this
/// check is what reaches Go's `localGetUser`, which 400s it.
#[allow(clippy::result_large_err)]
fn require_local_user_id(user_id: &str) -> Result<&str, ApiError> {
    let user_id = if user_id == ME { "" } else { user_id };
    if !is_valid_id(user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }
    Ok(user_id)
}

/// Port of `localGetUser` (api4/user_local.go:283): `RequireUserId`, `GetUser`, then the
/// terms-of-service, etag and `SanitizeProfile(user, IsSystemAdmin())` tail — with no
/// `UserCanSeeOtherUser`, no `Sanitize` self branch (the local user id equals nobody's) and no
/// `UpdateLastActivityAtIfNeeded`.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
async fn local_get_user(
    State(state): State<AppState>,
    UrlPath(user_id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let user_id = match require_local_user_id(&user_id) {
        Ok(user_id) => user_id,
        Err(err) => return err.into_response(),
    };
    let user = match state.app.get_user(user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };
    match users::respond_with_user(
        &state,
        &headers,
        &local_session(),
        user,
        ActivityUpdate::Skip,
    )
    .await
    {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localDeleteUser` (api4/user_local.go:320).
///
/// `RequireUserId`, `GetUser`, then `PermanentDeleteUser` or `UpdateActive(user, false)` —
/// **no** `EnableAPIUserDeletion` check on the permanent arm, unlike `deleteUser`: the socket
/// erases accounts whether or not the API is allowed to. `PermanentDeleteUser` is not ported
/// ([D-470]) and is forwarded after the `GetUser`, which Go repeats. The soft arm is
/// `deleteUser`'s own tail: the bot-owner forward ([D-461]) and then the deactivation.
#[tracing::instrument(skip_all, fields(user_id, permanent, forwarded = false))]
async fn local_delete_user(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(user_id): UrlPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    let user_id = match require_local_user_id(&user_id) {
        Ok(user_id) => user_id,
        Err(err) => return err.into_response(),
    };
    tracing::Span::current().record("user_id", user_id);
    let permanent = user_deletes::parse_go_bool(
        &query_first(query.as_deref(), "permanent").unwrap_or_default(),
    );
    tracing::Span::current().record("permanent", permanent);

    let user = match state.app.get_user(user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if permanent {
        tracing::Span::current().record("forwarded", true);
        return forward_over_unix(&go.0, request).await;
    }

    match state.app.owns_bots(user_id).await {
        Ok(true) => {
            tracing::Span::current().record("forwarded", true);
            return forward_over_unix(&go.0, request).await;
        }
        Ok(false) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }
    if let Err(err) = state.app.deactivate_user(&user).await {
        return ApiError::from(err).into_response();
    }
    user_updates::status_ok()
}

/// Port of `localGetUserByUsername` (api4/user_local.go:359): `RequireUsername`, the lookup,
/// and the shared tail. The HTTP handler's restrictions fast path and its existence-hiding 403
/// are the parts that are missing.
#[tracing::instrument(skip_all, fields(username = %username, forwarded = false))]
async fn local_get_user_by_username(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    UrlPath(username): UrlPath<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    // Outside gorilla's username class the local mux never matched, so Go's 404 is the answer.
    if !users::segment_matches_username_mux(&username) {
        tracing::Span::current().record("forwarded", true);
        return forward_over_unix(&go.0, request).await;
    }
    if !mm_model::user::is_valid_username(&username) {
        return ApiError::invalid_param("username").into_response();
    }
    let user = match state.app.get_user_by_username(&username).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };
    match users::respond_with_user(
        &state,
        &headers,
        &local_session(),
        user,
        ActivityUpdate::Skip,
    )
    .await
    {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localGetUserByEmail` (api4/user_local.go:392): `SanitizeEmail`, the
/// `GetSanitizeOptions(IsSystemAdmin())["email"]` gate — which the local session always passes,
/// being a system admin to that check — the lookup, and the shared tail.
#[tracing::instrument(skip_all)]
async fn local_get_user_by_email(
    State(state): State<AppState>,
    UrlPath(email): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let session = local_session();
    let email = match users::email_lookup_prologue(&state, &session, &email).await {
        Ok(email) => email,
        Err(err) => return err.into_response(),
    };
    let user = match state.app.get_user_by_email(&email).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };
    users::respond_user_by_email(&state, &headers, &session, user).await
}

/// Port of `localGetUserByAuthData` (api4/user_local.go:424): the `value` checks, the lookup,
/// and the shared tail — with neither the `IsSystemAdmin` gate nor `UserCanSeeOtherUser`.
#[tracing::instrument(skip_all, fields(user_id))]
async fn local_get_user_by_auth_data(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let auth_data = match users::auth_data_value(query.as_deref()) {
        Ok(value) => value,
        Err(err) => return err.into_response(),
    };
    let user = match state.app.get_user_by_auth_data(&auth_data).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("user_id", user.id.as_str());
    match users::respond_user_by_auth_data(&state, &headers, user).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `localGetUploadsForUser` (api4/user_local.go:469): the segment as sent, no
/// `RequireUserId`, no self check. See the module docs for what that does to `me`.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
async fn local_get_uploads_for_user(
    State(state): State<AppState>,
    UrlPath(user_id): UrlPath<String>,
) -> Response {
    match uploads::uploads_for_user_response(&state, &user_id).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// The seventeen HTTP handlers through `APILocal`
// ---------------------------------------------------------------------------------------------

/// `createUser` through `APILocal` (user_local.go:22). An `APIHandler` on the port, so it takes
/// an optional session; here the session is present and local, which makes `IsSystemAdmin()`
/// true — the socket creates through `CreateUserAsAdmin`, with `SanitizeInput(true)` keeping
/// the roles and `email_verified` a signup would lose.
async fn local_create_user(state: State<AppState>, request: Request) -> Response {
    user_creates::create_user(state, OptionalSession(Some(local_session().0)), request).await
}

/// `sendPasswordReset` through `APILocal` (user_local.go:23) — no session read anywhere in it.
async fn local_send_password_reset(state: State<AppState>, request: Request) -> Response {
    user_creates::send_password_reset(state, request).await
}

/// `updateUser` through `APILocal` (user_local.go:27). `me` is a 400; the e-mail-change
/// password check is skipped, being `Session().UserId == c.Params.UserId`, and
/// `UpdateUserAsUser(user, IsSystemAdmin())` runs as an admin.
async fn local_update_user(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    user_updates::update_user(state, path, local_session(), request).await
}

/// `updateUserRoles` through `APILocal` (user_local.go:29).
async fn local_update_user_roles(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    user_updates::update_user_roles(state, path, local_session(), request).await
}

/// `updateUserMfa` through `APILocal` (user_local.go:30). Go's `MFARequired` check is skipped
/// on `!Session().Local` (api4/user.go:1930) — the only handler in the family that names the
/// flag — and this server does not make that check at all, so the two agree by absence.
async fn local_update_user_mfa(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    user_auth::update_user_mfa(state, path, local_session(), request).await
}

/// `updateUserActive` through `APILocal` (user_local.go:31). `isSelfDeactivate` compares the
/// target with the empty local user id and is therefore never true: the socket can deactivate
/// anyone, and never trips the `EnableUserDeactivation` guard or the deactivation e-mail.
async fn local_update_user_active(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    user_updates::update_user_active(state, path, local_session(), request).await
}

/// `updatePassword` through `APILocal` (user_local.go:32). The self arm (`current_password`)
/// is unreachable; every target takes the `canUpdatePassword` arm, which the local session
/// satisfies for admins, bots and users alike.
async fn local_update_password(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    auth_writes::update_password(state, path, local_session(), request).await
}

/// `convertUserToBot` through `APILocal` (user_local.go:33).
///
/// A converted account is a bot **owned by itself** (`model.BotFromUser`), and deactivating one
/// sends Go's `userDeactivated` → `disableUserBots` → `UpdateBotActive` → `UpdateActive` round
/// without end — measured on 2026-09-14, when a `DELETE /users/{id}?permanent=true` on such an
/// account never returned and `Users.UpdateAt` kept moving until the `Bots` row was marked
/// deleted by hand. This port's `owns_bots` forward ([`local_delete_user`], `updateUserActive`)
/// hands exactly that request to Go, so it reproduces the hang rather than a different answer.
async fn local_convert_user_to_bot(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    user_convert::convert_user_to_bot(state, path, local_session(), request).await
}

/// `verifyUserEmailWithoutToken` through `APILocal` (user_local.go:34).
async fn local_verify_user_email_without_token(
    state: State<AppState>,
    path: UrlPath<String>,
) -> Response {
    user_creates::verify_user_email_without_token(state, path, local_session()).await
}

/// `promoteGuestToUser` through `APILocal` (user_local.go:35). The requestor recorded against
/// the default channels is `Session().UserId` — the empty string, on this transport.
async fn local_promote_guest_to_user(state: State<AppState>, path: UrlPath<String>) -> Response {
    user_convert::promote_guest_to_user(state, path, local_session()).await
}

/// `demoteUserToGuest` through `APILocal` (user_local.go:36).
async fn local_demote_user_to_guest(state: State<AppState>, path: UrlPath<String>) -> Response {
    user_convert::demote_user_to_guest(state, path, local_session()).await
}

/// `updateUserAuth` through `APILocal` (user_local.go:38).
async fn local_update_user_auth(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    user_auth::update_user_auth(state, path, local_session(), request).await
}

/// `revokeUserAccessToken` through `APILocal` (user_local.go:44).
async fn local_revoke_user_access_token(state: State<AppState>, request: Request) -> Response {
    tokens::revoke_user_access_token(state, local_session(), request).await
}

/// `getUserAccessTokensForUser` through `APILocal` (user_local.go:45).
async fn local_get_user_access_tokens_for_user(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Result<Response, ApiError> {
    tokens::get_user_access_tokens_for_user(state, path, query, local_session()).await
}

/// `createUserAccessToken` through `APILocal` (user_local.go:46). Both permission checks —
/// `create_user_access_token` on the caller and `SessionHasPermissionToUserOrBot` on the
/// target — short-circuit on the local session, so the socket mints for anyone; the admin
/// target's extra `manage_system` check passes the same way.
async fn local_create_user_access_token(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    tokens::create_user_access_token(state, path, local_session(), request).await
}

/// `migrateAuthToLDAP` through `APILocal` (user_local.go:48).
async fn local_migrate_auth_to_ldap(state: State<AppState>, request: Request) -> Response {
    migrate_auth::migrate_auth_to_ldap(state, local_session(), request).await
}

/// `migrateAuthToSaml` through `APILocal` (user_local.go:49).
async fn local_migrate_auth_to_saml(state: State<AppState>, request: Request) -> Response {
    migrate_auth::migrate_auth_to_saml(state, local_session(), request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pairs: &str) -> users::GetUsersQuery {
        users::parse_get_users_request(Some(pairs))
    }

    /// `in_group` and `not_in_group` are not parameters of the local function.
    #[test]
    fn a_group_filter_is_ignored_on_the_socket() {
        assert_eq!(local_branch(&query("in_group=abc")), Branch::All);
        assert_eq!(local_branch(&query("not_in_group=abc")), Branch::All);
        assert_eq!(local_branch(&query("in_channel=abc")), Branch::InChannel);
        assert_eq!(
            local_branch(&query("without_team=true&in_team=abc")),
            Branch::WithoutTeam
        );
    }

    /// The four refusals, in the local function's order, and the two `sort` values only the
    /// HTTP handler accepts.
    #[test]
    fn the_local_refusals_match_user_local_go() {
        assert_eq!(
            local_get_users_refusal(&query("not_in_channel=c"), ""),
            Some("team_id")
        );
        assert_eq!(
            local_get_users_refusal(&query("not_in_channel=c&in_team=t"), ""),
            None
        );
        for sort in ["admin", "display_name", "bogus"] {
            assert_eq!(
                local_get_users_refusal(&query(&format!("sort={sort}&in_channel=c")), ""),
                Some("sort"),
                "{sort}"
            );
        }
        assert_eq!(
            local_get_users_refusal(&query("sort=create_at"), ""),
            Some("sort"),
            "needs in_team"
        );
        assert_eq!(
            local_get_users_refusal(&query("sort=create_at&in_team=t"), ""),
            None
        );
        // The raw string, not the parsed flag.
        assert_eq!(
            local_get_users_refusal(&query("sort=create_at&in_team=t&without_team=no"), "no"),
            Some("sort")
        );
        assert_eq!(
            local_get_users_refusal(&query("sort=status"), ""),
            Some("sort")
        );
        assert_eq!(
            local_get_users_refusal(&query("sort=status&in_channel=c"), ""),
            None
        );
        // Not a 400 here: the `inactive && active` check is the HTTP handler's.
        assert_eq!(
            local_get_users_refusal(&query("inactive=true&active=true"), ""),
            None
        );
    }

    /// What is handed to Go, and what is not.
    #[test]
    fn the_local_forwards_are_the_unported_arms() {
        let reason = |q: &str| {
            let parsed = query(q);
            local_forward_reason(&parsed, local_branch(&parsed))
        };
        assert_eq!(reason("role=system_admin"), Some(LocalForward::RoleFilter));
        assert_eq!(
            reason("channel_roles=channel_admin"),
            Some(LocalForward::RoleFilter)
        );
        assert_eq!(reason("sort=status&in_channel=c"), Some(LocalForward::Sort));
        assert_eq!(reason("without_team=true"), Some(LocalForward::WithoutTeam));
        assert_eq!(
            reason("not_in_team=t&group_constrained=true"),
            Some(LocalForward::GroupConstrained)
        );
        assert_eq!(reason("in_team=t&group_constrained=true"), None);
        assert_eq!(reason("in_group=g"), None);
        assert_eq!(reason("inactive=true&active=true"), None);
    }

    /// `me` is the empty string on this transport, and the empty string is not an id.
    #[test]
    fn me_and_non_ids_are_refused_as_url_params() {
        assert!(require_local_user_id("me").is_err());
        assert!(require_local_user_id("").is_err());
        assert!(require_local_user_id("stats").is_err());
        assert_eq!(
            require_local_user_id("abcdefghijklmnopqrstuvwxyz").ok(),
            Some("abcdefghijklmnopqrstuvwxyz")
        );
    }
}
