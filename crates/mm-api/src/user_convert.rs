//! Account conversion and the guest pair — `convertUserToBot` (api4/user.go:3665),
//! `convertBotToUser` (api4/bot.go:274), `promoteGuestToUser` (api4/user.go:3499) and
//! `demoteUserToGuest` (api4/user.go:3540).
//!
//! | route | served here | forwarded |
//! |---|---|---|
//! | `POST …/users/{id}/convert_to_bot` | the 400, the 404, the 403, the already-a-bot 500, the conversion | an account with an `AuthService` |
//! | `POST …/bots/{id}/convert_to_user` | all of it | — |
//! | `POST …/users/{id}/promote` | the 400, the 403, the 404, both 501s, the promotion | — |
//! | `POST …/users/{id}/demote` | the 400 and the unlicensed 501 | a licensed server |
//!
//! # The four gate orders are all different, and each one is measurable
//!
//! `convertUserToBot` fetches the user **before** checking `manage_system`, so a caller with no
//! permission asking about an id that does not exist gets **404**. `promoteGuestToUser` checks the
//! permission **first**, so the same pair of facts gets **403**. `demoteUserToGuest` checks the
//! *licence* before both, so on an unlicensed server every demote is 501 whatever the caller may
//! do and whoever the target is — including a target that does not exist, measured.
//! `convertBotToUser` reads the bot, then the body, then the permission: a malformed body from a
//! caller with no `manage_system` is **400**, not 403.
//!
//! Swapping any adjacent pair is invisible to a test that sends only one wrong thing at a time,
//! so the suite sends two.
//!
//! # `w.Write` versus `json.NewEncoder`, in the same family
//!
//! `convertUserToBot` marshals to a byte slice and writes it — **no trailing newline**.
//! `convertBotToUser` encodes through a `json.Encoder` — **one trailing newline**. Both measured
//! byte-for-byte against the stack; they are two lines apart in the Go tree and differ.
//!
//! # `convert_to_bot` has no self-guard and no already-a-bot guard
//!
//! An administrator may convert their own account (`RequireUserId` even resolves `me`), and the
//! session revoke that follows logs them out. Converting an account that is already a bot is a
//! primary-key violation surfacing as **500 `app.bot.createbot.internal_error`** — measured, not
//! a 400. Neither is defended against here because neither is defended against in Go.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_PROMOTE_GUEST, make_permission_error,
};
use mm_model::user::UserPatch;
use mm_model::utils::{AppError, decode_one_from_json, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{LicenceGate, ME, licence_gate, query_flag_is_true, resolve_me};
use crate::error::ApiError;

/// `web.ReturnStatusOK` (web/web.go:127) — `w.Write(MapToJSON(...))`, so no trailing newline.
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

/// Port of `convertUserToBot` (api4/user.go:3665) —
/// `POST /api/v4/users/{user_id}/convert_to_bot`.
///
/// # Why an account with an `AuthService` is handed over
///
/// `App.ConvertUserToBot`'s first act is `UpdateUserAuth`, which blanks `Password`,
/// `LastPasswordUpdate`, `FailedAttempts`, `AuthService` and `AuthData` through
/// `UserStore.UpdateAuthData` — a store function this tree does not have and whose owner is the
/// authentication-data work, not this one. The fact that decides it is on the row the handler has
/// already read, and the forward happens before the `Bots` insert, so a forwarded request is one
/// this server has written nothing for. See [D-510].
///
/// The refusals above it are all served, including the 403, so a forwarded request is always one
/// Go would have accepted.
#[tracing::instrument(skip_all, fields(user_id = %user_id, federated))]
pub async fn convert_user_to_bot(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = resolve_me(&user_id, &session).to_owned();
    if !is_valid_id(&user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    // `GetUser` first: the 404 precedes the 403, which is the opposite of `promoteGuestToUser`
    // one screen away in the same file.
    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let federated = !user.auth_service.is_empty();
    tracing::Span::current().record("federated", federated);
    if federated {
        return crate::proxy::forward_to_go(State(state), request).await;
    }

    match state.app.convert_user_to_bot(&user).await {
        // `js, _ := json.Marshal(bot)` then `w.Write(js)` — no newline, and no explicit
        // `Content-Type` either, so Go's sniffer settles on `text/plain; charset=utf-8`. That
        // header is [D-030]'s subject and is written as JSON here, as everywhere else in this
        // server.
        Ok(bot) => match serde_json::to_vec(&bot) {
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
                tracing::error!(error = %err, "failed to serialise the converted bot");
                ApiError::from(AppError::new(
                    "convertUserToBot",
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

/// Port of `convertBotToUser` (api4/bot.go:274) —
/// `POST /api/v4/bots/{bot_user_id}/convert_to_user`.
///
/// # The body is one 400 with four ways in
///
/// `jsonErr != nil || userPatch.Password == nil || *userPatch.Password == ""` — a body that will
/// not decode, no body at all, `{}`, and `{"password":""}` are **indistinguishable** on the wire:
/// all four are `api.context.invalid_body_param.app_error` naming `userPatch`. Measured, all
/// four. A password that decodes but is too short passes this gate and fails much later, inside
/// `UpdatePassword`, as `model.user.is_valid.pwd_min_length.app_error` — by which point the
/// patch has already been written.
///
/// # `set_system_admin` is a *query* parameter and a parse failure is `false`
///
/// `strconv.ParseBool(r.URL.Query().Get("set_system_admin"))` with the error discarded, so
/// `?set_system_admin=yes` is false and an absent parameter is false. `1`, `t`, `T`, `TRUE`,
/// `true` and `True` are the true set.
///
/// # `RequireBotUserId` does not resolve `me`
///
/// Unlike every `{user_id}` route, `/bots/me/convert_to_user` is a 400 — the same asymmetry
/// `assignBot` records.
#[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, set_system_admin))]
pub async fn convert_bot_to_user(
    State(state): State<AppState>,
    Path(bot_user_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&bot_user_id) {
        return ApiError::invalid_url_param("bot_user_id").into_response();
    }

    // `includeDeleted = false`: a disabled bot is a 404 here, so it cannot be converted back
    // without being enabled first.
    let bot = match state.app.get_bot(&bot_user_id, false).await {
        Ok(bot) => bot,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("userPatch").into_response();
        }
    };
    let patch: UserPatch = match decode_one_from_json(&bytes) {
        Ok(patch) => patch,
        Err(_) => return ApiError::invalid_param("userPatch").into_response(),
    };
    if patch.password.as_deref().unwrap_or_default().is_empty() {
        return ApiError::invalid_param("userPatch").into_response();
    }

    let set_system_admin = query_flag_is_true(query.as_deref(), "set_system_admin");
    tracing::Span::current().record("set_system_admin", set_system_admin);

    // Third, not first: the two 400s above are reachable by a caller who may not convert anything.
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    match state
        .app
        .convert_bot_to_user(&bot, &patch, set_system_admin, Some(&session.0))
        .await
    {
        // `json.NewEncoder(w).Encode(user)` — trailing newline, unlike its sibling above.
        Ok(user) => match serde_json::to_vec(&user) {
            Ok(mut body) => {
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
            Err(err) => {
                tracing::error!(error = %err, "failed to serialise the converted user");
                ApiError::from(AppError::new(
                    "convertBotToUser",
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

/// Port of `promoteGuestToUser` (api4/user.go:3499) — `POST /api/v4/users/{user_id}/promote`.
///
/// # No licence gate and no config gate — this half of the pair is free
///
/// Its twin refuses everything without a licence *and* without `GuestAccountsSettings.Enable`.
/// `promote` checks neither: any server can turn a guest back into a user, which is what stops a
/// licence lapsing from stranding the guests it created. Measured — a promote on this unlicensed
/// stack reaches `GetUser` and answers 404 for a missing id, where a demote answers 501.
///
/// # Both refusals past the permission are **501**, not 400
///
/// `api.user.promote_guest_to_user.no_guest.app_error` for an account that is not a guest, and
/// `…magic_link_enabled.app_error` for one signing in by magic link. 501 for "you asked for the
/// wrong kind of account" is unusual and is Go's.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn promote_guest_to_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let user_id = resolve_me(&user_id, &session).to_owned();
    if !is_valid_id(&user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    // The permission precedes the fetch here, so a caller without it learns nothing about whether
    // the id exists.
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_PROMOTE_GUEST)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_PROMOTE_GUEST],
        ))
        .into_response();
    }

    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if !user.is_guest() {
        return not_implemented(
            "Api4.promoteGuestToUser",
            "api.user.promote_guest_to_user.no_guest.app_error",
        )
        .into_response();
    }

    if user.is_magic_link_enabled() {
        return not_implemented(
            "Api4.promoteGuestToUser",
            "api.user.promote_guest_to_user.magic_link_enabled.app_error",
        )
        .into_response();
    }

    // `c.AppContext.Session().UserId` — the **caller**, which becomes the requestor recorded
    // against every default channel the promoted account is added to.
    match state
        .app
        .promote_guest_to_user(&user, &session.0.user_id)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `demoteUserToGuest` (api4/user.go:3540) — `POST /api/v4/users/{user_id}/demote`.
///
/// # The licence is checked before anything else, so on this deployment that *is* the route
///
/// `RequireUserId`, then `License() == nil` → **501 `api.team.demote_user_to_guest.license.error`**.
/// Not the permission, not the user, not the config: a demote of an id that does not exist by a
/// caller with no permissions is 501 on an unlicensed server, measured. Everything past it — the
/// `GuestAccountsSettings.Enable` 501, the `Features.GuestAccounts` **403** (a different status
/// for what reads like the same refusal, and the only 403 in the handler that is not a permission
/// error), the `demote_to_guest` permission, the `manage_system` escalation guard for demoting an
/// administrator, the already-a-guest 501, and `DemoteUserToGuest` itself — needs a licence to
/// reach and is forwarded whole.
///
/// A licensed server is therefore handed the request **before any write**: the first thing past
/// the gate is a config read. See [D-511] for the body, which cannot be compared against Go on an
/// unlicensed stack at all — planting `Systems.ActiveLicenseId` moves this side and leaves Go
/// where it was.
#[tracing::instrument(skip_all, fields(user_id = %user_id, licensed))]
pub async fn demote_user_to_guest(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `RequireUserId` resolves `me` before validating, so `/users/me/demote` is a 501 rather than
    // a 400 — the id it checks is the session's.
    let user_id = if user_id == ME {
        _session.0.user_id.as_str()
    } else {
        user_id.as_str()
    };
    if !is_valid_id(user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => not_implemented(
            "Api4.demoteUserToGuest",
            "api.team.demote_user_to_guest.license.error",
        )
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// The three `http.StatusNotImplemented` refusals in this file, which differ only in `where` and
/// the error id.
fn not_implemented(where_: &'static str, id: &'static str) -> ApiError {
    ApiError::from(AppError::new(where_, id, None, String::new(), 501))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `strconv.ParseBool`'s accept set, which is what `set_system_admin` is read with. Anything
    /// outside it — including `yes` and `on` — is the zero value, because Go discards the error.
    #[test]
    fn set_system_admin_takes_go_s_bool_set_and_nothing_else() {
        for truthy in ["1", "t", "T", "TRUE", "true", "True"] {
            assert!(
                query_flag_is_true(
                    Some(&format!("set_system_admin={truthy}")),
                    "set_system_admin"
                ),
                "{truthy} is true for ParseBool"
            );
        }
        for falsy in ["0", "f", "FALSE", "false", "yes", "on", "", "TrUe"] {
            assert!(
                !query_flag_is_true(
                    Some(&format!("set_system_admin={falsy}")),
                    "set_system_admin"
                ),
                "{falsy} is not ParseBool-true"
            );
        }
        assert!(!query_flag_is_true(None, "set_system_admin"));
        assert!(!query_flag_is_true(Some("other=true"), "set_system_admin"));
    }

    /// The four bodies that reach the same 400, kept as a list because the predicate that
    /// produces it is a three-way `||` a reader could easily narrow.
    #[test]
    fn the_body_gate_refuses_a_missing_and_an_empty_password_alike() {
        let refused: Vec<Option<UserPatch>> = vec![
            decode_one_from_json(b"").ok(),
            decode_one_from_json(b"not json").ok(),
            decode_one_from_json(b"{}").ok(),
            decode_one_from_json(br#"{"password":""}"#).ok(),
        ];
        for (index, patch) in refused.iter().enumerate() {
            let has_password = patch
                .as_ref()
                .and_then(|p| p.password.as_deref())
                .is_some_and(|p| !p.is_empty());
            assert!(!has_password, "body {index} must not pass the gate");
        }

        let accepted: UserPatch =
            decode_one_from_json(br#"{"password":"a"}"#).expect("a password decodes");
        assert_eq!(accepted.password.as_deref(), Some("a"));
    }
}
