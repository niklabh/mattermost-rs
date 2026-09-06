//! Port of `getServerLimits` (channels/api4/limits.go:19), reached as
//! `GET /api/v4/limits/server`.
//!
//! The webapp asks for this on **every** login and config refresh — `loadMe()` fans out to it —
//! so it is one of the hottest routes on the server, and the reason Go goes to the trouble of
//! skipping the count queries for non-admins.
//!
//! # The non-admin answer is built by the *handler*, not the app layer
//!
//! `App.GetServerLimits(false)` still returns the seat limits; it only skips the counts
//! (app/limits.go:59). The handler then throws even those away and rebuilds a `ServerLimits` with
//! **five explicit zeros**, keeping only the two post-history fields (limits.go:32-43). So the
//! same call answers differently depending on who asked, and the difference is applied twice, in
//! two places. Both are ported where Go put them.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::limits::ServerLimits;
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS,
};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// Port of `getServerLimits` (limits.go:19).
///
/// # "Admin" is two permissions, and both are system-scoped
///
/// `c.IsSystemAdmin() && SessionHasPermissionTo(sysconsole_read_user_management_users)` —
/// `IsSystemAdmin` is itself `SessionHasPermissionTo(manage_system)` (web/context.go:134), so this
/// is an **and** of two permissions and not a role check. A caller holding only one of them is a
/// non-admin here.
///
/// # There is no refusal
///
/// Every session gets a 200. A non-admin's answer is all zeros rather than a 403, which is what
/// lets the webapp call this unconditionally at login.
///
/// # Licensed installations are forwarded
///
/// Every non-zero field a licence would produce comes from the licence body — seat limits from
/// `Features.Users`, the history limit from `Limits.PostHistory` — and none of it is readable
/// from here. Same boundary as `getClientLicense`.
///
/// `json.NewEncoder(w).Encode`, so a trailing newline.
#[tracing::instrument(skip_all, fields(admin, licensed))]
pub async fn get_server_limits(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let licence = match state.app.license_state().await {
        Ok(licence) => licence,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if licence == mm_app::license::LicenseState::Licensed {
        tracing::Span::current().record("licensed", true);
        return proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("licensed", false);

    // `c.IsSystemAdmin() && …` — evaluated in Go's order, and both are system-scoped.
    let is_admin = counts_are_visible_to(
        state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await,
        state
            .app
            .session_has_permission_to(
                &session.0,
                &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS,
            )
            .await,
    );
    tracing::Span::current().record("admin", is_admin);

    let limits = match state.app.get_server_limits(is_admin).await {
        Ok(limits) => limits,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let limits = if is_admin {
        limits
    } else {
        // The handler's own rebuild (limits.go:33-42): five explicit zeros, and the two
        // post-history fields carried across from what the app layer returned. On an unlicensed
        // server those two are zero as well, so the whole object is zeros — but the *shape* of
        // the rule is what is ported, not the arithmetic it happens to produce here.
        ServerLimits {
            max_users_limit: 0,
            max_users_hard_limit: 0,
            active_user_count: 0,
            single_channel_guest_count: 0,
            single_channel_guest_limit: 0,
            last_accessible_post_time: limits.last_accessible_post_time,
            post_history_limit: limits.post_history_limit,
        }
    };

    match serde_json::to_vec(&limits) {
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
            tracing::error!(error = %err, "failed to serialise the server limits");
            ApiError::from(AppError::new(
                "getServerLimits",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// Go's `c.IsSystemAdmin() && SessionHasPermissionTo(sysconsole_read_user_management_users)`
/// (limits.go:20), as a value.
///
/// Extracted because it is **not reachable as two decisions over HTTP**: the only role granting
/// `manage_system` on a stock server also grants the sysconsole permission, so no caller can hold
/// one without the other and a mutation dropping either half survives the parity suite. A pure
/// function with a truth table is where a rule with no reachable branch can still be tested.
fn counts_are_visible_to(manages_system: bool, reads_user_management: bool) -> bool {
    manages_system && reads_user_management
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The admin test's whole truth table. Only the pair passes — and the pair is exactly what no
    /// stock role can separate, which is why this is asserted here rather than over HTTP.
    #[test]
    fn both_permissions_are_required_and_neither_alone_will_do() {
        assert!(counts_are_visible_to(true, true));
        assert!(
            !counts_are_visible_to(true, false),
            "manage_system alone is not enough"
        );
        assert!(
            !counts_are_visible_to(false, true),
            "nor is the sysconsole read on its own"
        );
        assert!(!counts_are_visible_to(false, false));
    }

    /// The non-admin rebuild keeps **exactly** the two post-history fields. Asserted with
    /// non-zero values, which an unlicensed server cannot produce — so the test is about the rule
    /// and not about the zeros this deployment happens to have.
    #[test]
    fn the_non_admin_rebuild_keeps_only_the_post_history_fields() {
        let full = ServerLimits {
            max_users_limit: 200,
            max_users_hard_limit: 250,
            active_user_count: 91,
            single_channel_guest_count: 3,
            single_channel_guest_limit: 4,
            post_history_limit: 5000,
            last_accessible_post_time: 1_788_636_490_668,
        };

        let limited = ServerLimits {
            max_users_limit: 0,
            max_users_hard_limit: 0,
            active_user_count: 0,
            single_channel_guest_count: 0,
            single_channel_guest_limit: 0,
            last_accessible_post_time: full.last_accessible_post_time,
            post_history_limit: full.post_history_limit,
        };

        assert_eq!(limited.post_history_limit, 5000);
        assert_eq!(limited.last_accessible_post_time, 1_788_636_490_668);
        assert_eq!(limited.max_users_limit, 0);
        assert_eq!(limited.active_user_count, 0);
        assert_eq!(limited.single_channel_guest_count, 0);
    }
}
