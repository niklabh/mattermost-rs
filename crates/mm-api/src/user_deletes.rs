//! `DELETE /api/v4/users/{user_id}` — `deleteUser` (api4/user.go:1663).
//!
//! One route, two entirely different operations chosen by a query string:
//!
//! ```text
//! DELETE /api/v4/users/{id}                   App.UpdateActive(user, false)   — a soft delete
//! DELETE /api/v4/users/{id}?permanent=true    App.PermanentDeleteUser(user)   — eighteen tables
//! ```
//!
//! # `permanent` is `strconv.ParseBool` with the error thrown away
//!
//! `params.Permanent, _ = strconv.ParseBool(query.Get("permanent"))` (web/params.go:232). So
//! `?permanent=1`, `?permanent=T` and `?permanent=True` are all true, and `?permanent=yes`,
//! `?permanent=`, a repeated key's *first* value and an absent key are all **false** — silently.
//! A client that means to erase an account and misspells the value soft-deletes it instead and
//! gets the same `{"status":"OK"}`. [`parse_go_bool`] is that function and nothing wider.
//!
//! # The gates, in Go's order
//!
//! 1. `RequireUserId` — `me` resolves to the session owner, then the 26-character check.
//! 2. `SessionHasPermissionToUserOrBot` → 403 naming `edit_other_users`.
//! 3. **self-delete**: `id == session.UserId && !TeamSettings.EnableUserDeactivation &&
//!    !manage_system` → 401 `api.user.update_active.not_enable.app_error`. Note the third
//!    conjunct: a system admin may delete their own account on a server where the flag is off.
//!    `updateUserActive`'s version of the same guard has **no** such escape (api4/user.go:1918),
//!    so the two routes disagree about a system admin deactivating themselves.
//! 4. `GetUser`.
//! 5. target is a system admin and caller lacks `manage_system` → 403 naming `manage_system`.
//! 6. the `permanent` fork.
//!
//! # Three gates `updateUserActive` has and this route does not
//!
//! `deleteUser` never checks the LDAP lock, never checks the guest flag and never runs
//! `SessionHasPermissionToManageBot` on its own (it is folded into step 2). So an account with
//! `AuthService == "ldap"` is **refused** a 403 by `PUT /users/{id}/active {"active":false}` and
//! **deactivated** by `DELETE /users/{id}` — same operation, same account, opposite answers.
//! `an_ldap_account_is_refused_by_active_and_accepted_by_delete` in `tests/parity.rs` would pin
//! that if the stack had an LDAP account to try it on; it does not, and the asymmetry is
//! transcribed here rather than asserted. See [D-473].
//!
//! # What forwards
//!
//! | request | here | Go |
//! |---|---|---|
//! | every refusal above | served | — |
//! | `?permanent=true`, `EnableAPIUserDeletion` off | served — the 401, which writes nothing | — |
//! | `?permanent=true`, `EnableAPIUserDeletion` on | — | forwarded ([D-470]) |
//! | soft delete, target owns no bots | served | — |
//! | soft delete, target owns a bot | — | forwarded ([D-461]) |
//!
//! Both forwards are decided from `SELECT`s and the configuration, strictly before
//! [`mm_app::App::deactivate_user`] writes anything — which is the whole constraint this route
//! is under; see the module doc on [`mm_app::user_delete`].

use axum::extract::{Path, Query, Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::permission::PERMISSION_MANAGE_SYSTEM;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;
use crate::proxy;
use crate::user_updates::{refuse_untrusted_admin_edit, require_edit_target, status_ok};

/// Go's `strconv.ParseBool` (strconv/atob.go), which is the whole of the `permanent` parse.
///
/// Exactly ten spellings are accepted and **everything else is false**, because `deleteUser`
/// discards the error. Case is significant beyond the single letters: `TRUE`, `True` and `true`
/// parse, `tRue` does not.
#[must_use]
pub fn parse_go_bool(raw: &str) -> bool {
    matches!(raw, "1" | "t" | "T" | "TRUE" | "true" | "True")
}

/// The query string of `DELETE /users/{user_id}`.
#[derive(Debug, serde::Deserialize)]
pub struct DeleteUserQuery {
    #[serde(default)]
    permanent: String,
}

/// Port of `deleteUser` (api4/user.go:1663).
///
/// # The `?permanent=true` refusal tells a system admin something it tells nobody else
///
/// With `ServiceSettings.EnableAPIUserDeletion` off, Go re-reads the **caller** and picks
/// between two error ids: `api.user.delete_user.not_enabled.for_admin.app_error` when the caller
/// is a system admin and `api.user.delete_user.not_enabled.app_error` when not — Go's comment
/// calls the first a "More verbose error message for system admins". Both are 401 and both are
/// reached only after every gate above has passed, so the fork leaks nothing to a caller who
/// could not already delete the account. It is also the *reachable* arm on this deployment: the
/// flag is off in the live document, and [D-470] is the other one.
///
/// A `GetUser` failure on that re-read is swallowed (`usrErr == nil && loggedUser != nil`), which
/// falls to the non-admin message — so a caller whose own row has vanished mid-request gets the
/// terse id, not a 500.
///
/// # The response is `{"status":"OK"}` either way
///
/// `ReturnStatusOK(w)`. A permanent delete and a soft delete are indistinguishable from the
/// response; only the database says which happened.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id, permanent))]
pub async fn delete_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Query(query): Query<DeleteUserQuery>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    delete_resolved_user(state, user_id, query, session, request).await
}

/// `DELETE /api/v4/users/me` — the same handler for the path axum has to register separately.
///
/// Go has no such route. `BaseRoutes.User` is `/users/{user_id:[A-Za-z0-9]+}` and `me` matches
/// it like any other segment; `RequireUserId` then swaps in the session's own id
/// (web/context.go:301). Here `/api/v4/users/me` is a **literal** sibling that axum prefers to
/// the parameterised route, so without this registration a `DELETE` on it reaches no method and
/// is forwarded — which is what it did until this route landed, and what `PUT /users/me` still
/// does. The self-deactivation refusal is the reachable answer on this stack, and
/// `a_self_delete_is_refused_while_the_deactivation_flag_is_off` asserts both spellings give it.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id, permanent))]
pub async fn delete_user_me(
    State(state): State<AppState>,
    Query(query): Query<DeleteUserQuery>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = session.0.user_id.clone();
    delete_resolved_user(state, user_id, query, session, request).await
}

/// Everything after `RequireUserId`'s `me` substitution, shared by the two registrations.
async fn delete_resolved_user(
    state: AppState,
    user_id: String,
    query: DeleteUserQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    let permanent = parse_go_bool(&query.permanent);
    tracing::Span::current().record("user_id", &user_id);
    tracing::Span::current().record("permanent", permanent);

    if let Err(err) = require_edit_target(&state, &session, &user_id).await {
        return err.into_response();
    }

    // "if EnableUserDeactivation flag is disabled the user cannot deactivate himself."
    if user_id == session.0.user_id
        && !state.app.config().enable_user_deactivation
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return ApiError::from(AppError::new(
            "deleteUser",
            "api.user.update_active.not_enable.app_error",
            None,
            format!("userId={user_id}"),
            401,
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

    if permanent {
        if state.app.config().enable_api_user_deletion {
            // `App.PermanentDeleteUser` — see [D-470].
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        let caller_is_admin = match state.app.get_user(&session.0.user_id).await {
            Ok(caller) => caller.is_system_admin(),
            // `usrErr == nil && loggedUser != nil` — a failed re-read takes the terse arm.
            Err(_) => false,
        };
        let id = if caller_is_admin {
            "api.user.delete_user.not_enabled.for_admin.app_error"
        } else {
            "api.user.delete_user.not_enabled.app_error"
        };
        return ApiError::from(AppError::new(
            "deleteUser",
            id,
            None,
            format!("userId={user_id}"),
            401,
        ))
        .into_response();
    }

    // The bot cascade and the sysadmin DM are the only parts of `userDeactivated` this process
    // cannot reproduce, and both are no-ops for an owner of no bots. Asked here, before the
    // `UPDATE`, because there is no prefix of a deactivation that can be handed over.
    match state.app.owns_bots(&user_id).await {
        Ok(true) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(false) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }

    if let Err(err) = state.app.deactivate_user(&user).await {
        return ApiError::from(err).into_response();
    }

    status_ok()
}

#[cfg(test)]
mod tests {
    use super::parse_go_bool;

    /// `strconv.ParseBool`'s ten spellings, and the near-misses that are silently false.
    ///
    /// The near-misses are the point: every one of them turns a request that *means* "erase this
    /// account" into a soft delete answered `{"status":"OK"}`. A port that used
    /// `str::parse::<bool>()` would accept only `true`/`false` and reject `1` and `t` — which,
    /// with the error discarded, reads as `permanent=false` and quietly downgrades a permanent
    /// delete. A port that used a case-insensitive compare would accept `tRue`, which Go does
    /// not, and upgrade one.
    #[test]
    fn parse_go_bool_matches_strconv() {
        for yes in ["1", "t", "T", "TRUE", "true", "True"] {
            assert!(parse_go_bool(yes), "{yes} parses true in Go");
        }
        for no in [
            "0", "f", "F", "FALSE", "false", "False", "", "yes", "Y", "on", "tRue", "TrUe",
            "TRUE ", " true", "2", "-1", "null",
        ] {
            assert!(!parse_go_bool(no), "{no} is false in Go");
        }
    }
}
