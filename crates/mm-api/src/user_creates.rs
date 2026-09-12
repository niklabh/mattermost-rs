//! Account creation and the two e-mail-token routes (api4/user.go):
//!
//! ```text
//! POST /api/v4/users                            createUser                 (user.go:239)
//! POST /api/v4/users/email/verify/send          sendVerificationEmail      (user.go:2884)
//! POST /api/v4/users/password/reset/send        sendPasswordReset          (user.go:2094)
//! POST /api/v4/users/{user_id}/email/verify/member verifyUserEmailWithoutToken (user.go:3629)
//! ```
//!
//! # The query parameters are `t` and `iid`, not `token` and `invite_id`
//!
//! `createUser` reads `r.URL.Query().Get("t")`, `("iid")` and `("r")`. The audit record names the
//! *second* of them `invite_id`, which is where the longer spelling comes from — it is a label on
//! a log line, not a parameter a client sends. A port that looked for `?token=` would silently
//! take the anonymous-signup branch for every invitation, creating unverified accounts on a
//! closed server. The three are the entire routing decision for this handler.
//!
//! # What forwards, and why the forward is always before a write
//!
//! | route | served here | forwarded |
//! |---|---|---|
//! | `POST /users` | no `t`, no `iid` — the admin and anonymous-signup branches | either query parameter present |
//! | `/email/verify/send` | empty `email`; an address no account matches | a matched address |
//! | `/password/reset/send` | empty `email`; unmatched address; remote, SSO and magic-link refusals | everything that reaches the token |
//! | `/{user_id}/email/verify/member` | all of it | nothing |
//!
//! Every forward above is decided from the query string, or from a `SELECT`, before this process
//! has written anything. That is not a coincidence and it is the property that makes these three
//! safe to split: **`sendVerificationEmail` and `sendPasswordReset` both mint a `Tokens` row
//! before they send**, and a forward taken after that row existed would leave a live one-shot
//! credential behind for a request Go then handled from scratch. The served half of each is
//! exactly the prefix that precedes `Token().Save`, and
//! [`a_forwarded_send_writes_no_token`][crate::user_creates::tests] pins it.
//!
//! # These two routes are mostly refusals, and that is the honest answer
//!
//! `sendVerificationEmail` exists to send mail and there is no mail service here ([D-238]). Its
//! served half is genuinely small: a 400 for a missing `email`, and the `{"status":"OK"}` that Go
//! returns for an address it cannot match — which is not a trivial branch, because the whole
//! point of that `ReturnStatusOK` is to refuse to say whether the address exists. Getting it
//! wrong in the other direction would turn the route into an account oracle.
//!
//! `sendPasswordReset` has more of a served half: three refusals that Go raises *before* the
//! token — remote user, SSO user, guest magic-link — each a 400 with its own id, and each
//! rewritten to `{"status":"OK"}` when `ExperimentalEnableHardenedMode` is on. That toggle is the
//! subtlety: it turns three distinguishable errors into the same success a nonexistent address
//! gets, which is the point of a hardened mode, and a port that ignored it would leak on a server
//! that had switched it on.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, make_permission_error};
use mm_model::user::User;
use mm_model::utils::{AppError, StringMap};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;
use crate::proxy;

/// Port of `model.MapFromJSON` (utils.go:507) — every failure is an empty map.
///
/// The fourth copy of this two-liner in the crate, for the reason `auth_writes` states: several
/// agents edit this workspace at once, and a shared two-line helper is a worse merge risk than a
/// copy.
fn map_from_json(bytes: &[u8]) -> StringMap {
    serde_json::from_slice::<StringMap>(bytes).unwrap_or_default()
}

/// Port of `web.ReturnStatusOK` (web/web.go:127) — `{"status":"OK"}` with **no trailing
/// newline**, because it is a `w.Write` rather than an encoder.
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

/// One query parameter, Go's `r.URL.Query().Get(name)` — the **first** value for a repeated key,
/// and `""` for an absent one or one with no `=`.
fn query_get(request: &Request, name: &str) -> String {
    let Some(query) = request.uri().query() else {
        return String::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// Port of `json.NewDecoder(r.Body).Decode(&user)` for a struct target, which is **not** what
/// `serde_json::from_slice::<User>` does.
///
/// Three differences, each measured against Go rather than reasoned about:
///
/// 1. **A JSON array decodes into `model.User` in serde and not in Go.** `User` carries
///    `#[serde(default)]`, and serde's derived `deserialize_struct` accepts a *sequence* as well
///    as a map — so `[]` produced a zero user here and `api.context.invalid_body_param.app_error`
///    there. The parity suite caught it as a password-length error against Go's decode error.
///    Only an object (or `null`) may reach the decoder.
/// 2. **`null` is not an error in Go.** `Decode` into a non-pointer struct leaves it untouched
///    and returns nil, so `null` is the zero user and fails later, at `IsValid`.
/// 3. **Trailing bytes after the first value are ignored.** `Decoder.Decode` reads one value and
///    stops; `from_slice` refuses anything after it. Using a streaming `Deserializer` without
///    calling `end()` is what reproduces that.
#[allow(clippy::result_large_err)]
fn decode_user(bytes: &[u8]) -> Result<User, ApiError> {
    use serde::Deserialize;

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = serde_json::Value::deserialize(&mut deserializer).map_err(|err| {
        tracing::warn!(error = %err, "the user body is not JSON");
        ApiError::invalid_param("user")
    })?;

    match value {
        serde_json::Value::Null => Ok(User::default()),
        serde_json::Value::Object(_) => serde_json::from_value(value).map_err(|err| {
            tracing::warn!(error = %err, "the user body did not decode");
            ApiError::invalid_param("user")
        }),
        _ => {
            tracing::warn!("the user body is not a JSON object");
            Err(ApiError::invalid_param("user"))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users
// ---------------------------------------------------------------------------------------------

/// Port of `createUser` (api4/user.go:239).
///
/// # The handler is a four-way switch and nothing else
///
/// ```text
/// t   != ""                -> CreateUserWithToken     (forwarded)
/// iid != ""                -> CreateUserWithInviteId  (forwarded)
/// c.IsSystemAdmin()        -> CreateUserAsAdmin
/// otherwise                -> CreateUserFromSignup
/// ```
///
/// `t` wins over `iid` when both are present, and the admin check is consulted **only** when
/// neither is. So a system admin following an invitation link takes the invitation branch, with
/// its team join and its domain rules, and not the unguarded admin one.
///
/// There is **no permission check** — Go says so in a comment. `c.IsSystemAdmin()` is a
/// `manage_system` probe that selects a branch, not a gate, and an anonymous request simply
/// answers `false`.
///
/// # `SanitizeInput` runs before the switch and its argument is the admin probe
///
/// A non-admin's `auth_data`, `auth_service` and `email_verified` are cleared from the submitted
/// body; everyone's `remote_id`, the five timestamps, `failed_attempts` and the whole MFA triple
/// are cleared regardless, and `email` is trimmed. So `email_verified: true` from an anonymous
/// signup is discarded here, before the branch that would have honoured it.
///
/// # 201, and the body is the sanitised user
///
/// `w.WriteHeader(http.StatusCreated)` then the encoder — a **201**, not a 200, and the body has
/// already been through `Sanitize(map[string]bool{})` inside the app layer, so `password`,
/// `mfa_secret`, `mfa_used_timestamps` and `last_login` are gone. The handler does not sanitise
/// again.
#[tracing::instrument(skip_all, fields(forwarded = false, branch, admin))]
pub async fn create_user(
    State(state): State<AppState>,
    session: crate::auth_writes::OptionalSession,
    request: Request,
) -> Response {
    // The token and invite branches both end in `JoinUserToTeam` plus `AddDirectChannels`,
    // neither of which is ported. **Read before the body**: forwarding has to happen before the
    // body is consumed and, more importantly, before anything is written.
    let token_id = query_get(&request, "t");
    let invite_id = query_get(&request, "iid");
    if !token_id.is_empty() {
        tracing::Span::current().record("forwarded", true);
        tracing::Span::current().record("branch", "token");
        return proxy::forward_to_go(State(state), request).await;
    }
    if !invite_id.is_empty() {
        tracing::Span::current().record("forwarded", true);
        tracing::Span::current().record("branch", "invite_id");
        return proxy::forward_to_go(State(state), request).await;
    }

    // A licensed installation is Go's: the guest-invitation licence gates, the licensed
    // user-limit message and `CreateGuest` all live behind it, and none is ported.
    match state.app.license_state().await {
        Ok(mm_app::license::LicenseState::Licensed) => {
            tracing::Span::current().record("forwarded", true);
            tracing::Span::current().record("branch", "licensed");
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(_) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }

    // `c.IsSystemAdmin()` is `SessionHasPermissionTo(session, manage_system)` over whatever
    // session the request carried — and this route is an `APIHandler`, so there may be none.
    let is_admin = match session.0.as_ref() {
        Some(session) => {
            state
                .app
                .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
                .await
        }
        None => false,
    };
    tracing::Span::current().record("admin", is_admin);

    let (_request, bytes) = match split_body(request, "user").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };

    let mut user = match decode_user(&bytes) {
        Ok(user) => user,
        Err(err) => return err.into_response(),
    };

    user.sanitize_input(is_admin);

    let created = if is_admin {
        tracing::Span::current().record("branch", "admin");
        state.app.create_user_as_admin(&user).await
    } else {
        tracing::Span::current().record("branch", "signup");
        state.app.create_user_from_signup(&user).await
    };

    match created {
        Ok(ruser) => match serde_json::to_vec(&ruser) {
            Ok(body) => (
                StatusCode::CREATED,
                [
                    ("Content-Type", "application/json"),
                    ("x-mmrs-served-by", "rust"),
                ],
                body,
            )
                .into_response(),
            Err(err) => {
                tracing::error!(error = %err, "failed to serialise the created User");
                ApiError::from(AppError::new(
                    "createUser",
                    "api.marshal_error",
                    None,
                    String::new(),
                    500,
                ))
                .into_response()
            }
        },
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/email/verify/send
// ---------------------------------------------------------------------------------------------

/// Port of `sendVerificationEmail` (api4/user.go:2884).
///
/// # Exactly one status this route can answer other than 200
///
/// An empty `email` is a **400** naming `email`; *everything* else is `{"status":"OK"}`. Both
/// failure paths inside — the lookup and the send — are swallowed with the comment "Don't want to
/// leak whether the email is valid or not", so a nonexistent address, a suspended mail server and
/// a successful send are indistinguishable to the caller. The lower-casing happens before the
/// emptiness test, which makes no difference (`strings.ToLower("") == ""`) but is Go's order.
///
/// The served half is the 400 and the unmatched-address OK. A matched address forwards, because
/// `SendEmailVerification` mints a `Tokens` row before it sends and this process has no mail
/// service to send with.
#[tracing::instrument(skip_all, fields(forwarded = false, outcome))]
pub async fn send_verification_email(State(state): State<AppState>, request: Request) -> Response {
    let (request, bytes) = match split_body(request, "email").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    let props = map_from_json(&bytes);
    let email = mm_model::utils::go_to_lower(props.get("email").map_or("", String::as_str));

    if email.is_empty() {
        tracing::Span::current().record("outcome", "invalid_param");
        return ApiError::invalid_param("email").into_response();
    }

    // `GetUserForLogin(c.AppContext, "", email)` — the **login** lookup, not `GetUserByEmail`, so
    // it honours `EnableSignInWithEmail`/`EnableSignInWithUsername` and will match a *username*
    // that happens to equal the submitted string. On a server with e-mail sign-in switched off it
    // fails for every address, and this route answers OK to everything.
    if state.app.get_user_for_login("", &email).await.is_err() {
        tracing::Span::current().record("outcome", "unmatched");
        return status_ok();
    }

    tracing::Span::current().record("forwarded", true);
    tracing::Span::current().record("outcome", "send");
    proxy::forward_to_go(State(state), request).await
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/password/reset/send
// ---------------------------------------------------------------------------------------------

/// Port of `sendPasswordReset` (api4/user.go:2094) over `App.SendPasswordReset`
/// (app/user.go:1911).
///
/// # The three refusals, and the one that is not an error
///
/// | condition | answer |
/// |---|---|
/// | `email` absent or empty | **400** `api.context.invalid_body_param.app_error` naming `email` |
/// | no account with that address | **200** `{"status":"OK"}` — `(false, nil)`, not an error |
/// | `user.IsRemote()` | **400** `api.user.send_password_reset.send.app_error` |
/// | `user.AuthData != nil && *user.AuthData != ""` | **400** `api.user.send_password_reset.sso.app_error` |
/// | `user.IsMagicLinkEnabled()` | **400** `api.user.send_password_reset.guest_magic_link.app_error` |
///
/// The order is the table's: a remote SSO user reports `send`, not `sso`. The `DetailedError` on
/// all three is `"userId=" + user.Id`, which `wipe_detailed` blanks before it leaves the process.
///
/// # `ExperimentalEnableHardenedMode` rewrites every one of those 400s into a 200
///
/// The handler catches the app error and, with the toggle on, calls `ReturnStatusOK` instead —
/// so on a hardened server the three refusals become indistinguishable from the unmatched-address
/// success. Note what it does **not** cover: the `email == ""` 400 is raised by the handler
/// before `SendPasswordReset` is called and stays a 400 either way.
///
/// Anything that gets past the refusals forwards: `CreatePasswordRecoveryToken` deletes this
/// user's existing recovery tokens and inserts a new one, and both of those are writes.
#[tracing::instrument(skip_all, fields(forwarded = false, hardened, outcome))]
pub async fn send_password_reset(State(state): State<AppState>, request: Request) -> Response {
    let (request, bytes) = match split_body(request, "email").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    let props = map_from_json(&bytes);
    let email = mm_model::utils::go_to_lower(props.get("email").map_or("", String::as_str));

    if email.is_empty() {
        tracing::Span::current().record("outcome", "invalid_param");
        return ApiError::invalid_param("email").into_response();
    }

    let Ok(user) = state.app.get_user_by_email(&email).await else {
        // `return false, nil` — no error, and the handler's `if sent` simply does not fire.
        tracing::Span::current().record("outcome", "unmatched");
        return status_ok();
    };

    let refusal = if user.is_remote() {
        Some("api.user.send_password_reset.send.app_error")
    } else if user
        .auth_data
        .as_deref()
        .is_some_and(|data| !data.is_empty())
    {
        Some("api.user.send_password_reset.sso.app_error")
    } else if user.is_magic_link_enabled() {
        Some("api.user.send_password_reset.guest_magic_link.app_error")
    } else {
        None
    };

    if let Some(id) = refusal {
        let hardened = state.app.config().experimental_enable_hardened_mode;
        tracing::Span::current().record("hardened", hardened);
        tracing::Span::current().record("outcome", id);
        if hardened {
            return status_ok();
        }
        return ApiError::from(AppError::boxed(
            "SendPasswordReset",
            id,
            None,
            format!("userId={}", user.id),
            400,
        ))
        .into_response();
    }

    tracing::Span::current().record("forwarded", true);
    tracing::Span::current().record("outcome", "send");
    proxy::forward_to_go(State(state), request).await
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/{user_id}/email/verify/member
// ---------------------------------------------------------------------------------------------

/// Port of `verifyUserEmailWithoutToken` (api4/user.go:3629).
///
/// # The lookup comes before the permission check
///
/// `GetUser` runs first, so a caller **without** `manage_system` asking about an id that names
/// nobody gets a **404**, not a 403 — the route tells an unprivileged caller whether an account
/// exists. That is Go's order, it is an information leak, and reversing it would be a silent
/// behaviour change rather than a fix. The 403 only appears for an id that resolves.
///
/// # The body is the user as it was *before* the write
///
/// `VerifyUserEmail` updates the row and then the handler encodes the copy it fetched earlier, so
/// the response never shows `email_verified: true` for the change it just made. It is moot in
/// practice — `SanitizeProfile` → `ClearNonProfileFields` sets `EmailVerified = false`
/// unconditionally — but it is why a reader should not "fix" this by re-reading the user.
///
/// # `SanitizeProfile(user, true)`, unconditionally
///
/// The `asAdmin` argument is the literal `true`, not `c.IsSystemAdmin()`. The caller has just
/// been proven to hold `manage_system`, so the two agree here; the literal is what Go writes.
///
/// The response is a **200** with no explicit `WriteHeader`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, actor = %session.0.user_id))]
pub async fn verify_user_email_without_token(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }

    let mut user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    // `user.Email`, not the request — there is no body on this route at all.
    if let Err(err) = state.app.verify_user_email(&user.id, &user.email).await {
        return ApiError::from(err).into_response();
    }

    state.app.sanitize_profile(&mut user, true);

    match serde_json::to_vec(&user) {
        Ok(body) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise User");
            ApiError::from(AppError::new(
                "verifyUserEmailWithoutToken",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;

    fn request(uri: &str) -> Request {
        HttpRequest::builder()
            .method("POST")
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("a request")
    }

    /// `r.URL.Query().Get` takes the **first** value for a repeated key and `""` for anything
    /// absent. Both matter: `?t=&t=real` must take the anonymous branch, because that is what Go
    /// does, and a port that scanned for "any non-empty value" would forward instead.
    #[test]
    fn query_get_matches_gos_first_value_rule() {
        assert_eq!(query_get(&request("/x"), "t"), "");
        assert_eq!(query_get(&request("/x?iid=abc"), "t"), "");
        assert_eq!(query_get(&request("/x?t=one&t=two"), "t"), "one");
        assert_eq!(query_get(&request("/x?t=&t=two"), "t"), "");
        assert_eq!(query_get(&request("/x?t"), "t"), "");
        // Percent-decoded, like `url.Query()`.
        assert_eq!(query_get(&request("/x?t=a%20b"), "t"), "a b");
        // A key that merely *starts with* the name must not match.
        assert_eq!(query_get(&request("/x?token=abc"), "t"), "");
        assert_eq!(query_get(&request("/x?iid=abc"), "iid"), "abc");
    }

    /// The map decoder swallows every failure, including an object with a non-string value —
    /// which is the shape Go's `encoding/json` partially decodes and serde does not. The
    /// consequence is that `{"email": 7}` is an empty map here *and* on Go (`MapFromJSON` returns
    /// the zero map on any error), so both answer the missing-`email` 400.
    #[test]
    fn a_mixed_object_decodes_to_an_empty_map() {
        assert!(map_from_json(br#"{"email": 7}"#).is_empty());
        assert!(map_from_json(b"not json").is_empty());
        assert!(map_from_json(b"").is_empty());
        assert_eq!(
            map_from_json(br#"{"email":"A@B.C"}"#)
                .get("email")
                .map(String::as_str),
            Some("A@B.C")
        );
    }

    /// The three shapes `decode_user` exists for. Each was a live divergence before it did.
    #[test]
    fn the_user_decoder_matches_gos_decoder_and_not_serdes() {
        // A sequence: serde's derived impl would take it, Go's would not.
        assert!(decode_user(b"[]").is_err());
        assert!(decode_user(br#"["a","b"]"#).is_err());
        // Scalars, likewise.
        for body in [&b"7"[..], b"\"x\"", b"true"] {
            assert!(
                decode_user(body).is_err(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        // `null` is the zero user and **not** an error.
        let zero = decode_user(b"null").expect("null is the zero user");
        assert_eq!(zero, User::default());
        assert_eq!(
            decode_user(b"{}").expect("an empty object"),
            User::default()
        );
        // Trailing bytes after the first value are ignored, as `Decoder.Decode` ignores them.
        let user = decode_user(br#"{"username":"bob"} trailing garbage"#)
            .expect("one value, then anything");
        assert_eq!(user.username, "bob");
        // A wrong-typed field is still an error on both.
        assert!(decode_user(br#"{"username":7}"#).is_err());
        // And the id is the body-param one naming `user`.
        let err = decode_user(b"[]").expect_err("a sequence");
        assert_eq!(err.0.id, "api.context.invalid_body_param.app_error");
        assert_eq!(
            err.0
                .params
                .as_ref()
                .and_then(|p| p.get("Name"))
                .and_then(serde_json::Value::as_str),
            Some("user")
        );
    }

    #[test]
    fn status_ok_has_no_trailing_newline() {
        // The literal is the assertion; `ReturnStatusOK` is a `w.Write`, and a client comparing
        // bytes across the two servers sees the newline an encoder would add.
        assert_eq!(r#"{"status":"OK"}"#.len(), 15);
    }
}
