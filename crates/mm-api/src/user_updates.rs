//! The user-update family (api4/user.go):
//!
//! ```text
//! PUT /api/v4/users/{user_id}          updateUser        (user.go:1482)
//! PUT /api/v4/users/{user_id}/patch    patchUser         (user.go:1572)
//! PUT /api/v4/users/{user_id}/active   updateUserActive  (user.go:1775)
//! PUT /api/v4/users/{user_id}/roles    updateUserRoles   (user.go:1726)
//! ```
//!
//! # `updateUser` takes a whole `model.User` and ignores most of it
//!
//! The body is decoded into a full user, and then: the id must equal the path segment (a
//! mismatch is a 400 naming `user_id`, *not* a silent reassignment — Go's comment says "The user
//! being updated in the payload must be the same one as indicated in the URL"), and thirteen
//! columns are copied back off the stored row inside `SqlUserStore.Update` before the statement
//! is built. `Roles` and `DeleteAt` join them because `App.UpdateUser` passes
//! `trustedUpdateData = false`. So a body claiming `"roles": "system_admin"` or
//! `"email_verified": true` is accepted, ignored, and answered with the stored values —
//! `the_body_cannot_grant_itself_roles` and its siblings in `tests/parity.rs` send exactly that.
//!
//! There is **no `SanitizeInput` call on this route**; `createUser` is the only api4 handler that
//! makes one. See the module doc on [`mm_app::user_update`].
//!
//! # What forwards, and why every forward precedes every write
//!
//! | route | served here | forwarded |
//! |---|---|---|
//! | `PUT /users/{id}` | everything on an unlicensed server whose target is not LDAP/SAML-locked | an LDAP or SAML target on a licensed server; a licensed profile-field lock that would actually refuse |
//! | `PUT /users/{id}/patch` | the same, plus: any patch that does not switch the auto-responder **on** | the same, plus a patch that switches the auto-responder on |
//! | `PUT /users/{id}/active` | `{"active": true}` | `{"active": false}`, and a licensed server |
//! | `PUT /users/{id}/roles` | everything on an unlicensed server | a licensed server when the new roles name a system-console role |
//!
//! The ordering property is load-bearing on the first two routes, because
//! `App.DoubleCheckPassword` — the current-password check on an email change — **writes**: it
//! claims a `FailedAttempts` slot before it compares anything. A forward taken after that would
//! leave a consumed login attempt behind for a request Go then handled from scratch, and three
//! of them would lock the account out of a password it typed correctly once. Every forward above
//! is decided from the body, a `SELECT`, or the licence, and all three come first.
//!
//! `updateUserActive`'s forward is the starker one: `active = false` continues into
//! `RevokeAllSessions` and `userDeactivated`, which delete the user's OAuth grants, disable the
//! bots they own and DM the system admins — all of it **after** the row is written, so there is
//! no prefix of it that can be served. The whole request goes to Go. See [D-461].

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_app::user_update::roles_need_custom_permissions_schemes;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_ROLES, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_USERS, make_permission_error,
};
use mm_model::user::{User, UserPatch, is_valid_user_roles};
use mm_model::utils::{AppError, StringMap};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;
use crate::proxy;
use crate::user_creates::{decode_go_struct, decode_user};

/// Read the whole body, keeping the parts so the request can still be forwarded.
async fn split_body(
    request: Request,
    parameter: &'static str,
) -> Result<(Request, Vec<u8>), ApiError> {
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

/// Port of `model.MapFromJSON` (utils.go:507) — `json.NewDecoder(r.Body).Decode(&map[string]string)`
/// with the error **discarded**, which is not the same thing as "an empty map on any problem".
///
/// Three behaviours the three existing one-line copies of this helper in the crate do not have,
/// and that `updateUserRoles` can actually be handed:
///
/// 1. **A partial decode survives.** `encoding/json` records the first `UnmarshalTypeError` and
///    keeps going, so `{"roles":"system_user","n":1}` leaves `roles` set. A
///    `from_slice::<BTreeMap<String,String>>().unwrap_or_default()` returns the *empty* map for
///    that body — and an empty map means `roles: ""`, which this route writes. The divergence is
///    not a 400 versus a 200; it is one user keeping their roles versus having them erased.
/// 2. **The offending key is still inserted, holding `""`.** Measured, not assumed: the fixture
///    row for `{"roles":1}` is `{"roles":""}`, not `{}`, and `{"roles":null}` is the same. Go's
///    decoder assigns the zero value and records the error rather than skipping the entry, so a
///    port that *dropped* the key would agree on every `.get("roles")` and disagree on
///    `len(props)` — and on any future reader of a second key.
/// 3. **Trailing bytes after the first value are ignored** (`Decoder.Decode`, not `Unmarshal`),
///    and a duplicate key is last-wins.
/// 4. A non-object — `null`, an array, a number, a malformed body — leaves the map nil, which Go
///    replaces with an empty one.
fn map_from_json(bytes: &[u8]) -> StringMap {
    use serde::Deserialize;

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    match serde_json::Value::deserialize(&mut deserializer) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .map(|(key, value)| match value {
                serde_json::Value::String(value) => (key, value),
                _ => (key, String::new()),
            })
            .collect(),
        _ => StringMap::new(),
    }
}

/// Port of `model.StringInterfaceFromJSON` (utils.go:590) — the same call into a
/// `map[string]any`, where every JSON value is assignable, so only a non-object top level
/// produces the empty map. Trailing bytes are ignored for the same reason as above.
fn string_interface_from_json(bytes: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    use serde::Deserialize;

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    match serde_json::Value::deserialize(&mut deserializer) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
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

/// `json.NewEncoder(w).Encode(ruser)` — a 200 and a **trailing newline** ([D-086]).
fn user_response(handler: &'static str, user: &User) -> Response {
    let mut body = match serde_json::to_vec(user) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise User");
            return ApiError::from(AppError::new(
                handler,
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

/// `model.NewAppError(handler, id, map[string]any{"Field": field}, "", 409)` — the two
/// profile-conflict errors. **409 Conflict**, not 400: a client that treats every 4xx on this
/// route as "bad input" will retry forever.
fn field_conflict(handler: &'static str, id: &'static str, field: &str) -> Response {
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    params.insert(
        "Field".to_owned(),
        serde_json::Value::String(field.to_owned()),
    );
    ApiError::from(AppError::new(handler, id, Some(params), String::new(), 409)).into_response()
}

/// The shared prologue of `updateUser` and `patchUser`: both resolve `me`, both gate on
/// `SessionHasPermissionToUserOrBot`, both load the stored user, and both refuse a non-admin
/// touching a system administrator.
///
/// **The two disagree about the order of the last two steps and about the error for a missing
/// user**, so neither is folded in here:
///
/// - `updateUser` loads with `c.Err = err`, keeping `GetUser`'s **404** for an unknown id.
/// - `patchUser` rewrites it to `SetInvalidParam("user_id")` — a **400** with a *body*-param id,
///   on a value that came from the URL.
///
/// Both then run the sysadmin check, but `updateUser` runs it before the audit prior-state and
/// `patchUser` after; that is not observable.
async fn require_edit_target(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: &str,
) -> Result<(), ApiError> {
    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }
    Ok(())
}

/// "Cannot update a system admin unless user making request is a systemadmin also."
async fn refuse_untrusted_admin_edit(
    state: &AppState,
    session: &AuthenticatedSession,
    target: &User,
) -> Result<(), ApiError> {
    if target.is_system_admin()
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }
    Ok(())
}

/// `c.SetPermissionError(EditOtherUsers)` followed by
/// `c.Err.DetailedError += ", attempted email update by oauth app"` (user.go:1526).
///
/// The suffix is appended to a `DetailedError` that `SetPermissionError` has already filled in
/// with the permission id, and `detailed_error` **is** on the wire — so the concatenation is
/// observable and the comma-space is part of it.
fn oauth_email_refusal(session: &AuthenticatedSession) -> Response {
    let mut err = make_permission_error(&session.0, &[&PERMISSION_EDIT_OTHER_USERS]);
    err.detailed_error
        .push_str(", attempted email update by oauth app");
    ApiError::from(err).into_response()
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}
// ---------------------------------------------------------------------------------------------

/// Port of `updateUser` (api4/user.go:1482).
///
/// # The password check here is not the one on `/patch`
///
/// `user.Email != "" && ouser.Email != user.Email && session.UserId == c.Params.UserId` — so it
/// fires only when the session owner changes *their own* address, and an admin changing somebody
/// else's needs no password at all. `DoubleCheckPassword`'s failure is flattened to
/// `SetInvalidParam("password")`, a 400, which swallows the 401 a locked-out account would
/// otherwise get. `/patch` propagates that error instead; the two routes answer differently for
/// the same wrong password.
///
/// Note the first conjunct: an **empty** `email` in the body skips the check entirely — and then
/// reaches `App.UpdateUser` with `user.Email == ""`, where `IsValid` refuses it. So the route
/// cannot be used to blank an address, but it gets there by validation rather than by this gate.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id))]
pub async fn update_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    tracing::Span::current().record("user_id", &user_id);

    let (request, bytes) = match split_body(request, "user").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    let user = match decode_user(&bytes) {
        Ok(user) => user,
        Err(err) => return err.into_response(),
    };

    // "The user being updated in the payload must be the same one as indicated in the URL."
    if user.id != user_id {
        return ApiError::invalid_param("user_id").into_response();
    }

    if let Err(err) = require_edit_target(&state, &session, &user_id).await {
        return err.into_response();
    }
    let target = match state.app.get_user(&user_id).await {
        Ok(target) => target,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Err(err) = refuse_untrusted_admin_edit(&state, &session, &target).await {
        return err.into_response();
    }

    if session.0.is_oauth && target.email != user.email {
        return oauth_email_refusal(&session);
    }

    let patch = user.to_patch();
    match state.app.check_provider_attributes(&target, &patch).await {
        Ok(Some(field)) => {
            return field_conflict(
                "updateUser",
                "api.user.update_user.login_provider_attribute_set.app_error",
                field,
            );
        }
        Ok(None) => {}
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason, "forwarding updateUser");
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
    }

    match state
        .app
        .check_locked_profile_fields(&session.0, &target, &patch)
        .await
    {
        Ok(Some(field)) => {
            return field_conflict(
                "updateUser",
                "api.user.update_user.profile_field_locked.app_error",
                field,
            );
        }
        Ok(None) => {}
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason, "forwarding updateUser");
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
    }

    if !user.email.is_empty() && target.email != user.email && session.0.user_id == user_id {
        // Go passes `user.Password` — the *body's* password field, which `model.User` carries and
        // nothing else on this route reads. Every failure, including the account lockout, becomes
        // the same 400.
        if state
            .app
            .double_check_password(&target, &user.password)
            .await
            .is_err()
        {
            return ApiError::invalid_param("password").into_response();
        }
    }

    match state.app.update_user_as_user(&user).await {
        Ok(ruser) => user_response("updateUser", &ruser),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/patch
// ---------------------------------------------------------------------------------------------

/// Port of `patchUser` (api4/user.go:1572).
///
/// # `patch.RemoteId = nil`, unconditionally and before anything else
///
/// The field is on `model.UserPatch` with a plain `json:"remote_id"` tag, so a client can send it
/// — and Go discards it on the second line of the handler, before the permission check and before
/// the audit record is even built. `User.Patch` would otherwise write it, and `RemoteId` is what
/// marks a row as owned by a remote cluster. There is no admin bypass: the assignment is not
/// inside a branch.
///
/// # `GetUser`'s failure becomes a 400 naming a *body* parameter
///
/// `c.SetInvalidParam("user_id")` on a value that came from the URL — so an unknown but
/// well-formed id answers `api.context.invalid_body_param.app_error`/400 here and
/// `app.user.missing_account.const`/404 on `PUT /users/{id}`. Same id, same session, two routes,
/// two answers.
///
/// # The auto-responder forward
///
/// `SetAutoResponderStatus` runs *after* the write and its off→on arm needs
/// `SetStatusOutOfOffice`, which is not ported. The transition is computed here from the stored
/// props and the patch — `User.Patch` replaces the whole `NotifyProps` map when the patch carries
/// one — so the forward happens before `PatchUser` writes anything.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id))]
pub async fn patch_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    tracing::Span::current().record("user_id", &user_id);

    let (request, bytes) = match split_body(request, "user").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    // The decode error names `user`, not `user_patch` — `c.SetInvalidParamWithErr("user", …)`.
    let mut patch: UserPatch = match decode_go_struct(&bytes, "user") {
        Ok(patch) => patch,
        Err(err) => return err.into_response(),
    };
    patch.remote_id = None;

    if let Err(err) = require_edit_target(&state, &session, &user_id).await {
        return err.into_response();
    }
    // `GetUser`'s error is discarded and replaced wholesale — *every* failure, not only the 404,
    // so a broken query answers 400 here where `updateUser` answers 500.
    let target = match state.app.get_user(&user_id).await {
        Ok(target) => target,
        Err(_) => return ApiError::invalid_param("user_id").into_response(),
    };
    if let Err(err) = refuse_untrusted_admin_edit(&state, &session, &target).await {
        return err.into_response();
    }

    if session.0.is_oauth
        && let Some(email) = patch.email.as_ref()
        && &target.email != email
    {
        return oauth_email_refusal(&session);
    }

    match state.app.check_provider_attributes(&target, &patch).await {
        Ok(Some(field)) => {
            return field_conflict(
                "patchUser",
                "api.user.patch_user.login_provider_attribute_set.app_error",
                field,
            );
        }
        Ok(None) => {}
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason, "forwarding patchUser");
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
    }

    match state
        .app
        .check_locked_profile_fields(&session.0, &target, &patch)
        .await
    {
        Ok(Some(field)) => {
            return field_conflict(
                "patchUser",
                "api.user.patch_user.profile_field_locked.app_error",
                field,
            );
        }
        Ok(None) => {}
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason, "forwarding patchUser");
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
    }

    // The auto-responder transition, decided before the write. `User.Patch` swaps the whole map,
    // so the resulting props are the patch's when it carries one and the stored user's otherwise.
    if mm_app::user_update::auto_responder_turns_on(
        patch.notify_props.as_ref().or(target.notify_props.as_ref()),
        target.notify_props.as_ref(),
    ) {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!("forwarding patchUser: SetStatusOutOfOffice is not ported");
        return proxy::forward_to_go(State(state), request).await;
    }

    if let Some(email) = patch.email.as_ref()
        && &target.email != email
        && session.0.user_id == user_id
    {
        // Two branches where `updateUser` has one, and the second keeps the real error: a wrong
        // password is `api.user.check_user_password.invalid.app_error`/401 here.
        let Some(password) = patch.password.as_ref() else {
            return ApiError::invalid_param("password").into_response();
        };
        if let Err(err) = state.app.double_check_password(&target, password).await {
            return ApiError::from(err).into_response();
        }
    }

    let ruser = match state.app.patch_user(&user_id, &patch).await {
        Ok(ruser) => ruser,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if let Err(err) = state
        .app
        .set_auto_responder_status(&ruser, target.notify_props.as_ref())
        .await
    {
        // Unreachable: the same transition was refused above, before the write. Logged rather
        // than turned into a 500, because the row is already committed and Go's own version of
        // this call returns nothing at all.
        tracing::error!(error = %err, "the auto-responder transition was not applied");
    }

    user_response("patchUser", &ruser)
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/active
// ---------------------------------------------------------------------------------------------

/// Port of `updateUserActive` (api4/user.go:1775), **activation only**.
///
/// # `props["active"].(bool)` is a type assertion, not a parse
///
/// The body is decoded into `map[string]any`, so `{"active": true}` passes and
/// `{"active": "true"}`, `{"active": 1}` and a missing key all fail the assertion and answer 400
/// naming `active`. A port that accepted the string would activate accounts from bodies Go
/// refuses.
///
/// # Five gates, in this order, and two of them are 401s
///
/// `sysconsole_write_user_management_users` (403, `api.user.update_active.permissions.app_error`,
/// with `userId=<id>` as the detail) → the self-deactivation toggle (401) → the sysadmin guard
/// (403) → the bot-manage check → guest accounts disabled (**401**, not 403) → LDAP (403). The
/// two 401s on a route whose other refusals are 403 are Go's, and a client that retries on 401 by
/// re-authenticating will loop.
///
/// # The event is broadcast to everybody, with no payload
///
/// `NewWebSocketEvent(user_activation_status_change, "", "", "", nil, "")` — no team, no channel,
/// no user id, no omitted users and no data. Every connected client is told that *somebody's*
/// activation changed and has to refetch to learn who.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id, active))]
pub async fn update_user_active(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    tracing::Span::current().record("user_id", &user_id);

    let (request, bytes) = match split_body(request, "active").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    let props = string_interface_from_json(&bytes);
    let Some(active) = props.get("active").and_then(serde_json::Value::as_bool) else {
        return ApiError::invalid_param("active").into_response();
    };
    tracing::Span::current().record("active", active);

    // Deactivation's whole tail runs after the write — see [D-461] and the module doc.
    if !active {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }

    // The licensed seat-limit message, `CreateGuest` and the licensed activation warning all read
    // the licence, which is not visible here.
    match state.app.license_state().await {
        Ok(mm_app::license::LicenseState::Licensed) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(_) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }

    // `isSelfDeactivate` is `!active && …`, so on this path it is false by construction and the
    // permission is always required — including from the account's own owner reactivating itself.
    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_USERS,
        )
        .await
    {
        return ApiError::from(AppError::new(
            "updateUserActive",
            "api.user.update_active.permissions.app_error",
            None,
            format!("userId={user_id}"),
            403,
        ))
        .into_response();
    }

    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if let Err(err) = refuse_untrusted_admin_edit(&state, &session, &user).await {
        return err.into_response();
    }

    if user.is_bot
        && let Err(err) = state
            .app
            .session_has_permission_to_manage_bot(&session.0, &user_id)
            .await
    {
        return ApiError::from(err).into_response();
    }

    if user.is_guest() && !state.app.config().guest_accounts_enable {
        return ApiError::from(AppError::new(
            "updateUserActive",
            "api.user.update_active.cannot_enable_guest_when_guest_feature_is_disabled.app_error",
            None,
            format!("userId={user_id}"),
            401,
        ))
        .into_response();
    }

    // `user.AuthService == model.UserAuthServiceLdap` — `IsLDAPUser` is that comparison and
    // nothing else (model/user.go:579), so this is the Go predicate and not a widening of it.
    if user.is_ldap_user() {
        return ApiError::from(AppError::new(
            "updateUserActive",
            "api.user.update_active.cannot_modify_status_when_user_is_managed_by_ldap.app_error",
            None,
            format!("userId={user_id}"),
            403,
        ))
        .into_response();
    }

    if let Err(err) = state.app.activate_user(&user).await {
        return ApiError::from(err).into_response();
    }

    let message = mm_model::websocket_message::WebSocketEvent::new(
        mm_model::websocket_message::WEBSOCKET_EVENT_USER_ACTIVATION_STATUS_CHANGE,
        "",
        "",
        "",
        None,
        "",
    );
    state.app.publish(message).await;

    status_ok()
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/roles
// ---------------------------------------------------------------------------------------------

/// Port of `updateUserRoles` (api4/user.go:1726).
///
/// # The licence check runs **before** the permission check
///
/// So a caller without `manage_roles` who names `system_manager` on an unlicensed server gets the
/// licence 400, not a 403 — the route tells an unprivileged caller something about the licence.
/// Reordering the two is invisible to every happy-path test and changes that answer.
///
/// # `IsValidUserRoles("")` is true
///
/// `strings.Fields` of an empty string is an empty slice, the validity loop never runs, and the
/// `len(roles) == 1 && roles[0] == "system_admin"` guard does not fire. So `{}` — or any body
/// with no `roles` key — passes validation and **erases the user's roles**. That is Go's
/// behaviour and it is why [`map_from_json`] above reproduces `encoding/json`'s partial decode
/// rather than collapsing a mistyped field to the same empty map.
///
/// # The response is `{"status":"OK"}`, not the user
///
/// `ReturnStatusOK(w)` — the updated user is built, audited and discarded. A client that wants
/// the new roles has to refetch.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id, roles))]
pub async fn update_user_roles(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    tracing::Span::current().record("user_id", &user_id);

    let (request, bytes) = match split_body(request, "roles").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    let props = map_from_json(&bytes);
    let new_roles = props.get("roles").map_or("", String::as_str).to_owned();
    tracing::Span::current().record("roles", &new_roles);

    if !is_valid_user_roles(&new_roles) {
        return ApiError::invalid_param("roles").into_response();
    }

    if roles_need_custom_permissions_schemes(&new_roles) {
        match state.app.license_state().await {
            // `license == nil` — Go's first disjunct, so the 400 is reached without ever reading
            // `Features.CustomPermissionsSchemes`.
            Ok(mm_app::license::LicenseState::Unlicensed) => {
                return ApiError::from(AppError::new(
                    "updateUserRoles",
                    "api.user.update_user_roles.license.app_error",
                    None,
                    String::new(),
                    400,
                ))
                .into_response();
            }
            Ok(mm_app::license::LicenseState::Licensed) => {
                tracing::Span::current().record("forwarded", true);
                return proxy::forward_to_go(State(state), request).await;
            }
            Err(err) => return ApiError::from(err).into_response(),
        }
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_ROLES)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_ROLES],
        ))
        .into_response();
    }

    match state
        .app
        .update_user_roles(&user_id, &new_roles, true)
        .await
    {
        Ok(_) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_user_update.json"))
            .expect("the behaviour fixture parses")
    }

    fn cases(key: &str) -> Vec<serde_json::Value> {
        oracle()[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} is an array"))
            .clone()
    }

    /// `model.MapFromJSON` over the whole corpus, including the four shapes a
    /// `from_slice().unwrap_or_default()` gets wrong.
    #[test]
    fn map_from_json_matches_go() {
        let rows = cases("map_decode");
        assert!(rows.len() >= 30, "the corpus is populated");
        for row in rows {
            let body = row["in"].as_str().expect("a body");
            let ours = map_from_json(body.as_bytes());
            let theirs: StringMap =
                serde_json::from_value(row["map_from_json"].clone()).expect("a string map");
            assert_eq!(ours, theirs, "MapFromJSON({body:?})");

            // And the value this route actually reads off it, which is the thing that gets
            // written to the `Roles` column.
            assert_eq!(
                ours.get("roles").map_or("", String::as_str),
                row["roles"].as_str().expect("a string"),
                "props[\"roles\"] for {body:?}"
            );
        }
    }

    /// The rows that separate Go's partial decode from the collapse-to-empty a reader reaches
    /// for. Pinned by name because the assertion above passes vacuously if they leave the corpus.
    #[test]
    fn the_map_corpus_contains_the_partial_decode_rows() {
        let bodies: Vec<String> = cases("map_decode")
            .iter()
            .map(|row| row["in"].as_str().expect("a body").to_owned())
            .collect();
        for body in [
            r#"{"roles":"system_user","n":1}"#,
            r#"{"n":1,"roles":"system_user"}"#,
            r#"{"roles":"system_user"} trailing"#,
            r#"{"roles":"system_user","roles":"system_admin"}"#,
        ] {
            assert!(
                bodies.iter().any(|b| b == body),
                "the corpus still has {body:?}"
            );
        }

        // The partial-decode body must still yield the roles, and a port that dropped the whole
        // object would erase them instead.
        let props = map_from_json(br#"{"roles":"system_user","n":1}"#);
        assert_eq!(props.get("roles").map(String::as_str), Some("system_user"));
        // ...and the offending key is present holding the empty string, not absent.
        assert_eq!(props.get("n").map(String::as_str), Some(""));
    }

    /// `model.StringInterfaceFromJSON` plus the **type assertion** `updateUserActive` makes on
    /// it. `{"active":"true"}` and `{"active":1}` are 400s in Go, not activations.
    #[test]
    fn the_active_type_assertion_matches_go() {
        for row in cases("map_decode") {
            let body = row["in"].as_str().expect("a body");
            let props = string_interface_from_json(body.as_bytes());
            let got = props.get("active").and_then(serde_json::Value::as_bool);

            let expected = if row["active_ok"].as_bool().expect("a bool") {
                Some(row["active"].as_bool().expect("a bool"))
            } else {
                None
            };
            assert_eq!(got, expected, "props[\"active\"].(bool) for {body:?}");

            let keys: Vec<String> = props.keys().cloned().collect();
            let theirs: Vec<String> = row["string_interface_keys"]
                .as_array()
                .expect("an array")
                .iter()
                .map(|k| k.as_str().expect("a string").to_owned())
                .collect();
            assert_eq!(keys, theirs, "StringInterfaceFromJSON keys for {body:?}");
        }
    }

    /// `json.NewDecoder(r.Body).Decode(&model.UserPatch)`, through the shared
    /// [`decode_go_struct`].
    ///
    /// # One row is a known divergence and is asserted as one
    ///
    /// `{"USERNAME":"folded"}` sets `Username` in Go — `encoding/json` matches field names
    /// case-insensitively ([D-040]) — and leaves it unset here, because no
    /// `go_json::GoFields` schema exists for `UserPatch` or `User`. The test asserts the
    /// *divergence* rather than skipping the row, so closing [D-460] will fail it and the
    /// assertion has to be flipped deliberately.
    #[test]
    fn user_patch_decoding_matches_go() {
        let rows = cases("patch_decode");
        assert!(rows.len() >= 17, "the corpus is populated");
        let mut divergent = 0;
        for row in rows {
            let body = row["in"].as_str().expect("a body");
            let ours: Result<UserPatch, ApiError> = decode_go_struct(body.as_bytes(), "user");

            if row["failed"].as_bool().expect("a bool") {
                assert!(ours.is_err(), "Go refuses {body:?} and so must we");
                continue;
            }
            let ours = ours.unwrap_or_else(|_| panic!("Go accepts {body:?}"));
            let theirs: UserPatch =
                serde_json::from_value(row["patch"].clone()).expect("the patch deserialises");

            if body == r#"{"USERNAME":"folded"}"# {
                assert_eq!(
                    theirs.username.as_deref(),
                    Some("folded"),
                    "Go folds the key"
                );
                assert_eq!(ours.username, None, "we do not — [D-460]");
                divergent += 1;
                continue;
            }

            assert_eq!(ours, theirs, "UserPatch decode of {body:?}");
        }
        assert_eq!(
            divergent, 1,
            "exactly one divergent row, and it is the folded key"
        );
    }

    /// A JSON **array** decodes into a `#[serde(default)]` struct and not into a Go one. Pinned
    /// separately from the corpus loop because it is the bug the `createUser` session found, and
    /// it reappears for every struct body this crate decodes.
    #[test]
    fn a_json_array_is_not_a_user_patch() {
        assert!(decode_go_struct::<UserPatch>(b"[]", "user").is_err());
        assert!(decode_go_struct::<UserPatch>(br#"["username"]"#, "user").is_err());
        // `null` is not an error in Go: `Decode` into a non-pointer struct leaves it untouched.
        let patch = decode_go_struct::<UserPatch>(b"null", "user").expect("null is the zero patch");
        assert_eq!(patch, UserPatch::default());
    }

    /// `patch.RemoteId = nil` is unconditional, and the field is on the wire — so the only thing
    /// stopping a client from claiming a row for a remote cluster is that one assignment.
    #[test]
    fn the_remote_id_is_decoded_and_then_discarded() {
        let mut patch: UserPatch =
            decode_go_struct(br#"{"remote_id":"abcdefghijklmnopqrstuvwxyz"}"#, "user")
                .expect("it decodes");
        assert_eq!(
            patch.remote_id.as_deref(),
            Some("abcdefghijklmnopqrstuvwxyz"),
            "the decoder does accept it"
        );
        patch.remote_id = None;
        assert_eq!(patch, UserPatch::default(), "and the handler drops it");
    }

    /// `User.ToPatch` is what both conflict scans are driven from, and it **omits `RemoteId`** —
    /// so no locked-field or provider-attribute check can ever fire on a remote-id change
    /// arriving through `PUT /users/{id}`.
    #[test]
    fn to_patch_matches_go() {
        let case = &oracle()["to_patch"];
        let user: mm_model::user::User =
            serde_json::from_value(case["user"].clone()).expect("the user deserialises");
        let theirs: UserPatch =
            serde_json::from_value(case["patch"].clone()).expect("the patch deserialises");
        assert_eq!(user.to_patch(), theirs);
        assert_eq!(theirs.remote_id, None, "ToPatch drops the remote id");
    }
}
