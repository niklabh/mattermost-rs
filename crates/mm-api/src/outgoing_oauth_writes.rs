//! Port of the four writes in `api4/outgoing_oauth_connection.go` — `createOutgoingOAuthConnection`
//! (:206), `updateOutgoingOAuthConnection` (:250), `deleteOutgoingOAuthConnection` (:316) and
//! `validateOutgoingOAuthConnectionCredentials` (:357):
//!
//! ```text
//! POST   /api/v4/oauth/outgoing_connections
//! POST   /api/v4/oauth/outgoing_connections/validate
//! PUT    /api/v4/oauth/outgoing_connections/{outgoing_oauth_connection_id}
//! DELETE /api/v4/oauth/outgoing_connections/{outgoing_oauth_connection_id}
//! ```
//!
//! # Every one of them is a gate, and the gate is the whole route on this build
//!
//! Each handler opens with `checkOutgoingOAuthConnectionWritePermissions` — the single
//! `manage_outgoing_oauth_connections` permission, unlike the list read's three-way or — and then
//! `ensureOutgoingOAuthConnectionInterface` (:59), which the two reads in
//! [`crate::gated_reads`] share. That gate has two arms at one status: the setting off is 501
//! `…not_available.configuration_disabled`; the setting on asks `OutgoingOAuthConnections() ==
//! nil || !MinimumEnterpriseLicense` and answers 501 `api.license.upgrade_needed.app_error`.
//!
//! **The interface is nil on every build from this tree**, licensed or not — its implementation
//! lives in the enterprise repository, and `reference/mattermost/server/enterprise/` holds no
//! `OutgoingOAuthConnection`. So the second arm's licence half is never consulted, and both arms
//! are served unconditionally, the way `getPrevTrialLicense` is for its nil licence manager
//! ([D-087], served since 2026-09-13). The reads forward the licensed case instead; that is their
//! choice and predates the maintainer's direction that a nil enterprise interface is an answer.
//! The body — `SaveConnection`, `Patch`, `RetrieveTokenForConnection` (the outbound token
//! request the guard would govern) — is unreachable and not ported.
//!
//! `RequireOutgoingOAuthConnectionId` runs **after** the gate on the two id routes and the body
//! is decoded after it, so no request shape reaches a 400 here. The audit records are log lines.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_MANAGE_OUTGOING_OAUTH_CONNECTIONS, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `whereOutgoingOAuthConnection` (outgoing_oauth_connection.go:20).
const WHERE: &str = "Api4.outgoingOAuthConnection";
const CONFIGURATION_DISABLED: &str =
    "api.context.outgoing_oauth_connection.not_available.configuration_disabled";
const UPGRADE_NEEDED: &str = "api.license.upgrade_needed.app_error";

/// `ensureOutgoingOAuthConnectionInterface` (outgoing_oauth_connection.go:59) as it resolves on
/// a build whose `OutgoingOAuthConnections()` is nil: the setting picks which 501.
fn interface_refusal(enable_outgoing_oauth_connections: bool) -> ApiError {
    let id = if enable_outgoing_oauth_connections {
        UPGRADE_NEEDED
    } else {
        CONFIGURATION_DISABLED
    };
    ApiError::from(AppError::new(WHERE, id, None, String::new(), 501))
}

/// `checkOutgoingOAuthConnectionWritePermissions` (:50) then the interface gate — the prefix
/// every one of the four handlers shares, and on this build the whole of each.
async fn gate(state: &AppState, session: &AuthenticatedSession) -> Response {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OUTGOING_OAUTH_CONNECTIONS)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OUTGOING_OAUTH_CONNECTIONS],
        ))
        .into_response();
    }
    interface_refusal(state.app.config().enable_outgoing_oauth_connections).into_response()
}

/// Port of `createOutgoingOAuthConnection` (:206) — `POST /api/v4/oauth/outgoing_connections`.
#[tracing::instrument(skip_all)]
pub async fn create_outgoing_oauth_connection(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    gate(&state, &session).await
}

/// Port of `validateOutgoingOAuthConnectionCredentials` (:357) —
/// `POST /api/v4/oauth/outgoing_connections/validate`.
#[tracing::instrument(skip_all)]
pub async fn validate_outgoing_oauth_connection_credentials(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    gate(&state, &session).await
}

/// Port of `updateOutgoingOAuthConnection` (:250) —
/// `PUT /api/v4/oauth/outgoing_connections/{outgoing_oauth_connection_id}`.
#[tracing::instrument(skip_all, fields(connection_id = %connection_id))]
pub async fn update_outgoing_oauth_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let _ = &connection_id;
    gate(&state, &session).await
}

/// Port of `deleteOutgoingOAuthConnection` (:316) —
/// `DELETE /api/v4/oauth/outgoing_connections/{outgoing_oauth_connection_id}`.
#[tracing::instrument(skip_all, fields(connection_id = %connection_id))]
pub async fn delete_outgoing_oauth_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let _ = &connection_id;
    gate(&state, &session).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The setting picks the id; the status is 501 either way. The `upgrade_needed` arm is the
    /// one no shared oracle reaches (the stack's setting is off), so it is pinned here.
    #[test]
    fn the_setting_picks_which_501() {
        let off = interface_refusal(false);
        assert_eq!(off.0.status_code, 501);
        assert_eq!(off.0.id, CONFIGURATION_DISABLED);
        let on = interface_refusal(true);
        assert_eq!(on.0.status_code, 501);
        assert_eq!(on.0.id, UPGRADE_NEEDED);
    }
}
