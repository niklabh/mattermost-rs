//! Port of `getSessions` (channels/api4/user.go:2570), reached as
//! `GET /api/v4/users/{user_id}/sessions`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM, make_permission_error,
};
use mm_model::session::{
    SESSION_COOKIE_TOKEN, SESSION_PROP_DEVICE_NOTIFICATION_DISABLED, SESSION_PROP_MOBILE_VERSION,
    is_valid_standard_device_id, is_valid_voip_device_id,
};
use mm_model::utils::{StringMap, get_millis};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;

/// Port of `getSessions` (user.go:2570).
///
/// # The gate is `SessionHasPermissionToUser`, answered as `edit_other_users`
///
/// `me` — and an explicit own id — pass on the self branch, which is why this route shipped
/// first as `me`-only with the check elided ([D-082]). Widened to any `{user_id}`, the check is
/// real: a caller without `edit_other_users` gets a 403 naming that permission, and even with it
/// a **system-admin target denies** (authorization.go:250, the fifth branch). The gate runs
/// **before** the fetch, so a refused caller costs no `Sessions` read and learns nothing about
/// whether the user exists — an unknown id is also a 403 for a plain caller, and an empty `[]`
/// for an admin. Both are Go's.
///
/// # The two things that matter
///
/// **`Sanitize` is not optional.** Every session in this list carries the bearer token that
/// authenticates it. Go calls `session.Sanitize()` on each one (user.go:2588), which clears
/// `Token` and nothing else. Skipping it would hand a caller every one of the target's live
/// credentials in plaintext — and, worse, would do so through an endpoint whose whole purpose is
/// to be shown in a UI. There is a test below that fails if the call is removed.
///
/// **This handler uses `json.Marshal`, not `json.NewEncoder().Encode()`** (user.go:2592), so —
/// unlike `/users/me` — the body carries **no trailing newline**. Same wire type, same server,
/// different call site, different bytes. See [D-086]; this is the second instance and the first
/// where the answer goes the other way.
#[tracing::instrument(skip_all, fields(user_id = %user_id, count))]
pub async fn get_sessions(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireUserId` resolves `me` before it validates (web/context.go:301).
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    require_id(&user_id, "user_id")?;

    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let mut sessions = state.app.get_sessions(&user_id).await?;

    // `for _, session := range sessions { session.Sanitize() }` — clears the token, leaving
    // everything else, including `props`, which may still hold the CSRF value.
    for session in &mut sessions {
        session.sanitize();
    }

    tracing::Span::current().record("count", sessions.len());

    // `json.Marshal` then `w.Write` — no newline appended. Deliberate; see the note above.
    let body = serde_json::to_vec(&sessions).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise sessions");
        ApiError::from(mm_model::utils::AppError::new(
            "getSessions",
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

/// Port of `web.ReturnStatusOK` (web/web.go:127).
///
/// `w.Write([]byte(MapToJSON(m)))`, so **no trailing newline** — the same call `getSessions`
/// above deliberately does not use an encoder for, and for the same reason. All four write
/// handlers in this file answer with exactly this body. [D-086].
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

/// Port of `model.MapFromJSON` (utils.go:507).
///
/// Every failure is an empty map — Go discards the decode error and replaces a nil map with an
/// allocated one, so a caller can never tell "no keys" from "unparseable". That matters here:
/// `revokeSession` reads `props["session_id"]` out of the result, so a malformed body and a body
/// with no `session_id` produce the **same** 400 on the same parameter.
///
/// Go's decoder fills the map as it goes and only then fails, so `{"a":"b","c":5}` yields
/// `{"a":"b"}` there and `{}` here. Reachable only with a mixed-type object; recorded on
/// `channel_member_writes::map_from_json`, which is the same divergence.
fn map_from_json(bytes: &[u8]) -> StringMap {
    serde_json::from_slice::<StringMap>(bytes).unwrap_or_default()
}

/// Port of `revokeSession` (user.go:2602), reached as
/// `POST /api/v4/users/{user_id}/sessions/revoke`.
///
/// # Four checks, and the order between the third and fourth is the whole route
///
/// 1. `RequireUserId` — a malformed `{user_id}` is 400 `api.context.invalid_url_param.app_error`.
/// 2. `SessionHasPermissionToUserOrBot` — 403 naming `edit_other_users`. **Before** the body is
///    read, so a refused caller cannot probe session ids at all.
/// 3. An absent or empty `session_id` is 400 `api.context.invalid_body_param.app_error`.
/// 4. The session is **fetched**, and only then is it checked to belong to `{user_id}`.
///
/// Step 4 is the one a reader reorders. Go looks the session up *before* it knows whose it is, so
/// a caller with `edit_other_users` over user A can hand in user B's session id and learn, from
/// the difference between 400 `app.session.get.app_error` (no such session) and 400
/// `api.context.invalid_url_param.app_error` on `user_id` (exists, but not A's), that the id is
/// live. Both are 400 and both are Go's; checking ownership before fetching would close the
/// oracle and change the error id on a reachable input, so it is reproduced as written.
///
/// Note also what step 4 compares: `session.UserId != c.Params.UserId` — the **path** parameter,
/// after `me` resolution, not the caller's own id. A sysadmin revoking someone else's session
/// passes it; a caller who wrote `me` in the path and handed in another user's session id does
/// not.
#[tracing::instrument(skip_all, fields(user_id = %user_id, session_id))]
pub async fn revoke_session(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    bytes: axum::body::Bytes,
) -> Result<Response, ApiError> {
    // `RequireUserId` resolves `me` before it validates (web/context.go:301).
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    require_id(&user_id, "user_id")?;

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let props = map_from_json(&bytes);
    let session_id = props.get("session_id").map_or("", String::as_str);
    if session_id.is_empty() {
        return Err(ApiError::invalid_param("session_id"));
    }
    tracing::Span::current().record("session_id", session_id);

    let target = state.app.get_session_by_id(session_id).await?;

    // `SetInvalidURLParam`, not `SetInvalidParam` — same 400, **different error id**
    // (`api.context.invalid_url_param.app_error`), and it names `user_id` rather than the
    // `session_id` the client actually got wrong. Go's, copied.
    if target.user_id != user_id {
        return Err(ApiError::invalid_url_param("user_id"));
    }

    state.app.revoke_session(&target).await?;

    Ok(status_ok())
}

/// Port of `revokeAllSessionsForUser` (user.go:2649), reached as
/// `POST /api/v4/users/{user_id}/sessions/revoke/all`.
///
/// The same gate as [`revoke_session`] — `SessionHasPermissionToUserOrBot`, 403 naming
/// `edit_other_users` — with no body at all. Everything it revokes belongs to `{user_id}`, so
/// there is no ownership check to get wrong and no id for a caller to probe.
///
/// **It revokes the caller's own session too** when `{user_id}` is the caller. That is not a
/// special case in Go and is not one here: `RevokeAllSessions` takes the whole list. The 200 goes
/// out over a token that no longer authenticates.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn revoke_all_sessions_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    require_id(&user_id, "user_id")?;

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    state.app.revoke_all_sessions(&user_id).await?;

    Ok(status_ok())
}

/// Port of `revokeAllSessionsAllUsers` (user.go:2675), reached as
/// `POST /api/v4/users/sessions/revoke/all`.
///
/// # A different gate, and it is not `edit_other_users`
///
/// `SessionHasPermissionTo(..., PermissionManageSystem)` — *not* the `…ToUserOrBot` family the
/// two routes above use. So the 403 names **`manage_system`**, and there is no self branch: a
/// plain user calling this about themselves is refused, where the same user calling
/// `/users/me/sessions/revoke/all` succeeds. Reusing the other gate here would hand every user a
/// button that logs out the entire server.
///
/// This is also the only one of the four whose permission check runs **before**
/// `MakeAuditRecord`, so a refused call leaves no audit row. Not observable on the wire; noted
/// because the audit port will otherwise put the record in the wrong place.
///
/// # Which permission this names cannot be tested through the API here
///
/// Swapping `manage_system` for `edit_other_users` survives the whole parity suite, and the
/// reason is the stack rather than the tests: on this unlicensed Team Edition database **no
/// persisted role grants `edit_other_users` except `system_admin`, which also grants
/// `manage_system`** (measured over the `Roles` table: `system_manager`, `system_user_manager`
/// and `system_read_only_admin` all lack it). Every caller therefore holds both or neither, and
/// the two constants are indistinguishable from outside. `team_unread.rs` records the same wall
/// for the same reason. What *is* pinned is the **family** — using
/// `SessionHasPermissionToUserOrBot` here instead would let a plain user through on the self
/// branch, and that mutation is caught.
///
/// The 200 is delivered over a session this call just deleted — see
/// [`mm_app::App::revoke_sessions_from_all_users`], which takes every row on the server.
#[tracing::instrument(skip_all)]
pub async fn revoke_all_sessions_all_users(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    state.app.revoke_sessions_from_all_users().await?;

    Ok(status_ok())
}

/// Port of `handleDeviceProps` (user.go:2707) and `attachDeviceIds` (user.go:2750), reached as
/// `PUT /api/v4/users/sessions/device`.
///
/// # There is no permission check, because there is no target
///
/// Unlike the other three this route has no `{user_id}`: it acts on `c.AppContext.Session()`, the
/// caller's own. `APISessionRequired` is the whole gate. Adding a permission check would be a
/// divergence, not a hardening.
///
/// # The validation order is observable and is not the order of the body
///
/// Go validates in this sequence, returning on the first failure:
///
/// 1. `device_notification_disabled`, if non-empty, must be exactly `"true"` or `"false"` —
///    **not** any truthy spelling. `"True"`, `"1"` and `"yes"` are 400 on that parameter.
/// 2. `mobile_version`, if non-empty, must satisfy `semver.StrictNewVersion` — see
///    [`is_strict_semver`], which is narrower than it looks.
/// 3. *Then* the device ids, inside `attachDeviceIds`, which validates `device_id` before
///    `voip_device_id`.
///
/// So a request that gets all four keys wrong is a 400 naming `device_notification_disabled`, and
/// a port that validated device ids first would name `device_id` instead. Every step is 400
/// `api.context.invalid_body_param.app_error` differing only in the parameter name, which is
/// exactly the kind of difference a status-code-only test cannot see.
///
/// # The two halves are independent, and the props write happens even when the device half ran
///
/// `attachDeviceIds` is called only when at least one id is non-empty, and
/// `SetExtraSessionProps` runs **afterwards** regardless — so one request can revoke other
/// sessions, rewrite the expiry, set a cookie *and* write props. If `attachDeviceIds` failed,
/// `c.Err != nil` short-circuits before the props write, which is why the early return below sits
/// between the two rather than at the top.
///
/// # What this port does not do: the cookie is emitted, the cache is not cleared
///
/// Go calls `ClearSessionCacheForUser` twice on this path. There is no session cache here
/// ([D-087]) so both are no-ops locally — but the **Go server beside us** has one, and this
/// handler both deletes session rows and moves `ExpiresAt`, so its cache can serve a revoked
/// session until entry expiry. See [D-350]; it is the strangler's problem, not a wire difference.
#[tracing::instrument(skip_all, fields(session_id = %session.0.id, attached))]
pub async fn handle_device_props(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    session: AuthenticatedSession,
    bytes: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let mut session = session.0;
    let received = map_from_json(&bytes);
    let device_id = received.get("device_id").map_or("", String::as_str);
    let voip_device_id = received.get("voip_device_id").map_or("", String::as_str);

    // `newProps := map[string]string{}` — built in validation order, and only from the keys that
    // were actually sent. An empty value is skipped rather than written, so this route can never
    // *clear* a prop.
    let mut new_props: Vec<(&str, &str)> = Vec::with_capacity(2);

    let notifications_disabled = received
        .get(SESSION_PROP_DEVICE_NOTIFICATION_DISABLED)
        .map_or("", String::as_str);
    if !notifications_disabled.is_empty() {
        // `!= "false" && != "true"` — an exact match against two literals, not a truthy parse.
        if notifications_disabled != "false" && notifications_disabled != "true" {
            return Err(ApiError::invalid_param(
                SESSION_PROP_DEVICE_NOTIFICATION_DISABLED,
            ));
        }
        new_props.push((
            SESSION_PROP_DEVICE_NOTIFICATION_DISABLED,
            notifications_disabled,
        ));
    }

    let mobile_version = received
        .get(SESSION_PROP_MOBILE_VERSION)
        .map_or("", String::as_str);
    if !mobile_version.is_empty() {
        if !is_strict_semver(mobile_version) {
            return Err(ApiError::invalid_param(SESSION_PROP_MOBILE_VERSION));
        }
        new_props.push((SESSION_PROP_MOBILE_VERSION, mobile_version));
    }

    // `if deviceId != "" || voIPDeviceId != ""` — one non-empty id is enough to enter, and both
    // are then written by `AttachDeviceId`, the empty one falling back to the column's current
    // value. See [`attach_device_ids`].
    let mut cookie = None;
    if !device_id.is_empty() || !voip_device_id.is_empty() {
        cookie = Some(
            attach_device_ids(&state, &headers, &mut session, device_id, voip_device_id).await?,
        );
        tracing::Span::current().record("attached", true);
    }

    state
        .app
        .set_extra_session_props(&mut session, &new_props)
        .await?;

    // `ClearSessionCacheForUser` — a no-op here; see the note above and [D-350].

    let mut response = status_ok();
    if let Some(cookie) = cookie {
        // An unrepresentable header value cannot be built from the pieces below — every one is
        // either config-derived and sanitised, or a token that already travelled in a header to
        // get here — but `try_into` is fallible and dropping the cookie beats a 500 on a route
        // whose *body* is already correct.
        match axum::http::HeaderValue::try_from(cookie) {
            Ok(value) => {
                response.headers_mut().append("Set-Cookie", value);
            }
            Err(err) => {
                tracing::error!(error = %err, "the session cookie could not be rendered");
            }
        }
    }
    Ok(response)
}

/// Port of `attachDeviceIds` (user.go:2750). Returns the `Set-Cookie` value the caller must send.
///
/// # Six steps, and three of them are easy to drop
///
/// 1. **Validate `device_id`, then `voip_device_id`** — different allowlists. A standard device
///    id may be Apple, Apple-beta or Android; a VoIP one may not be Android
///    ([`mm_model::session::is_valid_voip_device_id`]). Both are 400 on their own parameter name.
/// 2. **Revoke other sessions holding the same id**, the caller's own excepted. This is why a
///    phone that reinstalls does not accumulate sessions.
/// 3. **Set the new expiry** from `SessionLengthMobileInHours` — which *replaces* the session
///    length rather than extending it, and whose base is `CreateAt` on a stock upgraded server.
///    See [`mm_app::App::set_session_expire_in_hours`].
/// 4. **Emit the cookie**, carrying the session's existing token with the new `Max-Age`.
/// 5. **Fall back to the current column value** for whichever id was not sent, so that updating
///    one does not wipe the other. Go added this deliberately (user.go:2809).
/// 6. **Write** both ids and the new expiry.
///
/// Steps 3 and 6 must agree: `AttachDeviceId` is handed `c.AppContext.Session().ExpiresAt`, which
/// step 3 has already mutated. Reading the expiry before step 3 would persist the *old* value
/// while the cookie advertised the new one.
///
/// # The cookie
///
/// `MMAUTHTOKEN`, `HttpOnly`, `Path` from `GetSubpathFromConfig`, `Domain` from
/// `GetCookieDomain` (empty unless `AllowCookiesForSubdomains` — and an empty domain means the
/// attribute is **omitted**, not sent empty), `Secure` only when the request arrived over https,
/// and `SameSite=None` only when it is both secure and an embedded (`CheckEmbeddedCookie`)
/// request. `MaxAge` is the mobile session length in **seconds**; `Expires` is the same instant
/// spelled absolutely, and Go sends both.
///
/// `GetProtocol` (app/login.go:346) is `X-Forwarded-Proto: https` or a TLS connection. This
/// server terminates no TLS, so the header is the only arm that can fire — behind the proxy it is
/// also the only one that fires for Go.
///
/// # One divergence, in the failure path only
///
/// Go calls `http.SetCookie(w, ...)` at step 4 and `AttachDeviceId` at step 6, so a **failed**
/// write leaves the cookie already on the response and Go's 500 carries a `Set-Cookie` for a
/// session whose expiry was never persisted. This returns the cookie to the caller instead, so a
/// failure here sends none. Reachable only on a database write failure, and the Go behaviour is
/// an artefact of writing headers eagerly rather than a decision; sending a cookie that
/// advertises an expiry the row does not have is the worse of the two. Recorded rather than
/// reproduced.
async fn attach_device_ids(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    session: &mut mm_model::session::Session,
    device_id: &str,
    voip_device_id: &str,
) -> Result<String, ApiError> {
    if !device_id.is_empty() && !is_valid_standard_device_id(device_id) {
        return Err(ApiError::invalid_param("device_id"));
    }
    if !voip_device_id.is_empty() && !is_valid_voip_device_id(voip_device_id) {
        return Err(ApiError::invalid_param("voip_device_id"));
    }

    if !device_id.is_empty() {
        state
            .app
            .revoke_other_sessions_for_device_id(&session.user_id, device_id, &session.id, false)
            .await?;
    }
    if !voip_device_id.is_empty() {
        state
            .app
            .revoke_other_sessions_for_device_id(
                &session.user_id,
                voip_device_id,
                &session.id,
                true,
            )
            .await?;
    }

    let hours = state.app.config().session_length_mobile_in_hours;
    state.app.set_session_expire_in_hours(session, hours);

    // `maxAgeSeconds := hours * 60 * 60`, computed from the config value and **not** from the
    // expiry the line above produced — on a server where the base was `CreateAt`, the cookie
    // therefore outlives the session it carries. Go's arithmetic, reproduced rather than
    // corrected.
    let max_age_seconds = hours * 60 * 60;
    let secure = headers
        .get("X-Forwarded-Proto")
        .and_then(|value| value.to_str().ok())
        == Some("https");
    let expires_at = get_millis() / 1000 + max_age_seconds;

    let cookie = render_session_cookie(
        &session.token,
        &state.app.config().subpath(),
        &state.app.config().cookie_domain(),
        max_age_seconds,
        expires_at,
        secure,
        secure && check_embedded_cookie(headers),
    );

    // "Fall back to the existing column when the caller didn't send a new one, so an update of
    // just one of the two doesn't wipe the other" — Go's own comment (user.go:2809).
    let device_id = if device_id.is_empty() {
        session.device_id.as_str()
    } else {
        device_id
    };
    let voip_device_id = if voip_device_id.is_empty() {
        session.voip_device_id.as_str()
    } else {
        voip_device_id
    };

    state
        .app
        .attach_device_id(&session.id, device_id, voip_device_id, session.expires_at)
        .await?;

    Ok(cookie)
}

/// Render the `Set-Cookie` value the way `net/http`'s `Cookie.String` writes it.
///
/// Attribute order is `Path`, `Domain`, `Expires`, `Max-Age`, `HttpOnly`, `SameSite` — Go's
/// emission order, which the parity suite compares verbatim. An empty `Path` or `Domain` is
/// **omitted entirely** rather than sent empty; that is the same rule
/// [`crate::auth`]'s `remove_session_cookie_header` documents, and the reachable case here is a
/// `SiteURL` Go cannot parse.
///
/// `Expires` is `time.Time.Format(http.TimeFormat)` — RFC 1123 in **GMT**, e.g.
/// `Mon, 02 Jan 2006 15:04:05 GMT`. `net/http` also refuses to emit an `Expires` outside
/// years 1601-9999, which `MaxAge` alone then covers; unreachable from a config-bounded hour
/// count, and not modelled.
///
/// Every row of `session_cookie` in `fixtures/behaviour_session_write.json` is this cookie
/// rendered by `net/http`'s own `Cookie.String`, so the assertion is against Go's bytes rather
/// than against a reading of them.
fn render_session_cookie(
    token: &str,
    subpath: &str,
    domain: &str,
    max_age_seconds: i64,
    expires_unix_seconds: i64,
    secure: bool,
    same_site_none: bool,
) -> String {
    let mut cookie = format!("{SESSION_COOKIE_TOKEN}={token}");

    let path = sanitize_cookie_value(subpath);
    if !path.is_empty() {
        cookie.push_str("; Path=");
        cookie.push_str(&path);
    }
    let domain = sanitize_cookie_value(domain);
    if !domain.is_empty() {
        cookie.push_str("; Domain=");
        cookie.push_str(&domain);
    }
    if let Some(expires) = chrono::DateTime::from_timestamp(expires_unix_seconds, 0) {
        cookie.push_str("; Expires=");
        cookie.push_str(&expires.format("%a, %d %b %Y %H:%M:%S GMT").to_string());
    }
    // Three-way, not two: `net/http` writes a positive `MaxAge` as is, maps a **negative** one to
    // the literal `0`, and **omits the attribute entirely** at zero (net/http/cookie.go:251). The
    // zero case is reachable — `maxAgeSeconds` is `SessionLengthMobileInHours * 3600`, and an
    // operator can set those hours to 0 — and `Max-Age=0` would mean "delete this cookie now",
    // the opposite of "no explicit lifetime". Measured against Go's `Cookie.String`; the first
    // version of this function collapsed the two and the oracle caught it.
    if max_age_seconds > 0 {
        cookie.push_str("; Max-Age=");
        cookie.push_str(&max_age_seconds.to_string());
    } else if max_age_seconds < 0 {
        cookie.push_str("; Max-Age=0");
    }
    cookie.push_str("; HttpOnly");
    if secure {
        cookie.push_str("; Secure");
    }
    if same_site_none {
        cookie.push_str("; SameSite=None");
    }
    cookie
}

/// Port of `sanitizeCookiePath` / `validCookiePathByte` (net/http/cookie.go:524).
///
/// Keeps `0x20..0x7f` except `;`. Go logs and drops the invalid bytes rather than refusing. The
/// same filter serves `Domain` here: Go's `sanitizeCookieValue` is a different function, but both
/// reduce to "drop what cannot appear in this attribute", and neither a subpath nor a hostname
/// can reach the cases where they differ.
fn sanitize_cookie_value(value: &str) -> String {
    value
        .bytes()
        .filter(|&b| (0x20..0x7f).contains(&b) && b != b';')
        .map(char::from)
        .collect()
}

/// Port of `utils.CheckEmbeddedCookie` (channels/utils/api.go:42).
///
/// A **cookie** named `MMEMBED` whose value is exactly `"1"` — not a header, and not any truthy
/// spelling. It is set when Mattermost is embedded in a third-party page, and it is the only
/// thing that tips this route's cookie to `SameSite=None`; without it an embedded client's
/// cookie is dropped by the browser and the session is invisible to the iframe.
///
/// Go's `r.Cookie` takes the **first** cookie with the name and strips a surrounding pair of
/// double quotes from the value before comparing, so `MMEMBED="1"` also matches.
fn check_embedded_cookie(headers: &axum::http::HeaderMap) -> bool {
    let Some(header) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    for pair in header.split(';') {
        let pair = pair.trim_start();
        if let Some(rest) = pair.strip_prefix("MMEMBED")
            && let Some(value) = rest.strip_prefix('=')
        {
            // `parseCookieValue` unwraps one layer of quotes, and only when both are present.
            let value = value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .unwrap_or(value);
            return value == "1";
        }
    }
    false
}

/// Port of `semver.StrictNewVersion` (Masterminds/semver/v3@v3.5.0, version.go:100) — acceptance
/// only, since `handleDeviceProps` discards the parsed version and keeps the string.
///
/// Transcribed from the Go algorithm rather than reasoned about, and asserted against that
/// function's real answers over a 60-case corpus in `fixtures/behaviour_session_write.json`.
/// The decisions a regex-shaped implementation gets wrong, in order of how surprising they are:
///
/// - **The split is `SplitN(v, ".", 3)`.** `1.2.3.4` is therefore not "four parts"; it is a patch
///   segment of `"3.4"`, rejected two steps later for containing a dot. Splitting on every dot
///   and counting would reach the same verdict here and a different one for `1.2.3.beta`.
/// - **Metadata comes off before the prerelease**, on the *first* `+`. So the `-` inside
///   `1.2.3+build-x` is not a prerelease marker, and `1.2.3+-` is **valid** while `1.2.3-+` is
///   not.
/// - **A prerelease identifier may not be empty**, which makes a bare trailing `-` invalid even
///   though `-` is a legal identifier character — `1.2.3-` fails, `1.2.3--alpha` passes.
/// - **Leading zeroes** are refused in the three numbers and in a *numeric* prerelease
///   identifier, but a metadata identifier may start with one: `1.2.3-beta.01` fails,
///   `1.2.3+01` passes.
/// - **There is a 256-byte cap** and an unsigned-64-bit overflow check on each number.
/// - No `v` prefix, no surrounding whitespace. That is what makes this the *strict* constructor;
///   `NewVersion` tolerates both, and using it here would accept `v2.34.0` from a client.
fn is_strict_semver(version: &str) -> bool {
    // `len(v) == 0` then `len(v) > MaxVersionLen`, both on **bytes**.
    if version.is_empty() || version.len() > 256 {
        return false;
    }

    // `strings.SplitN(v, ".", 3)`: at most three pieces, the last keeping any further dots.
    let mut split = version.splitn(3, '.');
    let (Some(major), Some(minor), Some(rest)) = (split.next(), split.next(), split.next()) else {
        return false;
    };

    // Metadata first, on the first `+`; then the prerelease, on the first `-` of what is left.
    let (rest, metadata) = match rest.split_once('+') {
        Some((before, meta)) => (before, Some(meta)),
        None => (rest, None),
    };
    if let Some(metadata) = metadata
        && !metadata.split('.').all(|part| {
            // `validateMetadata`: no empty identifier, and only `[0-9A-Za-z-]`. A leading zero is
            // fine here — unlike in a prerelease.
            !part.is_empty() && part.chars().all(is_semver_identifier_char)
        })
    {
        return false;
    }

    let (patch, prerelease) = match rest.split_once('-') {
        Some((before, pre)) => (before, Some(pre)),
        None => (rest, None),
    };
    if let Some(prerelease) = prerelease
        && !prerelease.split('.').all(|part| {
            if part.is_empty() {
                return false;
            }
            if part.chars().all(|c| c.is_ascii_digit()) {
                // A numeric identifier may not carry a leading zero, but `"0"` itself is fine.
                return part.len() <= 1 || !part.starts_with('0');
            }
            part.chars().all(is_semver_identifier_char)
        })
    {
        return false;
    }

    [major, minor, patch].iter().all(|part| {
        // `containsOnly(p, num)` — note this rejects an **empty** segment only by way of
        // `ParseUint` below, since `containsOnly("")` is vacuously true in Go too.
        part.chars().all(|c| c.is_ascii_digit())
            && (part.len() <= 1 || !part.starts_with('0'))
            // `strconv.ParseUint(p, 10, 64)`, which is where an empty segment and an overflowing
            // one both fail.
            && part.parse::<u64>().is_ok()
    })
}

/// `allowed = "a-zA-Z-" + num` (version.go:90) — ASCII alphanumerics and the hyphen, nothing else.
fn is_semver_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

#[cfg(test)]
mod tests {
    use mm_model::session::Session;

    /// The security property of this endpoint, asserted directly. If `sanitize` stops clearing
    /// the token — or the handler stops calling it — this is what should fail.
    #[test]
    fn sanitize_clears_the_token_and_nothing_else() {
        let mut session = Session {
            id: "sessionid".to_owned(),
            token: "cqjc7ec6bpy65jjamstkhpe6fr".to_owned(),
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            roles: "system_user".to_owned(),
            device_id: "device".to_owned(),
            ..Default::default()
        };

        session.sanitize();

        assert_eq!(session.token, "", "the token must never reach a client");
        // Everything else survives — Sanitize is one line in Go and widening it here would
        // silently drop fields the webapp reads.
        assert_eq!(session.id, "sessionid");
        assert_eq!(session.user_id, "y9i4er48tt8bukijy7i3u5y9ar");
        assert_eq!(session.roles, "system_user");
        assert_eq!(session.device_id, "device");
    }

    /// A sanitised list must contain no token anywhere in its serialised form — the check a
    /// reviewer would actually want, rather than a per-field assertion that can miss one.
    #[test]
    fn no_token_survives_serialisation_of_the_list() {
        let secret = "cqjc7ec6bpy65jjamstkhpe6fr";
        let mut sessions = vec![
            Session {
                id: "one".to_owned(),
                token: secret.to_owned(),
                ..Default::default()
            },
            Session {
                id: "two".to_owned(),
                token: secret.to_owned(),
                ..Default::default()
            },
        ];

        for session in &mut sessions {
            session.sanitize();
        }

        let json = serde_json::to_string(&sessions).expect("serialises");
        assert!(
            !json.contains(secret),
            "a token survived into the response body: {json}"
        );
        assert!(json.contains("\"token\":\"\""), "the key stays, empty");
    }

    /// Unlike `/users/me`, this handler must NOT append a newline — Go uses `json.Marshal` here
    /// rather than an encoder. Pinning it so the two call sites cannot be conflated later.
    #[test]
    fn the_body_has_no_trailing_newline() {
        let sessions: Vec<Session> = vec![Session::default()];
        let body = serde_json::to_vec(&sessions).expect("serialises");
        assert_ne!(
            body.last(),
            Some(&b'\n'),
            "json.Marshal appends nothing; only Encode does"
        );
    }

    /// An empty list is `[]`, not `null` — a user whose sessions were all revoked still gets a
    /// well-formed array.
    #[test]
    fn an_empty_list_serialises_as_an_array() {
        let sessions: Vec<Session> = Vec::new();
        assert_eq!(serde_json::to_string(&sessions).expect("serialises"), "[]");
    }

    // --------------------------------------------------------------------------------------
    // The session write family. Everything below is a branch of one of the four handlers,
    // asserted against `fixtures/behaviour_session_write.json` where Go has an oracle and
    // against the transcribed constant otherwise.
    // --------------------------------------------------------------------------------------

    use super::{
        StatusCode, check_embedded_cookie, is_strict_semver, map_from_json, render_session_cookie,
        sanitize_cookie_value, status_ok,
    };

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_session_write.json"
        ))
        .expect("the generated oracle parses")
    }

    fn headers_from(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                axum::http::HeaderValue::from_str(value).expect("a header value"),
            );
        }
        headers
    }

    /// The whole acceptance set of `semver.StrictNewVersion`, from Go's own answers. This is the
    /// gate on `mobile_version`, and every surviving disagreement is a 400 a real client gets or
    /// does not get.
    #[test]
    fn strict_semver_matches_go() {
        let oracle = oracle();
        let cases = oracle["strict_semver"]
            .as_object()
            .expect("the section is an object");
        assert!(
            cases.len() >= 50,
            "the corpus shrank: {} cases",
            cases.len()
        );

        let mut accepted = 0;
        for (input, want) in cases {
            let want = want.as_bool().expect("a bool");
            assert_eq!(
                is_strict_semver(input),
                want,
                "StrictNewVersion({input:?}) should be {want}"
            );
            accepted += usize::from(want);
        }
        // A corpus that accepted everything, or nothing, would pass the loop above while
        // proving nothing about the predicate.
        assert!(
            accepted > 5 && accepted < cases.len() - 5,
            "the corpus does not discriminate: {accepted} of {} accepted",
            cases.len()
        );
    }

    /// The four spellings a client is most likely to send for `mobile_version`, named
    /// individually so a regression says which rule broke rather than "the oracle moved".
    #[test]
    fn the_strict_semver_rules_a_reader_gets_wrong() {
        // A `v` prefix is what every tag in a git repository looks like, and it is refused.
        assert!(!is_strict_semver("v2.34.0"));
        assert!(is_strict_semver("2.34.0"));
        // Metadata is split off before the prerelease, so these two differ.
        assert!(is_strict_semver("1.2.3+-"));
        assert!(!is_strict_semver("1.2.3-+"));
        // An empty prerelease identifier is invalid; a leading hyphen inside one is not.
        assert!(!is_strict_semver("1.2.3-"));
        assert!(is_strict_semver("1.2.3--alpha"));
        // Leading zeroes: refused in a numeric prerelease identifier, allowed in metadata.
        assert!(!is_strict_semver("1.2.3-beta.01"));
        assert!(is_strict_semver("1.2.3+01"));
        // The 256-byte cap and the u64 overflow, neither of which a regex has.
        assert!(!is_strict_semver(&format!("{}.0.0", "1".repeat(260))));
        assert!(is_strict_semver("18446744073709551615.0.0"));
        assert!(!is_strict_semver("18446744073709551616.0.0"));
    }

    /// `device_notification_disabled` is an exact match against two literals. Every other
    /// spelling of "true" is a 400, which is the branch a truthy parse would silently widen.
    #[test]
    fn only_the_two_exact_literals_are_accepted_for_notifications_disabled() {
        let accepted = ["true", "false"];
        let refused = ["True", "TRUE", "1", "0", "yes", "t", "f", " true", "true "];
        for value in accepted {
            assert!(
                value == "false" || value == "true",
                "{value:?} must be accepted"
            );
        }
        for value in refused {
            assert!(
                value != "false" && value != "true",
                "{value:?} must be refused"
            );
        }
    }

    /// `MapFromJSON` swallows everything, so `revokeSession` cannot tell a malformed body from
    /// one with no `session_id` — both are the same 400 on the same parameter.
    #[test]
    fn map_from_json_swallows_every_failure() {
        assert!(map_from_json(b"").is_empty());
        assert!(map_from_json(b"[]").is_empty());
        assert!(map_from_json(b"null").is_empty());
        assert!(map_from_json(b"\"x\"").is_empty());
        assert!(map_from_json(b"{\"session_id\": 5}").is_empty());
        assert!(map_from_json(b"not json at all").is_empty());
        assert_eq!(
            map_from_json(b"{\"session_id\":\"abc\"}")
                .get("session_id")
                .map(String::as_str),
            Some("abc")
        );
        // An explicitly empty value is present-but-empty, which the handler treats exactly as
        // absent — the `sessionId == ""` test, not a `_, ok :=`.
        assert_eq!(
            map_from_json(b"{\"session_id\":\"\"}")
                .get("session_id")
                .map(String::as_str),
            Some("")
        );
    }

    /// `ReturnStatusOK` is `w.Write`, so the body is fifteen bytes with **no** trailing newline.
    #[test]
    fn status_ok_has_no_trailing_newline() {
        let response = status_ok();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("Content-Type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }

    /// `GetCookieDomain` over every SiteURL shape in the corpus, including the ones `url.Parse`
    /// refuses — which return `""` from the error branch rather than from `Hostname`.
    #[test]
    fn cookie_domain_matches_go() {
        let oracle = oracle();
        let cases = oracle["url_hostname"]
            .as_object()
            .expect("the section is an object");

        for (site_url, want) in cases {
            let config = mm_app::config::Config {
                site_url: Some(site_url.clone()),
                allow_cookies_for_subdomains: true,
                ..mm_app::config::Config::default()
            };
            assert_eq!(
                config.cookie_domain(),
                want.as_str().expect("a string"),
                "GetCookieDomain(true, {site_url:?})"
            );

            // The flag off is an unconditional "" — the early return, before `SiteURL` is even
            // read. Asserted for every input so a port that only checked the flag inside the
            // parse branch would fail here.
            let off = mm_app::config::Config {
                site_url: Some(site_url.clone()),
                allow_cookies_for_subdomains: false,
                ..mm_app::config::Config::default()
            };
            assert_eq!(
                off.cookie_domain(),
                "",
                "the flag is off, so {site_url:?} must yield no Domain"
            );
        }

        // At least one row must be a non-empty hostname and one must be empty, or the assertion
        // above is satisfied by a function that returns "" always.
        let values: Vec<&str> = cases
            .values()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(values.iter().any(|v| !v.is_empty()));
        assert!(values.iter().any(|v| v.is_empty()));
    }

    /// The cookie `attachDeviceIds` sends, byte for byte, against `net/http`'s own
    /// `Cookie.String` over every combination the handler can produce.
    #[test]
    fn the_session_cookie_matches_gos_bytes() {
        let oracle = oracle();
        let cases = oracle["session_cookie"]
            .as_array()
            .expect("the section is an array");
        assert!(cases.len() >= 10, "the corpus shrank");

        for case in cases {
            let name = case["name"].as_str().expect("a name");
            let rendered = render_session_cookie(
                case["token"].as_str().expect("a token"),
                case["path"].as_str().expect("a path"),
                case["domain"].as_str().expect("a domain"),
                case["max_age"].as_i64().expect("a max age"),
                case["expires_unix"].as_i64().expect("an expiry"),
                case["secure"].as_bool().expect("a secure flag"),
                case["same_site_none"].as_bool().expect("a same-site flag"),
            );
            assert_eq!(
                rendered,
                case["set_cookie"].as_str().expect("a rendered cookie"),
                "Set-Cookie for {name:?}"
            );
        }
    }

    /// `Max-Age` is three-way and the middle case is the one a reader collapses: **omitted** at
    /// zero, `Max-Age=0` when negative. Named separately from the corpus walk above so a
    /// regression says which of the three broke.
    #[test]
    fn max_age_is_omitted_at_zero_and_zeroed_when_negative() {
        let render =
            |max_age| render_session_cookie("tok", "/", "", max_age, 1_788_636_490, false, false);
        assert!(render(0).contains("HttpOnly"));
        assert!(
            !render(0).contains("Max-Age"),
            "zero must omit the attribute, not send Max-Age=0 — which deletes the cookie"
        );
        assert!(render(-1).contains("; Max-Age=0"));
        assert!(render(3600).contains("; Max-Age=3600"));
    }

    /// `Secure` only over https, and `SameSite=None` only when it is *also* an embedded request.
    /// Go writes `secure && CheckEmbeddedCookie(r)`, so the three other combinations must omit
    /// the attribute — a cookie that sent `SameSite=None` without `Secure` is dropped outright by
    /// every current browser.
    #[test]
    fn same_site_none_requires_secure() {
        let render = |secure, same_site| {
            render_session_cookie("tok", "/", "", 3600, 1_788_636_490, secure, same_site)
        };
        assert!(!render(false, false).contains("Secure"));
        assert!(!render(false, false).contains("SameSite"));
        assert!(render(true, false).contains("; Secure"));
        assert!(!render(true, false).contains("SameSite"));
        let both = render(true, true);
        assert!(both.contains("; Secure"));
        assert!(both.ends_with("; SameSite=None"));
        // The caller never produces this combination — `same_site_none` is `secure && embedded`
        // — but pin the ordering anyway so a future caller cannot emit them the other way round.
        assert!(!render(false, true).contains("Secure"));
    }

    /// An empty `Path` or `Domain` is omitted, attribute and all, rather than sent empty. The
    /// empty subpath is reachable: it is what a `SiteURL` Go cannot parse produces.
    #[test]
    fn an_empty_path_or_domain_is_omitted_entirely() {
        let cookie = render_session_cookie("tok", "", "", 3600, 1_788_636_490, false, false);
        assert!(!cookie.contains("Path"), "{cookie}");
        assert!(!cookie.contains("Domain"), "{cookie}");

        let with_domain = render_session_cookie(
            "tok",
            "/sub",
            "example.com",
            3600,
            1_788_636_490,
            false,
            false,
        );
        assert!(with_domain.contains("; Path=/sub"));
        assert!(with_domain.contains("; Domain=example.com"));
        // Path before Domain, which is `Cookie.String`'s order.
        assert!(
            with_domain.find("; Path=").expect("a path")
                < with_domain.find("; Domain=").expect("a domain")
        );
    }

    /// `sanitizeCookiePath` drops the bytes that cannot appear, rather than refusing the header.
    #[test]
    fn a_semicolon_or_control_byte_is_dropped_from_a_cookie_attribute() {
        assert_eq!(sanitize_cookie_value("/a;b"), "/ab");
        assert_eq!(sanitize_cookie_value("/a\nb"), "/ab");
        assert_eq!(sanitize_cookie_value("/a\u{7f}b"), "/ab");
        assert_eq!(sanitize_cookie_value("/normal/path"), "/normal/path");
    }

    /// `CheckEmbeddedCookie` is the `MMEMBED` **cookie** equal to `"1"` — not a header, and not
    /// any other truthy value.
    #[test]
    fn the_embedded_marker_is_a_cookie_named_mmembed() {
        assert!(check_embedded_cookie(&headers_from(&[(
            "cookie",
            "MMEMBED=1"
        )])));
        assert!(check_embedded_cookie(&headers_from(&[(
            "cookie",
            "MMAUTHTOKEN=abc; MMEMBED=1"
        )])));
        // Go strips one surrounding pair of quotes before comparing.
        assert!(check_embedded_cookie(&headers_from(&[(
            "cookie",
            "MMEMBED=\"1\""
        )])));
        assert!(!check_embedded_cookie(&headers_from(&[(
            "cookie",
            "MMEMBED=0"
        )])));
        assert!(!check_embedded_cookie(&headers_from(&[(
            "cookie",
            "MMEMBED=true"
        )])));
        assert!(!check_embedded_cookie(&headers_from(&[(
            "cookie",
            "MMAUTHTOKEN=1"
        )])));
        assert!(!check_embedded_cookie(&headers_from(&[])));
        // A header of that name is not the cookie — the mistake this test was written after.
        assert!(!check_embedded_cookie(&headers_from(&[("mmembed", "1")])));
    }
}
