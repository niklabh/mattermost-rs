//! Port of `getUser` (channels/api4/user.go:305), reached as `GET /api/v4/users/me` and
//! `GET /api/v4/users/{user_id}`; `getUserByUsername`; and `getUsersByIds`
//! (`POST /api/v4/users/ids`).

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_app::user::UserPage;
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_READ_CHANNEL, PERMISSION_VIEW_MEMBERS,
    PERMISSION_VIEW_TEAM, make_permission_error,
};
use mm_model::user::User;
use mm_model::utils::{AppError, PAYLOAD_PARSE_ERROR, is_valid_id, sorted_array_from_json};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `model.HeaderEtagServer`.
const HEADER_ETAG_SERVER: &str = "ETag";

/// The options map `SanitizeProfile` receives — `Config.GetSanitizeOptions` (config.go:5332)
/// merged with `UserService.GetSanitizeOptions`'s admin override (app/users/utils.go:48).
///
/// The config half maps the two privacy settings; the admin half force-enables **four** flags,
/// two of which (`authservice`, `authdata`) have no config source at all — a non-admin viewer
/// never sees them regardless of settings, because `Sanitize`'s populated-map mode strips every
/// unflagged field.
fn sanitize_options(
    show_full_name: bool,
    show_email_address: bool,
    as_admin: bool,
) -> HashMap<String, bool> {
    let mut options = HashMap::new();
    options.insert("fullname".to_owned(), show_full_name);
    options.insert("email".to_owned(), show_email_address);
    if as_admin {
        options.insert("email".to_owned(), true);
        options.insert("fullname".to_owned(), true);
        options.insert("authservice".to_owned(), true);
        options.insert("authdata".to_owned(), true);
    }
    options
}

/// The tail every `getUser` variant shares, from the fetched user to the response: the
/// terms-of-service branch, the etag and its 304, the sanitize split, and the encoder newline.
///
/// # Order matters twice
///
/// 1. **Terms of service lands on the user before the etag is computed** — `User.Etag`
///    interpolates `TermsOfServiceId`/`CreateAt` (user.go:692), so running the branch after
///    `HandleEtag` would 304 a response whose body should have changed when a user accepts a
///    ToS. The branch runs only for a **self-or-admin** viewer, and its 404 is a normal outcome
///    (nobody on Team Edition can author a ToS); any other error propagates (user.go:330).
/// 2. **The etag is computed before sanitisation**, from the unsanitised row — Go's order, and
///    harmless because the inputs (`update_at`, the ToS pair, the flags) survive every sanitize
///    mode.
///
/// # The sanitize split
///
/// Self gets `Sanitize(map[string]bool{})` — the empty map that reads backwards: `Sanitize`
/// guards its flag block behind `len(options) != 0` (user.go:702), so an **empty** map strips
/// only the four unconditional secrets and email/full name survive. Everyone else goes through
/// `SanitizeProfile` with [`sanitize_options`], where an absent flag strips its field — the
/// populated map is the strict mode. Getting the two backwards blanks every user's own email.
/// Measured against the running Go server, not inferred.
///
/// # Not ported
///
/// `UpdateLastActivityAtIfNeeded` — a write on the read path, deferred with the session cache it
/// belongs to (D-084). Privacy settings are the stand-ins from `AppState` (D-085).
async fn respond_with_user(
    state: &AppState,
    headers: &HeaderMap,
    session: &AuthenticatedSession,
    mut user: User,
) -> Result<Response, ApiError> {
    // `c.IsSystemAdmin()` is `SessionHasPermissionTo(manage_system)` (web/context.go:134) — the
    // session's roles, not the user row's.
    let is_admin = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;
    let is_self = session.0.user_id == user.id;

    if is_admin || is_self {
        match state.app.get_user_terms_of_service(&user.id).await {
            Ok(terms) => {
                user.terms_of_service_id = terms.terms_of_service_id;
                user.terms_of_service_create_at = terms.create_at;
            }
            // `err.StatusCode != http.StatusNotFound` is the propagation guard (user.go:330):
            // a missing row is the common case, not a failure.
            Err(err) if err.status_code == 404 => {}
            Err(err) => return Err(ApiError(err)),
        }
    }

    // `user.Etag(*c.App.Config().PrivacySettings.ShowFullName, *...ShowEmailAddress)`.
    let etag = user.etag(state.show_full_name, state.show_email_address);

    // Port of `Context.HandleEtag`. Go compares the raw header against the computed value; it
    // does not implement weak comparison or a list of candidates.
    if let Some(if_none_match) = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok())
        && if_none_match == etag
    {
        return Ok((StatusCode::NOT_MODIFIED, [(ETAG, etag)]).into_response());
    }

    if is_self {
        user.sanitize(&HashMap::new());
    } else {
        // `c.App.SanitizeProfile(user, c.IsSystemAdmin())` — `ClearNonProfileFields` first
        // (which additionally blanks auth data, notify props and failed attempts for a
        // non-admin), then `Sanitize` with the populated map.
        user.sanitize_profile(
            &sanitize_options(state.show_full_name, state.show_email_address, is_admin),
            is_admin,
        );
    }

    // Go writes the etag header and then `json.NewEncoder(w).Encode(user)` (user.go:353).
    //
    // `Encode` appends a newline; `json.Marshal` does not. That one byte was once the entire
    // difference between this response and Go's — see D-086.
    let mut body = serde_json::to_vec(&user).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise User");
        ApiError(mm_model::utils::AppError::new(
            "getUser",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            (HEADER_ETAG_SERVER, etag.as_str()),
            ("Content-Type", "application/json"),
            // The operator-facing marker the proxy sets to "go"; this route is the other case.
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Port of `getUser` for the `me` case.
///
/// The target is the session's own user, so `UserCanSeeOtherUser` is `true` by construction
/// (its first branch is the self comparison, app/user.go:2711) — the reasoning D-082 records.
/// Everything after the fetch is [`respond_with_user`], shared with the parameterised route;
/// the terms-of-service branch now runs here too, which closed D-083.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn get_user_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user = state.app.get_user(&session.0.user_id).await?;
    respond_with_user(&state, &headers, &session, user).await
}

/// Port of `getUser` (api4/user.go:305) for an explicit id — `GET /api/v4/users/{user_id}`.
///
/// # Anything that is not exactly an id is forwarded
///
/// The `/users/{user_id}` namespace is the most crowded in api4: `stats`, `known`,
/// `autocomplete` and `tokens` are all alphanumeric GET literals the running Go server routes to
/// their own handlers (measured — gorilla gives them precedence over `{user_id:[A-Za-z0-9]+}`),
/// and upstream adds more. Rather than enumerate a sibling list that drifts, this handler serves
/// only a segment that is **exactly a valid 26-character id** and forwards everything else — so
/// an invalid-id 400 is Go's own too, and a literal added upstream keeps working unported. The
/// same decision D-150 made for the charset, extended to the whole rule.
///
/// # The permission gate is `UserCanSeeOtherUser`, served on its nil-restrictions fast path
///
/// Go's check (app/user.go:2710): self is always visible; otherwise `GetViewUsersRestrictions`,
/// which is nil — everyone visible — whenever the viewer holds system-wide `view_members`, the
/// default `system_user` grant. The restricted remainder (membership-intersection queries, and
/// the check's own error path, which Go turns into a 403 naming `view_members`) is **forwarded
/// whole**, like `getTeamStats`'s restrictions branch: unreachable in this deployment without a
/// role edit, and Go re-runs the id check itself so ordering holds by construction.
///
/// The `me` literal never reaches this handler — axum matches the literal `/users/me` route
/// first, same as every other alias pair.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded))]
pub async fn get_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if !is_valid_id(&user_id) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }

    if session.0.user_id != user_id
        && !state
            .app
            .has_permission_to(&session.0.user_id, &PERMISSION_VIEW_MEMBERS)
            .await
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError(err).into_response(),
    };

    match respond_with_user(&state, &headers, &session, user).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Go's username-parameter charset: `{username:[A-Za-z0-9\_\-\.]+}` (api.go:204) — wider than
/// the id class by exactly `_`, `-` and `.`, the same three characters the `plugin_id` exception
/// in `lib.rs` names. A segment outside it never matches Go's route and falls to the mux 404,
/// so it must be forwarded rather than answered — [D-150]'s rule under a different alphabet.
fn segment_matches_username_mux(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Port of `getUserByUsername` (api4/user.go:359) —
/// `GET /api/v4/users/username/{username}`.
///
/// # The order is inverted relative to `getUser`, and the inversion carries a policy
///
/// `getUser` checks visibility **before** fetching; this handler fetches **first** and asks
/// `UserCanSeeOtherUser` about the row it found. The fetch-failure branch then re-checks the
/// caller's restrictions and answers the restricted caller a **403, not the 404** — so a caller
/// who may not enumerate users cannot probe which usernames exist. Both the restricted halves
/// (the failure branch's 403 and the visibility check's remainder) live behind the same
/// user-based `view_members` fast path as `getUser`, so this port forwards the whole request
/// for any caller without it — before the fetch, which also keeps the existence-hiding answer
/// Go's own.
///
/// # `RequireUsername` answers the *body*-param error
///
/// `c.SetInvalidParam("username")` (web/context.go:405) — `invalid_body_param`, not the
/// `invalid_url_param` every id-shaped segment gets. One of two handlers so far where a path
/// segment fails with the body id, reproduced not tidied.
///
/// # The store's case-folding is dead code through this route
///
/// `GetByUsername` lowers its **parameter** (`Username = lower(?)`), but `IsValidUsername`'s
/// class is `[a-z0-9\.\-_]+` — lowercase only — so an uppercase segment 400s here before the
/// lookup could fold it. The mux class (`A-Za-z…`) is wider than the validator, which is why
/// `SliceUser` reaches the handler at all and then fails validation rather than routing. The
/// `lower()` exists for Go's login paths, which share the store method; ported anyway and
/// pinned by a store-level DB test, [D-151]'s shape.
///
/// Everything after the visibility question is [`respond_with_user`], shared with both `getUser`
/// variants — terms of service, etag, the sanitize split, the encoder newline.
#[tracing::instrument(skip_all, fields(username = %username, forwarded))]
pub async fn get_user_by_username(
    State(state): State<AppState>,
    Path(username): Path<String>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if !segment_matches_username_mux(&username) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }

    if !mm_model::user::is_valid_username(&username) {
        return ApiError::invalid_param("username").into_response();
    }

    // The nil-restrictions fast path, checked before the fetch: a caller without user-based
    // `view_members` takes Go's restricted branches — including the failure branch's
    // existence-hiding 403 — through the forward.
    if !state
        .app
        .has_permission_to(&session.0.user_id, &PERMISSION_VIEW_MEMBERS)
        .await
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let user = match state.app.get_user_by_username(&username).await {
        Ok(user) => user,
        // Restrictions are nil for this caller, so Go surfaces the fetch error as-is
        // (api4/user.go:376) — the same 404 id as the 500, only the status differing.
        Err(err) => return ApiError(err).into_response(),
    };

    // `UserCanSeeOtherUser(session.UserId, user.Id)`: self is its first branch, and nil
    // restrictions — established by the fast path above — is its second. True by construction
    // here; the remainder was forwarded.
    match respond_with_user(&state, &headers, &session, user).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// The validated inputs of `getUsersByIds`, split from the handler so every 400 branch has a
/// unit test that needs no server: the id list after `SortedArrayFromJSON`, and `since`.
#[derive(Debug, PartialEq, Eq)]
struct UsersByIdsRequest {
    user_ids: Vec<String>,
    since: i64,
}

/// The two 400 branches of `getUsersByIds` (api4/user.go:1183-1203), in Go's order:
///
/// 1. **The body does not decode** → `api.payload.parse.error`, `where` `getUsersByIds`. The
///    decoder's habits are [`sorted_array_from_json`]'s and are pinned by its oracle.
/// 2. **No ids** (`[]` or `null`) → `invalid_body_param` naming `user_ids`.
/// 3. **`since` present and not an integer** → `invalid_body_param` naming `since` — the
///    *body*-param id for a query parameter, because `SetInvalidParamWithErr` is what Go calls.
///    `strconv.ParseInt(s, 10, 64)`: a leading sign is accepted, whitespace and out-of-range are
///    not, all of which `i64::from_str` matches. An **empty** `since=` is skipped, not an error.
///
/// Unlike `getUserStatusesByIds` there is **no length check** on the ids: `"zz"` is a legal id
/// here and is simply not found.
#[allow(clippy::result_large_err)]
fn parse_users_by_ids_request(
    body: &[u8],
    query: Option<&str>,
) -> Result<UsersByIdsRequest, ApiError> {
    let user_ids = sorted_array_from_json(body).map_err(|err| {
        tracing::debug!(error = %err, "user_ids body did not decode");
        ApiError(AppError::new(
            "getUsersByIds",
            PAYLOAD_PARSE_ERROR,
            None,
            String::new(),
            400,
        ))
    })?;
    if user_ids.is_empty() {
        return Err(ApiError::invalid_param("user_ids"));
    }

    let since = match crate::channels::query_first(query, "since") {
        Some(raw) if !raw.is_empty() => raw
            .parse::<i64>()
            .map_err(|_| ApiError::invalid_param("since"))?,
        _ => 0,
    };

    Ok(UsersByIdsRequest { user_ids, since })
}

/// Port of `getUsersByIds` (api4/user.go:1182) — `POST /api/v4/users/ids`, the lookup the
/// webapp makes for every author it is about to render.
///
/// # The restrictions forward comes first, before the body is read
///
/// Go's order is body → `since` → `GetViewUsersRestrictions` → query. Ours checks the
/// nil-restrictions fast path (user-based `view_members`, `getUser`'s rule) **first** and
/// forwards the whole request for a restricted caller — the body has to be intact to forward,
/// and Go re-runs both 400 branches itself, so the observable order is unchanged.
///
/// # Every user is sanitised as "other", including the caller
///
/// `sanitizeProfiles(users, IsAdmin)` is `SanitizeProfile` for each — the strict populated map,
/// admin forcing four flags — with no self exception: the caller's own row in the list loses
/// its email under the default privacy settings, where `GET /users/me` would keep it.
///
/// # What the store returns, and in which order
///
/// Deactivated users are included (no `DeleteAt` filter); unknown ids are silently absent;
/// `since` drops users not updated after it. Order is `Username ASC` from the query — but Go's
/// `userProfileByIdsCache` answers **hits first, in request order**, then the misses sorted, so
/// Go's wire order varies with what was recently asked. A client that depends on it is already
/// broken against Go; the parity suite compares as sets.
///
/// # Go's `update_at` is stale after a login; ours is the row's
///
/// `DoLogin` → `UpdateLastLogin` writes `Users.UpdateAt` (user_store.go:502) and nothing
/// invalidates that cache, so Go serves the pre-login value — here and via `GET /users/{id}` —
/// until eviction (measured: `…234499` from Go, `…304155` in the row, seconds after a login).
/// `since` is therefore answered against different timestamps by the two servers for a recently
/// logged-in user. Not reproducible without porting the cache; the parity fixture forces
/// coherence with an empty `PATCH`, which Go does invalidate on.
///
/// `json.Marshal` + `w.Write`: no trailing newline ([D-086]).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count, forwarded))]
pub async fn get_users_by_ids(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if !state
        .app
        .has_permission_to(&session.0.user_id, &PERMISSION_VIEW_MEMBERS)
        .await
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match serve_users_by_ids(&state, &session, request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_users_by_ids(
    state: &AppState,
    session: &AuthenticatedSession,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    let query = request.uri().query().map(str::to_owned);
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            ApiError(AppError::new(
                "getUsersByIds",
                PAYLOAD_PARSE_ERROR,
                None,
                String::new(),
                400,
            ))
        })?;
    let parsed = parse_users_by_ids_request(&bytes, query.as_deref())?;
    tracing::Span::current().record("count", parsed.user_ids.len());

    // `c.IsSystemAdmin()` — the session's roles (web/context.go:134).
    let is_admin = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;

    let mut users = state
        .app
        .get_users_by_ids(&parsed.user_ids, parsed.since)
        .await?;
    let options = sanitize_options(state.show_full_name, state.show_email_address, is_admin);
    for user in &mut users {
        user.sanitize_profile(&options, is_admin);
    }

    let body = serde_json::to_vec(&users).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise users");
        ApiError(AppError::new(
            "getUsersByIds",
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

/// The query parameters `getUsers` reads (api4/user.go:851-866), after the two conversions Go
/// applies on the way in: `url.Values.Get` for the strings and `strconv.ParseBool` — error
/// discarded — for the flags.
#[derive(Debug, Default, PartialEq, Eq)]
struct GetUsersQuery {
    in_team: String,
    not_in_team: String,
    in_channel: String,
    not_in_channel: String,
    in_group: String,
    not_in_group: String,
    group_constrained: bool,
    without_team: bool,
    inactive: bool,
    active: bool,
    role: String,
    roles: String,
    channel_roles: String,
    team_roles: String,
    sort: String,
    /// Read only inside the two `not_in_*` branches (api4/user.go:1021, 1062).
    abac_match_only: bool,
    /// `c.Params.Page` / `c.Params.PerPage` from the shared middleware — never a 400, whatever
    /// the caller sends. There is **no `since` here**: `UserGetOptions.UpdatedAfter` exists but
    /// `getUsers` never sets it, so the store's `UpdatedAfter` filter is unreachable from this
    /// route (it is `POST /users/ids` that has `since`).
    page: i64,
    per_page: i64,
}

fn parse_get_users_request(query: Option<&str>) -> GetUsersQuery {
    let get = |key: &str| crate::channels::query_first(query, key).unwrap_or_default();
    GetUsersQuery {
        in_team: get("in_team"),
        not_in_team: get("not_in_team"),
        in_channel: get("in_channel"),
        not_in_channel: get("not_in_channel"),
        in_group: get("in_group"),
        not_in_group: get("not_in_group"),
        group_constrained: crate::channels::query_flag_is_true(query, "group_constrained"),
        without_team: crate::channels::query_flag_is_true(query, "without_team"),
        inactive: crate::channels::query_flag_is_true(query, "inactive"),
        active: crate::channels::query_flag_is_true(query, "active"),
        role: get("role"),
        roles: get("roles"),
        channel_roles: get("channel_roles"),
        team_roles: get("team_roles"),
        sort: get("sort"),
        abac_match_only: crate::channels::query_flag_is_true(query, "abac_match_only"),
        page: crate::channels::parse_page(query),
        per_page: crate::channels::parse_per_page(query),
    }
}

/// Which arm of `getUsers`'s if/else-if chain (api4/user.go:1005-1130) this request selects.
///
/// The chain is **ordered**, and the order is not the order the parameters are declared in: a
/// request carrying both `in_team` and `in_channel` is an `in_team` request, and one carrying
/// `without_team=true` is that regardless of everything else. Resolving the branch first is what
/// lets the forwarding rules below be scoped to the parameters Go actually reads on the arm it
/// picked, rather than to the parameters merely present in the query string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Branch {
    WithoutTeam,
    NotInChannel,
    NotInTeam,
    InTeam,
    InChannel,
    InGroup,
    NotInGroup,
    All,
}

fn branch_of(query: &GetUsersQuery) -> Branch {
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
    } else if !query.in_group.is_empty() {
        Branch::InGroup
    } else if !query.not_in_group.is_empty() {
        Branch::NotInGroup
    } else {
        Branch::All
    }
}

/// Why this request goes to Go whole. `None` means this server answers it.
///
/// Each variant names a piece of Go the port does not have, and each is checked by **the same
/// condition Go uses to reach that piece** — not by the mere presence of a parameter. So
/// `?in_team=X&in_group=Y` is served (Go's chain picks `in_team` and never looks at the group),
/// while `?in_group=Y` alone is forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardReason {
    /// `role`, `roles`, `channel_roles` or `team_roles` — any one of them makes Go call
    /// `GetAllRoles()` to validate the names (api4/user.go:917), and there is no role store
    /// here. Go's guard is the four-way `||`, so this is deliberately *not* branch-scoped: a
    /// `team_roles` that the chosen branch would ignore still triggers the validation, and can
    /// still 400.
    RoleFilter,
    /// Any `sort`. `last_activity_at` needs the `Status` table, `create_at`, `status` and
    /// `admin` need three more store methods, and `display_name` needs groups — and every
    /// invalid value is one of five 400 branches. One rule covers the lot, and it is the same
    /// rule Go dispatches on (`sort == ""` is what reaches the plain store calls).
    Sort,
    /// `active=true&inactive=true`. Go calls `SetInvalidURLParam("inactive")` **without
    /// returning** (api4/user.go:900), so the request carries on, serves a 200 with the full
    /// list, and then `handleContextError` appends an error object to the body it has already
    /// written — the `getChannelsForUser` shape. Left to Go rather than reproduced.
    ActiveAndInactive,
    /// `without_team=true` → `GetUsersWithoutTeamPage`, plus its own `list_users_without_team`
    /// permission.
    WithoutTeam,
    /// `in_group` / `not_in_group` → the group store, which does not exist here.
    InGroup,
    NotInGroup,
    /// `group_constrained=true` on a `not_in_*` branch → `applyChannelGroupConstrainedFilter` /
    /// `applyTeamGroupConstrainedFilter`, three group tables deep. The other branches never read
    /// the flag, so they are served with it set.
    GroupConstrained,
    /// `abac_match_only=true` on a `not_in_*` branch → the Enterprise Advanced access-control
    /// service. See the handler's note for the half of ABAC this cannot detect.
    AbacMatchOnly,
}

fn forward_reason(query: &GetUsersQuery, branch: Branch) -> Option<ForwardReason> {
    if !query.role.is_empty()
        || !query.roles.is_empty()
        || !query.channel_roles.is_empty()
        || !query.team_roles.is_empty()
    {
        return Some(ForwardReason::RoleFilter);
    }
    if !query.sort.is_empty() {
        return Some(ForwardReason::Sort);
    }
    if query.inactive && query.active {
        return Some(ForwardReason::ActiveAndInactive);
    }
    match branch {
        Branch::WithoutTeam => Some(ForwardReason::WithoutTeam),
        Branch::InGroup => Some(ForwardReason::InGroup),
        Branch::NotInGroup => Some(ForwardReason::NotInGroup),
        Branch::NotInChannel | Branch::NotInTeam => {
            if query.group_constrained {
                Some(ForwardReason::GroupConstrained)
            } else if query.abac_match_only {
                Some(ForwardReason::AbacMatchOnly)
            } else {
                None
            }
        }
        Branch::InTeam | Branch::InChannel | Branch::All => None,
    }
}

/// Port of `getUsers` (api4/user.go:850) — `GET /api/v4/users`, the webapp's user list.
///
/// # What is served and what is forwarded
///
/// Five of the eight arms of Go's dispatch chain are served — the unfiltered list, `in_team`,
/// `in_channel`, `not_in_channel` and `not_in_team` — and the rest go to Go whole. Every
/// forwarding rule is [`ForwardReason`], and each is the *same* condition Go uses to reach the
/// code the port lacks, so a query that Go would answer from a served arm is never forwarded
/// merely because some parameter that arm ignores is present.
///
/// # The order of the three pre-flight steps
///
/// 1. **Forward first.** Go's own 400s (`sort`, the role names, `inactive`) all live behind the
///    forwarding rules, so a forwarded request gets Go's validation as well as Go's answer.
/// 2. **`not_in_channel` without `in_team` is `invalid_url_param` naming `team_id`** — the
///    *url*-param id for a query parameter, and it names a parameter the caller did not send.
///    This is the only 400 this handler can produce.
/// 3. **The restrictions fast path**, `getUser`'s rule: a caller without user-based
///    `view_members` has non-nil `ViewUsersRestrictions`, which every store query below would
///    have to join on, so the whole request is forwarded. Checked before the permission gates
///    because Go computes the restrictions before the dispatch.
///
/// # The etag is on two arms, and one of them reads the wrong parameter
///
/// `in_team` and `not_in_team` compute an etag and can answer 304; the other three never send
/// one. The `not_in_team` arm passes **`in_team`** to `GetUsersNotInTeamEtag` (api4/user.go:1049)
/// — see [`mm_app::App::get_users_not_in_team_etag`], which reproduces it.
///
/// # ABAC is a licence-gated divergence
///
/// `ChannelAccessControlled` / `TeamAccessControlled` return false without an Enterprise
/// Advanced licence, which is this deployment, so the served arms match. On a licensed server
/// with attribute-based access control on, a policy-enforced **private** channel would narrow
/// Go's `not_in_channel` list without any query parameter to detect it by — the port cannot see
/// the licence, so that variant would diverge. Recorded as [D-154]; the explicit
/// `abac_match_only=true` half is forwarded.
///
/// # Not ported
///
/// `UpdateLastActivityAtIfNeeded` — a write on the read path, deferred with the session cache
/// (D-084). `json.Marshal` + `w.Write`: **no trailing newline** ([D-086]).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, branch, forwarded, count))]
pub async fn get_users(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let parsed = parse_get_users_request(query.as_deref());
    let branch = branch_of(&parsed);
    tracing::Span::current().record("branch", tracing::field::debug(branch));

    if let Some(reason) = forward_reason(&parsed, branch) {
        tracing::Span::current().record("forwarded", tracing::field::debug(reason));
        return crate::proxy::forward_to_go(State(state), request).await;
    }

    if !parsed.not_in_channel.is_empty() && parsed.in_team.is_empty() {
        return ApiError::invalid_url_param("team_id").into_response();
    }

    if !state
        .app
        .has_permission_to(&session.0.user_id, &PERMISSION_VIEW_MEMBERS)
        .await
    {
        tracing::Span::current().record("forwarded", "view_restrictions");
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match serve_users(&state, &headers, &session, &parsed, branch).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Go's `HandleEtag` (web/context.go:230): a plain string compare against `If-None-Match`, no
/// weak comparison and no candidate list, and the 304 carries the etag back.
fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == etag)
}

/// The dispatch itself: one arm per served branch, each with its own permission gate, in Go's
/// order. Returns the etag alongside the users so the caller can set the header only when Go
/// would — `if etag != ""` (api4/user.go:1136).
async fn serve_users(
    state: &AppState,
    headers: &HeaderMap,
    session: &AuthenticatedSession,
    query: &GetUsersQuery,
    branch: Branch,
) -> Result<Response, ApiError> {
    let page = UserPage {
        page: query.page,
        per_page: query.per_page,
        inactive: query.inactive,
        active: query.active,
    };

    let (users, etag) = match branch {
        Branch::NotInChannel => {
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(
                    &session.0,
                    &query.not_in_channel,
                    &PERMISSION_READ_CHANNEL,
                )
                .await;
            if !allowed {
                return Err(ApiError(*make_permission_error(
                    &session.0,
                    &[&PERMISSION_READ_CHANNEL],
                )));
            }
            let users = state
                .app
                .get_users_not_in_channel_page(&query.in_team, &query.not_in_channel, page)
                .await?;
            (users, None)
        }
        Branch::NotInTeam => {
            if !state
                .app
                .session_has_permission_to_team(
                    &session.0,
                    &query.not_in_team,
                    &PERMISSION_VIEW_TEAM,
                )
                .await
            {
                return Err(ApiError(*make_permission_error(
                    &session.0,
                    &[&PERMISSION_VIEW_TEAM],
                )));
            }
            // `in_team`, not `not_in_team` — Go's argument, reproduced deliberately.
            let etag = state
                .app
                .get_users_not_in_team_etag(
                    &query.in_team,
                    state.show_full_name,
                    state.show_email_address,
                )
                .await;
            if etag_matches(headers, &etag) {
                return Ok(not_modified(etag));
            }
            let users = state
                .app
                .get_users_not_in_team_page(&query.not_in_team, page)
                .await?;
            (users, Some(etag))
        }
        Branch::InTeam => {
            if !state
                .app
                .session_has_permission_to_team(&session.0, &query.in_team, &PERMISSION_VIEW_TEAM)
                .await
            {
                return Err(ApiError(*make_permission_error(
                    &session.0,
                    &[&PERMISSION_VIEW_TEAM],
                )));
            }
            let etag = state
                .app
                .get_users_in_team_etag(
                    &query.in_team,
                    state.show_full_name,
                    state.show_email_address,
                )
                .await;
            if etag_matches(headers, &etag) {
                return Ok(not_modified(etag));
            }
            let users = state
                .app
                .get_users_in_team_page(&query.in_team, page)
                .await?;
            (users, Some(etag))
        }
        Branch::InChannel => {
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(
                    &session.0,
                    &query.in_channel,
                    &PERMISSION_READ_CHANNEL,
                )
                .await;
            if !allowed {
                return Err(ApiError(*make_permission_error(
                    &session.0,
                    &[&PERMISSION_READ_CHANNEL],
                )));
            }
            let users = state
                .app
                .get_users_in_channel_page(&query.in_channel, page)
                .await?;
            (users, None)
        }
        // `RestrictUsersGetByPermissions` only fills in the restrictions, and a caller whose
        // restrictions are non-nil was forwarded before this function ran.
        Branch::All => (state.app.get_users_page(page).await?, None),
        // Forwarded by `forward_reason`; forwarding again rather than panicking keeps the
        // impossible case a working request instead of a 500.
        Branch::WithoutTeam | Branch::InGroup | Branch::NotInGroup => {
            return Err(ApiError(AppError::new(
                "getUsers",
                "api.context.404.app_error",
                None,
                String::new(),
                404,
            )));
        }
    };
    tracing::Span::current().record("count", users.len());

    // `sanitizeProfiles(profiles, c.IsSystemAdmin())` — the same strict per-user map as
    // `getUsersByIds`, with no self exception. Go also runs `u.Sanitize(map[string]bool{})` in
    // the store first; that clears a strict subset of what this clears, so it is not ported.
    let is_admin = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;
    let mut users = users;
    let options = sanitize_options(state.show_full_name, state.show_email_address, is_admin);
    for user in &mut users {
        user.sanitize_profile(&options, is_admin);
    }

    let body = serde_json::to_vec(&users).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the user list");
        ApiError(AppError::new(
            "getUsers",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    let mut response = (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response();
    if let Some(etag) = etag
        && let Ok(value) = axum::http::HeaderValue::from_str(&etag)
    {
        response.headers_mut().insert(ETAG, value);
    }
    Ok(response)
}

/// Go's 304: the etag header and nothing else. `x-mmrs-served-by` is ours, on every response we
/// answer — the parity suite reads it to prove the request was not forwarded.
fn not_modified(etag: String) -> Response {
    (
        StatusCode::NOT_MODIFIED,
        [
            (HEADER_ETAG_SERVER, etag.as_str()),
            ("x-mmrs-served-by", "rust"),
        ],
    )
        .into_response()
}

/// The four query parameters `autocompleteUsers` reads (api4/user.go:1386-1389).
///
/// There is no `page`/`per_page` here and no `c.Params` at all — the handler goes straight to
/// `r.URL.Query()`, so nothing in this route can produce a 400 for a malformed parameter.
#[derive(Debug, Default, PartialEq, Eq)]
struct AutocompleteQuery {
    in_channel: String,
    in_team: String,
    name: String,
    /// Already defaulted and clamped — see [`autocomplete_limit`].
    limit: i64,
}

fn parse_autocomplete_request(query: Option<&str>) -> AutocompleteQuery {
    let get = |key: &str| crate::channels::query_first(query, key).unwrap_or_default();
    AutocompleteQuery {
        in_channel: get("in_channel"),
        in_team: get("in_team"),
        name: get("name"),
        limit: autocomplete_limit(crate::channels::query_first(query, "limit")),
    }
}

/// `strconv.Atoi` with the error **discarded**, which is what the handler does
/// (`limit, _ := strconv.Atoi(limitStr)`).
///
/// Go returns a value alongside its error and the two failure modes give *different* values:
/// a syntax error yields `0`, and a range error yields the saturated bound. So
/// `?limit=99999999999999999999` is `MaxInt64`, which the clamp below turns into 1000, while
/// `?limit=12abc` is `0` and returns nothing at all. Both measured against the running Go
/// server — a Rust `parse::<i64>()` that treats every error alike gets the first one wrong.
fn go_atoi(raw: &str) -> i64 {
    match raw.parse::<i64>() {
        Ok(value) => value,
        Err(err) => match err.kind() {
            std::num::IntErrorKind::PosOverflow => i64::MAX,
            std::num::IntErrorKind::NegOverflow => i64::MIN,
            _ => 0,
        },
    }
}

/// Port of the limit block at api4/user.go:1390-1395.
///
/// Read the branches carefully, because two of the three plausible readings are wrong:
///
/// - The default applies **only to an absent or empty `limit`**, not to an unparseable one. A
///   garbage limit is `0`, and `LIMIT 0` is an empty list with a 200 — not 100 results.
/// - The clamp is one-sided. There is no floor, so a **negative** limit reaches Postgres and
///   fails the query: a 500 carrying `app.user.search.app_error`, on both servers.
fn autocomplete_limit(raw: Option<String>) -> i64 {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return mm_store::user_store::USER_SEARCH_DEFAULT_LIMIT;
    };
    let limit = go_atoi(&raw);
    if limit > mm_store::user_store::USER_SEARCH_MAX_LIMIT {
        mm_store::user_store::USER_SEARCH_MAX_LIMIT
    } else {
        limit
    }
}

/// Port of the `AllowFullNames` block at api4/user.go:1402-1406.
///
/// `manage_system` forces it **on** outright; everyone else gets
/// `PrivacySettings.ShowFullName`. Written as the if/else Go writes rather than the `||` it
/// collapses to, so that which input was consulted stays visible — and so a mutation to either
/// arm is a failing test rather than a value that happens to agree.
fn allow_full_names(is_admin: bool, show_full_name: bool) -> bool {
    if is_admin { true } else { show_full_name }
}

/// Port of `autocompleteUsers` (api4/user.go:1385) — `GET /api/v4/users/autocomplete`, the
/// route behind every `@`-mention keystroke in the webapp.
///
/// # Three arms, three *shapes*
///
/// The response is always a `model.UserAutocomplete`, but which of its fields are populated
/// depends on the parameters, and `out_of_channel` and `agents` carry `omitempty` while `users`
/// does not. So:
///
/// | parameters | `users` | `out_of_channel` | `agents` |
/// |---|---|---|---|
/// | `in_channel` + `in_team` | in-channel matches, `[]` when none | out-of-channel matches, **omitted** when none | omitted |
/// | `in_team` only | in-team matches, `[]` when none | **omitted always** — the field is never assigned | omitted |
/// | neither | system-wide matches, `[]` when none | omitted always | omitted |
///
/// `users` is `[]` and never `null` because the store returns `[]*model.User{}` rather than nil,
/// which is why the app layer hands back `Some(vec)`.
///
/// # Never autocomplete on emails
///
/// `AllowEmails: false` is hard-wired (api4/user.go:1399) and it moves a *search* column, not a
/// response field: `performSearch` picks `UserSearchTypeNames` over `UserSearchTypeAll`, so
/// `Email` is never a `LIKE` target. A user whose address contains the term and whose name does
/// not is invisible to this route — while the `email` field itself is still on the wire for
/// anyone the search *did* match, subject to the privacy settings. The two are unrelated and
/// conflating them is the easy mistake.
///
/// # `AllowFullNames` has two sources and only one of them is the setting
///
/// `manage_system` sets it to `true` outright; everyone else gets
/// `PrivacySettings.ShowFullName`. So an admin searches first and last names even on a server
/// configured to hide them.
///
/// # The gates run before the missing-team check
///
/// `?in_channel=X` with no `in_team` is a 500 — but only if the caller could have read `X`.
/// The `read_channel` gate is evaluated first, so an unauthorised caller gets a 403 and never
/// learns that the parameter combination was invalid.
///
/// # `Agents` is unreachable here
///
/// `GetUsersForAgents` goes through `agentsBridge` to the `mattermost-plugin-ai` bridge API.
/// Without that plugin the call errors, the handler's `appErr == nil` guard fails, and `Agents`
/// is left nil — and `omitempty` then drops it. Measured on the running server: the key is
/// absent from every response. `Vec::new()` with `skip_serializing_if` reproduces that; an
/// `Option` or a bare `Vec` without the skip would emit `"agents":null` or `"agents":[]`, and
/// neither has ever appeared on this deployment's wire.
///
/// # Not ported
///
/// `RestrictUsersSearchByPermissions` only fills in `ViewRestrictions`, so a caller who lacks
/// user-based `view_members` is forwarded whole — `getUsers`' rule, for the same reason: every
/// query below would need the restriction joins.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, arm, forwarded))]
pub async fn autocomplete_users(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let parsed = parse_autocomplete_request(query.as_deref());

    // `c.IsSystemAdmin()` and the `AllowFullNames` gate are the *same* permission check asked
    // twice (api4/user.go:1397, 1403). Asking once and reusing it is the only shortcut taken.
    let is_admin = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;
    let options = mm_store::user_store::UserSearchOptions {
        allow_full_names: allow_full_names(is_admin, state.show_full_name),
        limit: parsed.limit,
    };

    if !parsed.in_channel.is_empty() {
        let (allowed, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &parsed.in_channel,
                &PERMISSION_READ_CHANNEL,
            )
            .await;
        if !allowed {
            return ApiError(*make_permission_error(
                &session.0,
                &[&PERMISSION_READ_CHANNEL],
            ))
            .into_response();
        }
    }

    if !parsed.in_team.is_empty()
        && !state
            .app
            .session_has_permission_to_team(&session.0, &parsed.in_team, &PERMISSION_VIEW_TEAM)
            .await
    {
        return ApiError(*make_permission_error(&session.0, &[&PERMISSION_VIEW_TEAM]))
            .into_response();
    }

    // `RestrictUsersSearchByPermissions` stands here in Go — after both gates, before the
    // dispatch. A caller with non-nil restrictions goes to Go whole, including for the
    // missing-team-id 500 below, which Go raises in exactly the same place.
    if !state
        .app
        .has_permission_to(&session.0.user_id, &PERMISSION_VIEW_MEMBERS)
        .await
    {
        tracing::Span::current().record("forwarded", "view_restrictions");
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match serve_autocomplete(&state, &parsed, &options, is_admin).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// The dispatch and the response body, split out so the handler above is just the gates.
async fn serve_autocomplete(
    state: &AppState,
    query: &AutocompleteQuery,
    options: &mm_store::user_store::UserSearchOptions,
    is_admin: bool,
) -> Result<Response, ApiError> {
    let mut autocomplete = mm_model::user_autocomplete::UserAutocomplete::default();

    if !query.in_channel.is_empty() {
        tracing::Span::current().record("arm", "in_channel");
        // The channel arm needs the team as well, and refuses rather than guessing: the team is
        // what makes the *out of channel* half answerable, and Go calls that a server error
        // (500) rather than a bad request. Reproduced, status and id both.
        if query.in_team.is_empty() {
            return Err(ApiError(AppError::new(
                "autocompleteUser",
                "api.user.autocomplete_users.missing_team_id.app_error",
                None,
                format!("channelId={}", query.in_channel),
                500,
            )));
        }
        let result = state
            .app
            .autocomplete_users_in_channel(&query.in_team, &query.in_channel, &query.name, options)
            .await?;
        autocomplete.users = result.in_channel;
        // `autocomplete.OutOfChannel = result.OutOfChannel` — into the `omitempty` field, so an
        // empty out-of-channel list is a *missing key*, not `[]`.
        autocomplete.out_of_channel = result.out_of_channel.unwrap_or_default();
    } else if !query.in_team.is_empty() {
        tracing::Span::current().record("arm", "in_team");
        let result = state
            .app
            .autocomplete_users_in_team(&query.in_team, &query.name, options)
            .await?;
        autocomplete.users = result.in_team;
    } else {
        tracing::Span::current().record("arm", "system");
        // `SearchUsersInTeam(c.AppContext, "", name, options)` — the empty team id is Go's, and
        // it is what makes this the system-wide arm.
        let users = state
            .app
            .search_users_in_team("", &query.name, options)
            .await?;
        autocomplete.users = Some(users);
    }

    // `a.SanitizeProfile(user, options.IsAdmin)` over both lists, in the app layer on Go's side
    // and here on ours (D-085). **There is no self exception**: unlike `getUser`, whose caller
    // gets the lax empty-map `Sanitize` on its own row, everyone here goes through the strict
    // populated map. So a non-admin searching for itself sees its own `notify_props` emptied
    // away (and the key dropped by `omitempty`) and its own `auth_data` blanked to `""` — which
    // `omitempty` keeps, because a pointer to the empty string is not nil. Measured, not
    // reasoned: `sanitisation_matches_go_for_both_an_admin_and_a_plain_caller`.
    let sanitize = sanitize_options(state.show_full_name, state.show_email_address, is_admin);
    for user in autocomplete.users.iter_mut().flatten() {
        user.sanitize_profile(&sanitize, is_admin);
    }
    for user in &mut autocomplete.out_of_channel {
        user.sanitize_profile(&sanitize, is_admin);
    }

    // `json.NewEncoder(w).Encode(autocomplete)` — **with** the trailing newline `Encode`
    // appends. The sibling `getUsers` uses `json.Marshal` and has none ([D-086]); the two
    // handlers are eleven lines apart in the same Go file and differ by this one byte.
    let mut body = serde_json::to_vec(&autocomplete).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise UserAutocomplete");
        ApiError(AppError::new(
            "autocompleteUser",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

#[cfg(test)]
mod tests {
    use mm_model::user::User;
    use std::collections::HashMap;

    use super::{
        Branch, ForwardReason, UsersByIdsRequest, branch_of, forward_reason,
        parse_get_users_request, parse_users_by_ids_request, sanitize_options,
        segment_matches_username_mux,
    };

    /// `branch_of` on the query string, so the tests read like requests.
    fn branch(query: &str) -> Branch {
        branch_of(&parse_get_users_request(Some(query)))
    }

    /// `forward_reason` on the query string.
    fn forward(query: &str) -> Option<ForwardReason> {
        let parsed = parse_get_users_request(Some(query));
        forward_reason(&parsed, branch_of(&parsed))
    }

    /// The chain is ordered and the order is **not** the order the parameters are declared in.
    /// Every pair below has a plausible wrong answer: a reader who checks `in_team` first would
    /// get four of these wrong, and one who checks the parameters in declaration order would get
    /// `without_team` wrong.
    #[test]
    fn the_dispatch_chain_is_gos_order_not_the_declaration_order() {
        assert_eq!(branch(""), Branch::All);
        assert_eq!(branch("page=2&per_page=10"), Branch::All);
        assert_eq!(branch("in_team=t"), Branch::InTeam);
        assert_eq!(branch("in_channel=c"), Branch::InChannel);
        assert_eq!(branch("not_in_team=t"), Branch::NotInTeam);
        assert_eq!(branch("not_in_channel=c&in_team=t"), Branch::NotInChannel);
        assert_eq!(branch("in_group=g"), Branch::InGroup);
        assert_eq!(branch("not_in_group=g"), Branch::NotInGroup);

        // The precedence pairs.
        assert_eq!(
            branch("in_team=t&in_channel=c"),
            Branch::InTeam,
            "in_team is checked before in_channel"
        );
        assert_eq!(
            branch("in_team=t&not_in_team=o"),
            Branch::NotInTeam,
            "not_in_team is checked before in_team"
        );
        assert_eq!(
            branch("in_team=t&not_in_channel=c&not_in_team=o"),
            Branch::NotInChannel,
            "not_in_channel outranks both team filters"
        );
        assert_eq!(
            branch("in_team=t&in_group=g"),
            Branch::InTeam,
            "in_group is last but one — an in_team request never reaches it"
        );
        assert_eq!(
            branch("without_team=true&in_team=t&not_in_channel=c"),
            Branch::WithoutTeam,
            "without_team short-circuits the whole chain"
        );
        // The flag is ParseBool, so only truthy values take the branch.
        assert_eq!(branch("without_team=false&in_team=t"), Branch::InTeam);
        assert_eq!(branch("without_team&in_team=t"), Branch::InTeam);
        assert_eq!(branch("without_team=1"), Branch::WithoutTeam);

        // An empty value is an absent filter, not a filter on the empty string.
        assert_eq!(branch("in_team=&in_channel=c"), Branch::InChannel);
    }

    /// The forwarding boundary, from both sides. The role rule is global because Go's validation
    /// guard is; the group and ABAC rules are branch-scoped because Go only reads those
    /// parameters on the arm that uses them.
    #[test]
    fn the_forwarding_rules_are_the_conditions_go_dispatches_on() {
        // Served.
        for served in [
            "",
            "page=1&per_page=200",
            "in_team=t",
            "in_channel=c",
            "not_in_team=t",
            "not_in_channel=c&in_team=t",
            "in_team=t&active=true",
            "in_channel=c&inactive=true",
            "sort=",
            "role=&roles=&channel_roles=&team_roles=",
            "without_team=false",
            "group_constrained=false&not_in_team=t",
        ] {
            assert_eq!(forward(served), None, "{served:?} must be served");
        }

        // The four role parameters, each on its own, and each still forwarding on a branch that
        // would ignore it — Go validates them all before it dispatches.
        assert_eq!(
            forward("role=system_admin"),
            Some(ForwardReason::RoleFilter)
        );
        assert_eq!(
            forward("roles=system_user"),
            Some(ForwardReason::RoleFilter)
        );
        assert_eq!(
            forward("in_team=t&channel_roles=channel_admin"),
            Some(ForwardReason::RoleFilter),
            "channel_roles is ignored by the in_team arm but still triggers GetAllRoles"
        );
        assert_eq!(
            forward("in_channel=c&team_roles=team_admin"),
            Some(ForwardReason::RoleFilter)
        );

        // Every sort, valid or not — including the ones Go 400s on.
        for sort in [
            "last_activity_at",
            "create_at",
            "status",
            "admin",
            "display_name",
            "bogus",
        ] {
            assert_eq!(
                forward(&format!("in_team=t&sort={sort}")),
                Some(ForwardReason::Sort),
                "sort={sort}"
            );
        }

        assert_eq!(
            forward("active=true&inactive=true"),
            Some(ForwardReason::ActiveAndInactive)
        );
        assert_eq!(
            forward("active=true&inactive=false"),
            None,
            "only both-at-once reaches Go's return-less SetInvalidURLParam"
        );

        assert_eq!(
            forward("without_team=true"),
            Some(ForwardReason::WithoutTeam)
        );
        assert_eq!(forward("in_group=g"), Some(ForwardReason::InGroup));
        assert_eq!(forward("not_in_group=g"), Some(ForwardReason::NotInGroup));
        assert_eq!(
            forward("in_team=t&in_group=g"),
            None,
            "the in_team arm never reads in_group, so this is served"
        );

        // group_constrained and abac_match_only, on and off the arms that read them.
        for scoped in ["not_in_team=t", "not_in_channel=c&in_team=t"] {
            assert_eq!(
                forward(&format!("{scoped}&group_constrained=true")),
                Some(ForwardReason::GroupConstrained),
                "{scoped}"
            );
            assert_eq!(
                forward(&format!("{scoped}&abac_match_only=true")),
                Some(ForwardReason::AbacMatchOnly),
                "{scoped}"
            );
        }
        for ignored in ["", "in_team=t", "in_channel=c"] {
            assert_eq!(
                forward(&format!(
                    "{ignored}&group_constrained=true&abac_match_only=true"
                )),
                None,
                "{ignored:?} never reads either flag"
            );
        }
    }

    /// Paging comes from the shared middleware and never fails; `since` — which the sibling
    /// `POST /users/ids` has — does not exist on this route at all.
    #[test]
    fn paging_defaults_and_caps_apply_and_there_is_no_since() {
        let parsed = parse_get_users_request(Some("page=3&per_page=999&since=1"));
        assert_eq!(parsed.page, 3);
        assert_eq!(parsed.per_page, 200, "capped at PerPageMaximum");
        let parsed = parse_get_users_request(Some("page=-1&per_page=abc"));
        assert_eq!((parsed.page, parsed.per_page), (0, 60));
        let parsed = parse_get_users_request(None);
        assert_eq!((parsed.page, parsed.per_page), (0, 60));
        assert_eq!(parsed.per_page, 60);
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Sorted, de-duplicated, and — unlike the status route — **no** length check: a two-byte
    /// id is accepted and left to the query to not find.
    #[test]
    fn users_by_ids_body_is_sorted_deduplicated_and_not_length_checked() {
        let parsed =
            parse_users_by_ids_request(format!(r#"["{B}","zz","{A}","{B}"]"#).as_bytes(), None)
                .expect("valid");
        assert_eq!(
            parsed,
            UsersByIdsRequest {
                user_ids: vec![A.to_owned(), B.to_owned(), "zz".to_owned()],
                since: 0,
            }
        );
    }

    /// Branch 1: the parse error wears this handler's `where`, not the status route's.
    #[test]
    fn an_undecodable_body_is_the_payload_parse_error_from_get_users_by_ids() {
        for body in ["", "{", "[1]", "[\"a\" \"b\"]"] {
            let err = parse_users_by_ids_request(body.as_bytes(), None)
                .expect_err("rejected")
                .0;
            assert_eq!(err.id, "api.payload.parse.error", "body {body:?}");
            assert_eq!(err.where_, "getUsersByIds", "body {body:?}");
            assert_eq!(err.status_code, 400);
        }
    }

    /// Branch 2: `[]` and `null` are the body-param error naming `user_ids`, checked **before**
    /// `since` — a bad `since` on an empty body reports `user_ids`.
    #[test]
    fn an_empty_list_names_user_ids_even_with_a_bad_since() {
        for body in ["[]", "null"] {
            let err = parse_users_by_ids_request(body.as_bytes(), Some("since=abc"))
                .expect_err("rejected")
                .0;
            assert_eq!(err.id, "api.context.invalid_body_param.app_error");
            assert_eq!(
                err.params.as_ref().and_then(|p| p.get("Name")),
                Some(&serde_json::Value::String("user_ids".to_owned())),
                "body {body:?}"
            );
        }
    }

    /// Branch 3: `since` is `ParseInt` — sign accepted, anything else a body-param 400 naming
    /// `since`; empty is skipped; the first of a repeated key wins (`url.Values.Get`).
    #[test]
    fn since_parses_like_parse_int() {
        let body = format!(r#"["{A}"]"#);
        let since_of = |query: &str| {
            parse_users_by_ids_request(body.as_bytes(), Some(query))
                .map(|r| r.since)
                .map_err(|e| {
                    e.0.params
                        .and_then(|p| p.get("Name").cloned())
                        .and_then(|n| n.as_str().map(str::to_owned))
                })
        };
        assert_eq!(since_of("since=1786973424207"), Ok(1_786_973_424_207));
        assert_eq!(
            since_of("since=-5"),
            Ok(-5),
            "a negative is legal; the store ignores it"
        );
        assert_eq!(
            since_of("since=%2B7"),
            Ok(7),
            "a leading plus is ParseInt-legal"
        );
        assert_eq!(since_of("since="), Ok(0), "empty is skipped, not parsed");
        assert_eq!(since_of("other=1"), Ok(0));
        assert_eq!(since_of("since=3&since=abc"), Ok(3), "Get takes the first");
        for bad in [
            "since=abc",
            "since=1.5",
            "since=%201",
            "since=1_000",
            "since=99999999999999999999",
        ] {
            assert_eq!(since_of(bad), Err(Some("since".to_owned())), "{bad}");
        }
    }

    /// The username class is the id class plus exactly `_`, `-` and `.` — each admitted, and
    /// the near-misses (space, `@`, `%`, empty, non-ASCII) all fall to the mux 404 forward.
    #[test]
    fn the_username_charset_is_gos_mux_class() {
        for ok in ["sliceuser", "a_b-c.d", "UPPER", "0", "..."] {
            assert!(segment_matches_username_mux(ok), "{ok:?} matches Go's mux");
        }
        for bad in ["", "a b", "a@b", "a%40b", "héllo", "a/b"] {
            assert!(
                !segment_matches_username_mux(bad),
                "{bad:?} never matches Go's route, so it must be forwarded"
            );
        }
    }

    /// The config half tracks the two privacy flags; the admin override force-enables four,
    /// including the two (`authservice`, `authdata`) that have **no** config source — so a
    /// non-admin's map never contains them and `Sanitize`'s strict mode strips both.
    #[test]
    fn sanitize_options_merges_config_and_the_admin_override() {
        let plain = sanitize_options(true, false, false);
        assert_eq!(plain.get("fullname"), Some(&true));
        assert_eq!(plain.get("email"), Some(&false));
        assert_eq!(plain.get("authservice"), None, "no config source, no key");
        assert_eq!(plain.get("authdata"), None);
        assert_eq!(plain.len(), 2);

        let admin = sanitize_options(false, false, true);
        assert_eq!(
            admin.get("email"),
            Some(&true),
            "the admin override beats the config flag"
        );
        assert_eq!(admin.get("fullname"), Some(&true));
        assert_eq!(admin.get("authservice"), Some(&true));
        assert_eq!(admin.get("authdata"), Some(&true));
        assert_eq!(admin.len(), 4);
    }

    /// The non-self path is `SanitizeProfile`, whose populated map is the **strict** mode — the
    /// exact opposite of the self path's empty map. The pair is the route's central trap.
    #[test]
    fn the_profile_sanitizer_is_strict_where_the_self_sanitizer_is_lax() {
        let build = || User {
            email: "someone@example.com".to_owned(),
            first_name: "Some".to_owned(),
            auth_service: "gitlab".to_owned(),
            notify_props: Some({
                let mut props = mm_model::utils::StringMap::new();
                props.insert("desktop".to_owned(), "all".to_owned());
                props
            }),
            ..Default::default()
        };

        let mut for_self = build();
        for_self.sanitize(&HashMap::new());
        assert_eq!(for_self.email, "someone@example.com", "self keeps email");
        assert_eq!(for_self.auth_service, "gitlab");

        let mut for_other = build();
        for_other.sanitize_profile(&sanitize_options(true, true, false), false);
        assert_eq!(for_other.email, "someone@example.com", "email flag is true");
        assert_eq!(for_other.first_name, "Some", "fullname flag is true");
        assert!(
            for_other.auth_service.is_empty(),
            "authservice has no flag for a non-admin, so the strict mode strips it"
        );
        assert_eq!(
            for_other.notify_props,
            Some(mm_model::utils::StringMap::new()),
            "ClearNonProfileFields blanks notify props for a non-admin viewer — an emptied \
             map, which omitempty then keeps off the wire"
        );
    }

    /// The four fields `Sanitize` clears unconditionally. These are the credential-bearing ones,
    /// and they go regardless of any flag.
    #[test]
    fn sanitize_for_self_strips_the_unconditional_secrets() {
        let mut user = User {
            password: "hunter2".to_owned(),
            mfa_secret: "mfa".to_owned(),
            mfa_used_timestamps: Some(vec!["1".to_owned()]),
            last_login: 1_786_973_424_207,
            ..Default::default()
        };

        user.sanitize(&HashMap::new());

        assert!(
            user.password.is_empty(),
            "password must never reach the wire"
        );
        assert!(user.mfa_secret.is_empty());
        assert_eq!(user.mfa_used_timestamps, None);
        assert_eq!(user.last_login, 0);
    }

    /// The inverted-looking half, and the one that would break `/users/me` if read the obvious
    /// way. Go guards the flag block behind `len(options) != 0`, so an EMPTY map keeps email,
    /// full name and auth service — the opposite of "no permissions granted". Asserted here
    /// because the handler passes exactly this empty map on every request.
    #[test]
    fn an_empty_options_map_keeps_the_flag_guarded_fields() {
        let mut user = User {
            email: "slice@example.com".to_owned(),
            first_name: "Slice".to_owned(),
            last_name: "Tester".to_owned(),
            auth_service: "gitlab".to_owned(),
            auth_data: Some("some-auth-data".to_owned()),
            last_password_update: 1_786_973_418_231,
            ..Default::default()
        };

        user.sanitize(&HashMap::new());

        assert_eq!(user.email, "slice@example.com");
        assert_eq!(user.first_name, "Slice");
        assert_eq!(user.last_name, "Tester");
        assert_eq!(user.auth_service, "gitlab");
        assert_eq!(user.auth_data.as_deref(), Some("some-auth-data"));
        assert_eq!(user.last_password_update, 1_786_973_418_231);
    }

    /// A populated map is the strict one: a flag that is absent or false strips its field.
    #[test]
    fn a_populated_options_map_strips_what_it_does_not_allow() {
        let mut user = User {
            email: "slice@example.com".to_owned(),
            first_name: "Slice".to_owned(),
            ..Default::default()
        };

        let mut options = HashMap::new();
        options.insert("fullname".to_owned(), true);
        user.sanitize(&options);

        assert!(
            user.email.is_empty(),
            "email not flagged, so it is stripped"
        );
        assert_eq!(
            user.first_name, "Slice",
            "fullname flagged true, so it stays"
        );
    }

    /// The etag's inputs are the two privacy flags, so the same user rendered under different
    /// settings must not collide in a cache.
    #[test]
    fn the_etag_changes_with_the_privacy_flags() {
        let user = User {
            id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            update_at: 1_786_973_424_207,
            ..Default::default()
        };

        let both = user.etag(true, true);
        assert_ne!(both, user.etag(false, true));
        assert_ne!(both, user.etag(true, false));
        assert_eq!(both, user.etag(true, true), "etag is deterministic");
    }
}

/// The parts of `autocompleteUsers` that need no database: the query parsing, the limit block,
/// and the `AllowFullNames` derivation.
#[cfg(test)]
mod autocomplete_tests {
    use super::{
        AutocompleteQuery, allow_full_names, autocomplete_limit, go_atoi,
        parse_autocomplete_request, sanitize_options,
    };
    use mm_store::user_store::{USER_SEARCH_DEFAULT_LIMIT, USER_SEARCH_MAX_LIMIT};

    fn limit(query: &str) -> i64 {
        parse_autocomplete_request(Some(query)).limit
    }

    #[test]
    fn the_four_parameters_are_read_verbatim() {
        let parsed =
            parse_autocomplete_request(Some("in_channel=chan1&in_team=team1&name=%40bob&limit=7"));
        assert_eq!(
            parsed,
            AutocompleteQuery {
                in_channel: "chan1".to_owned(),
                in_team: "team1".to_owned(),
                // The `@` a mention box sends survives url-decoding intact; the store trims it.
                name: "@bob".to_owned(),
                limit: 7,
            }
        );
    }

    #[test]
    fn an_empty_query_string_is_all_defaults() {
        assert_eq!(
            parse_autocomplete_request(None),
            AutocompleteQuery {
                limit: USER_SEARCH_DEFAULT_LIMIT,
                ..AutocompleteQuery::default()
            }
        );
    }

    /// The default fires for an **absent or empty** parameter and for nothing else.
    #[test]
    fn only_a_missing_limit_gets_the_default() {
        assert_eq!(limit("name=a"), USER_SEARCH_DEFAULT_LIMIT);
        assert_eq!(limit("name=a&limit="), USER_SEARCH_DEFAULT_LIMIT);
        assert_eq!(limit("name=a&limit=7"), 7);
    }

    /// One-sided: clamped above, untouched below. A floor added here would turn the 500 that
    /// `?limit=-1` produces on both servers into a 200 on one of them.
    #[test]
    fn the_clamp_has_a_ceiling_and_no_floor() {
        assert_eq!(limit("limit=5000"), USER_SEARCH_MAX_LIMIT);
        assert_eq!(limit("limit=1000"), USER_SEARCH_MAX_LIMIT);
        assert_eq!(limit("limit=999"), 999);
        assert_eq!(limit("limit=-1"), -1, "no floor — this reaches Postgres");
    }

    /// A limit Go cannot parse is `0`, not the default, and `LIMIT 0` returns nothing.
    /// Confirmed against the running Go server: `?limit=12abc` answers `{"users":[]}`.
    #[test]
    fn an_unparseable_limit_is_zero_and_not_the_default() {
        assert_eq!(limit("limit=12abc"), 0);
        assert_eq!(limit("limit=1e3"), 0);
        assert_eq!(limit("limit=abc"), 0);
        assert_eq!(
            limit("limit=%20 5"),
            0,
            "Go's Atoi rejects surrounding space"
        );
    }

    /// Go's `Atoi` distinguishes a syntax error from a range error and returns a different value
    /// for each. Both halves measured on the running server: the positive overflow answers with
    /// users, the negative one answers 500.
    #[test]
    fn an_overflowing_limit_saturates_the_way_gos_atoi_does() {
        assert_eq!(go_atoi("99999999999999999999"), i64::MAX);
        assert_eq!(go_atoi("-99999999999999999999"), i64::MIN);
        assert_eq!(go_atoi("nope"), 0);
        assert_eq!(go_atoi("+5"), 5, "Go's Atoi accepts a leading plus");

        assert_eq!(limit("limit=99999999999999999999"), USER_SEARCH_MAX_LIMIT);
        assert_eq!(
            limit("limit=-99999999999999999999"),
            i64::MIN,
            "no floor, so the negative overflow reaches the query and fails it"
        );
    }

    #[test]
    fn the_limit_helper_agrees_with_the_parser() {
        assert_eq!(autocomplete_limit(None), USER_SEARCH_DEFAULT_LIMIT);
        assert_eq!(
            autocomplete_limit(Some(String::new())),
            USER_SEARCH_DEFAULT_LIMIT
        );
        assert_eq!(autocomplete_limit(Some("0".to_owned())), 0);
    }

    /// `manage_system` forces full-name search on regardless of the setting, and only the
    /// non-admin branch consults it. Written as the table because the two inputs are easy to
    /// collapse into a single `||` that then hides which one was consulted.
    #[test]
    fn allow_full_names_is_the_permission_or_the_setting() {
        assert!(allow_full_names(true, false), "admin overrides the setting");
        assert!(allow_full_names(true, true));
        assert!(allow_full_names(false, true), "the setting is consulted");
        assert!(!allow_full_names(false, false), "and it can say no");
    }

    /// The sanitiser this route hands every user, including the caller's own row. A non-admin
    /// never sees `auth_data`/`auth_service` on anyone — those two flags have no config source,
    /// so the populated map's strict mode strips them.
    #[test]
    fn autocomplete_sanitises_with_no_self_exception() {
        let non_admin = sanitize_options(true, true, false);
        assert_eq!(non_admin.get("email"), Some(&true));
        assert_eq!(non_admin.get("fullname"), Some(&true));
        assert_eq!(non_admin.get("authservice"), None);
        assert_eq!(non_admin.get("authdata"), None);

        let admin = sanitize_options(false, false, true);
        assert_eq!(admin.get("authservice"), Some(&true));
        assert_eq!(admin.get("authdata"), Some(&true));
    }
}
