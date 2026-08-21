//! Port of `getUser` (channels/api4/user.go:305), reached as `GET /api/v4/users/me` and
//! `GET /api/v4/users/{user_id}`; `getUserByUsername`; and `getUsersByIds`
//! (`POST /api/v4/users/ids`).

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, PERMISSION_VIEW_MEMBERS};
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

#[cfg(test)]
mod tests {
    use mm_model::user::User;
    use std::collections::HashMap;

    use super::{
        UsersByIdsRequest, parse_users_by_ids_request, sanitize_options,
        segment_matches_username_mux,
    };

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
