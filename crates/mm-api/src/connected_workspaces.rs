//! Port of `api4/remote_cluster.go` and `api4/shared_channel.go` — the thirteen routes gated on
//! the remote-cluster service.
//!
//! # The service cannot start without a licence, so the gate is a licence gate here
//!
//! `Server.startRemoteClusterService` refuses on **two** conditions (app/server.go:683-693): no
//! licence with `HasRemoteClusterService` or `HasSharedChannels`, **or** neither
//! `ConnectedWorkspacesSettings.EnableRemoteClusterService` nor `EnableSharedChannels` set. The
//! licence half comes first and cannot be rescued by the config half, so on an unlicensed server
//! `App.GetRemoteClusterService()` is always nil and every one of these routes answers
//! `api.remote_cluster.service_not_enabled.app_error` at **501**.
//!
//! That makes this a licence boundary from here — unlike `/recaps`, whose gate an operator can
//! flip on their own. A licensed installation is forwarded, and Go then evaluates the config half
//! that this server has no reason to model.
//!
//! # The gate is **not** always the first check, and it moves relative to the permission
//!
//! | Route | Order |
//! |---|---|
//! | `getRemoteClusters`, `getRemoteCluster` | permission (either of two), then the gate |
//! | `createRemoteCluster`, `remoteClusterAcceptInvite` | permission, then the gate, then the body |
//! | `generateRemoteClusterInvite`, `patchRemoteCluster`, `deleteRemoteCluster` | permission, then the id, then the gate |
//! | `getSharedChannelRemotesByRemoteCluster` | the id, then permission, then the gate |
//! | `inviteRemoteClusterToChannel`, `uninviteRemoteClusterToChannel` | two ids, then permission, then the gate |
//! | `getRemoteClusterInfo` | the id, then the gate — **no permission check at all** |
//! | `getSharedChannels`, `getSharedChannelRemotes` | the id, then **the gate, then the permission** |
//!
//! The last row is the one worth the module: two routes consult the service *before* asking
//! whether the caller may see the team or channel, so an unauthorised caller gets 501 rather than
//! 403 — the opposite of every other route here.
//!
//! # Three routes in `remote_cluster.go` are **not** here
//!
//! `remoteClusterAcceptMessage`, `remoteClusterPing` and `remoteClusterConfirmInvite` are
//! `api.RemoteClusterTokenRequired` — a different authentication wrapper that expects a remote
//! cluster's own token, not a session. So are the two file-streaming routes. Answering them from
//! a session-authenticated handler would change who can reach them.
//!
//! `canUserDirectMessage` is not here either: it is an ordinary read that answers **200** on this
//! server, measured — it consults the shared-channel *store*, not the service.

use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_MANAGE_SECURE_CONNECTIONS, PERMISSION_MANAGE_SHARED_CHANNELS,
    PERMISSION_READ_CHANNEL, PERMISSION_VIEW_TEAM, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::LicenceGate;
use crate::error::ApiError;

/// `App.GetRemoteClusterService` (app/remote_cluster.go:352).
const SERVICE_NOT_ENABLED: &str = "api.remote_cluster.service_not_enabled.app_error";

/// The gate, or the proxy when a licence is installed.
async fn gate(state: &AppState, where_: &'static str, request: Request) -> Response {
    match crate::channels::licence_gate(state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => ApiError::from(AppError::new(
            where_,
            SERVICE_NOT_ENABLED,
            None,
            String::new(),
            501,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// `RequirePermissionToManageSecureConnections` (web/context.go:845).
async fn require_manage_secure_connections(
    state: &AppState,
    session: &mm_model::session::Session,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to(session, &PERMISSION_MANAGE_SECURE_CONNECTIONS)
        .await
    {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error(
        session,
        &[&PERMISSION_MANAGE_SECURE_CONNECTIONS],
    )))
}

/// `RequirePermissionToManageSecureConnectionsOrSharedChannels` (web/context.go:867).
///
/// `SessionHasPermissionToAny` over the two, and the refusal names **both** — so its body differs
/// from the single-permission one above, which is why the two are not folded together.
async fn require_manage_secure_connections_or_shared_channels(
    state: &AppState,
    session: &mm_model::session::Session,
) -> Result<(), ApiError> {
    let permissions: &[&mm_model::permission::Permission] = &[
        &PERMISSION_MANAGE_SECURE_CONNECTIONS,
        &PERMISSION_MANAGE_SHARED_CHANNELS,
    ];
    if state
        .app
        .session_has_permission_to_any(session, permissions)
        .await
    {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error(session, permissions)))
}

/// `RequirePermissionToManageSharedChannels` (web/context.go:856).
async fn require_manage_shared_channels(
    state: &AppState,
    session: &mm_model::session::Session,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to(session, &PERMISSION_MANAGE_SHARED_CHANNELS)
        .await
    {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error(
        session,
        &[&PERMISSION_MANAGE_SHARED_CHANNELS],
    )))
}

/// `c.RequireTeamId()` / `c.RequireChannelId()` — `IsValidId`, the usual check.
fn require_id(value: &str, parameter: &'static str) -> Result<(), ApiError> {
    if is_valid_id(value) {
        return Ok(());
    }
    Err(ApiError::invalid_url_param(parameter))
}

/// `c.RequireRemoteId()` (web/context.go:745) — and it is **not** `IsValidId`.
///
/// It tests `c.Params.RemoteId == ""` and nothing else. A twenty-six-character id, a five-letter
/// word and a UUID all pass; only the empty string is refused, and gorilla's
/// `{remote_id:[A-Za-z0-9]+}` cannot produce an empty segment, so the check is unreachable
/// through the router.
///
/// **Measured, not read.** The port applied `IsValidId` here at first — the shape every
/// neighbouring `Require*Id` has — and answered 400 to `PATCH /api/v4/remotecluster/short` where
/// Go answers its 501. Four routes were wrong the same way, and the two `/sharedchannels` routes
/// beside them really do use `IsValidId`, which is what made the mistake look right.
fn require_remote_id(value: &str) -> Result<(), ApiError> {
    if !value.is_empty() {
        return Ok(());
    }
    Err(ApiError::invalid_url_param("remote_id"))
}

// ---------------------------------------------------------------------------------------------
// remote_cluster.go — permission first, then (sometimes) the id, then the gate
// ---------------------------------------------------------------------------------------------

/// Port of `getRemoteClusters` (remote_cluster.go:320).
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_remote_clusters(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_manage_secure_connections_or_shared_channels(&state, &session.0).await
    {
        return err.into_response();
    }
    gate(&state, "getRemoteClusters", request).await
}

/// Port of `createRemoteCluster` (remote_cluster.go:366).
///
/// The body is decoded **after** the gate, so a malformed body on an unlicensed server is still
/// the 501 — the opposite of `/data_retention`'s create.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn create_remote_cluster(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_manage_secure_connections(&state, &session.0).await {
        return err.into_response();
    }
    gate(&state, "createRemoteCluster", request).await
}

/// Port of `remoteClusterAcceptInvite` (remote_cluster.go:453).
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn remote_cluster_accept_invite(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_manage_secure_connections(&state, &session.0).await {
        return err.into_response();
    }
    gate(&state, "remoteClusterAcceptInvite", request).await
}

/// Port of `generateRemoteClusterInvite` (remote_cluster.go:532).
#[tracing::instrument(skip_all, fields(remote_id, licensed))]
pub async fn generate_remote_cluster_invite(
    State(state): State<AppState>,
    Path(remote_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("remote_id", &remote_id);
    if let Err(err) = require_manage_secure_connections(&state, &session.0).await {
        return err.into_response();
    }
    if let Err(err) = require_remote_id(&remote_id) {
        return err.into_response();
    }
    gate(&state, "generateRemoteClusterInvite", request).await
}

/// Port of `getRemoteCluster` (remote_cluster.go:591).
///
/// **No id check**, unlike its `PATCH` and `DELETE` neighbours on the same path: Go reads
/// `c.Params.RemoteId` straight into the store call. On an unlicensed server nothing reaches the
/// store, so the difference is invisible here — and reproducing a check Go does not make would be
/// the divergence.
#[tracing::instrument(skip_all, fields(remote_id, licensed))]
pub async fn get_remote_cluster(
    State(state): State<AppState>,
    Path(remote_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("remote_id", &remote_id);
    if let Err(err) = require_manage_secure_connections_or_shared_channels(&state, &session.0).await
    {
        return err.into_response();
    }
    gate(&state, "getRemoteCluster", request).await
}

/// Port of `patchRemoteCluster` (remote_cluster.go:620).
#[tracing::instrument(skip_all, fields(remote_id, licensed))]
pub async fn patch_remote_cluster(
    State(state): State<AppState>,
    Path(remote_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("remote_id", &remote_id);
    if let Err(err) = require_manage_secure_connections(&state, &session.0).await {
        return err.into_response();
    }
    if let Err(err) = require_remote_id(&remote_id) {
        return err.into_response();
    }
    gate(&state, "patchRemoteCluster", request).await
}

/// Port of `deleteRemoteCluster` (remote_cluster.go:673).
#[tracing::instrument(skip_all, fields(remote_id, licensed))]
pub async fn delete_remote_cluster(
    State(state): State<AppState>,
    Path(remote_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("remote_id", &remote_id);
    if let Err(err) = require_manage_secure_connections(&state, &session.0).await {
        return err.into_response();
    }
    if let Err(err) = require_remote_id(&remote_id) {
        return err.into_response();
    }
    gate(&state, "deleteRemoteCluster", request).await
}

// ---------------------------------------------------------------------------------------------
// shared_channel.go — the id first, and two routes put the gate *before* the permission
// ---------------------------------------------------------------------------------------------

/// Port of `getSharedChannels` (shared_channel.go:26).
///
/// **The gate comes before the permission check.** A caller who cannot view the team gets the 501,
/// not a 403 — the opposite of every route in `remote_cluster.go`.
#[tracing::instrument(skip_all, fields(team_id, licensed))]
pub async fn get_shared_channels(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("team_id", &team_id);
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let response = gate(&state, "getSharedChannels", request).await;
    if response.status() != axum::http::StatusCode::NOT_IMPLEMENTED {
        return response;
    }

    // Unlicensed: the gate answered, and Go never reaches the permission check either. The check
    // is written out so the *order* is in the source rather than only in a comment — it is what
    // this route does differently from its neighbours.
    let _would_check = state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
        .await;
    response
}

/// Port of `getRemoteClusterInfo` (shared_channel.go:70).
///
/// **No permission check at all** — the id, then the gate. Any authenticated session may ask.
#[tracing::instrument(skip_all, fields(remote_id, licensed))]
pub async fn get_remote_cluster_info(
    State(state): State<AppState>,
    Path(remote_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("remote_id", &remote_id);
    if let Err(err) = require_remote_id(&remote_id) {
        return err.into_response();
    }
    gate(&state, "getRemoteClusterInfo", request).await
}

/// Port of `getSharedChannelRemotesByRemoteCluster` (shared_channel.go:102).
#[tracing::instrument(skip_all, fields(remote_id, licensed))]
pub async fn get_shared_channel_remotes_by_remote_cluster(
    State(state): State<AppState>,
    Path(remote_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("remote_id", &remote_id);
    if let Err(err) = require_remote_id(&remote_id) {
        return err.into_response();
    }
    if let Err(err) = require_manage_secure_connections_or_shared_channels(&state, &session.0).await
    {
        return err.into_response();
    }
    gate(&state, "getSharedChannelRemotesByRemoteCluster", request).await
}

/// Port of `inviteRemoteClusterToChannel` (shared_channel.go:143).
#[tracing::instrument(skip_all, fields(remote_id, channel_id, licensed))]
pub async fn invite_remote_cluster_to_channel(
    State(state): State<AppState>,
    Path((remote_id, channel_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    invite_or_uninvite(
        state,
        remote_id,
        channel_id,
        session,
        "inviteRemoteClusterToChannel",
        request,
    )
    .await
}

/// Port of `uninviteRemoteClusterToChannel` (shared_channel.go:194) — the same checks in the same
/// order, and the same 501.
#[tracing::instrument(skip_all, fields(remote_id, channel_id, licensed))]
pub async fn uninvite_remote_cluster_to_channel(
    State(state): State<AppState>,
    Path((remote_id, channel_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    invite_or_uninvite(
        state,
        remote_id,
        channel_id,
        session,
        "uninviteRemoteClusterToChannel",
        request,
    )
    .await
}

async fn invite_or_uninvite(
    state: AppState,
    remote_id: String,
    channel_id: String,
    session: AuthenticatedSession,
    where_: &'static str,
    request: Request,
) -> Response {
    tracing::Span::current().record("remote_id", &remote_id);
    tracing::Span::current().record("channel_id", &channel_id);
    // **The remote id first, then the channel id.** Both are checked, and a request with two bad
    // ids reports the remote one.
    if let Err(err) = require_remote_id(&remote_id) {
        return err.into_response();
    }
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    if let Err(err) = require_manage_shared_channels(&state, &session.0).await {
        return err.into_response();
    }
    gate(&state, where_, request).await
}

/// Port of `getSharedChannelRemotes` (shared_channel.go:254).
///
/// The second route whose gate precedes its permission check — here `read_channel` rather than
/// `view_team`.
#[tracing::instrument(skip_all, fields(channel_id, licensed))]
pub async fn get_shared_channel_remotes(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("channel_id", &channel_id);
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    let response = gate(&state, "getSharedChannelRemotes", request).await;
    if response.status() != axum::http::StatusCode::NOT_IMPLEMENTED {
        return response;
    }

    let _would_check = state
        .app
        .session_has_permission_to_channel(&session.0, &channel_id, &PERMISSION_READ_CHANNEL)
        .await;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error id and its status. It is a 501, unlike the cloud family's 400 and like the
    /// content-flagging one — three neighbouring families, three shapes.
    #[test]
    fn the_refusal_is_a_501_naming_the_service() {
        let err = AppError::new(
            "getRemoteClusters",
            SERVICE_NOT_ENABLED,
            None,
            String::new(),
            501,
        );
        assert_eq!(err.status_code, 501);
        assert_eq!(err.id, "api.remote_cluster.service_not_enabled.app_error");
        assert!(
            !err.id.contains("license"),
            "it names the service, not a licence — the licence is only why the service is absent"
        );
    }

    /// The two permissions the file uses are distinct, and the "either" refusal names both.
    #[test]
    fn the_two_permissions_are_distinct() {
        assert_eq!(
            PERMISSION_MANAGE_SECURE_CONNECTIONS.id,
            "manage_secure_connections"
        );
        assert_eq!(
            PERMISSION_MANAGE_SHARED_CHANNELS.id,
            "manage_shared_channels"
        );
        assert_ne!(
            PERMISSION_MANAGE_SECURE_CONNECTIONS.id,
            PERMISSION_MANAGE_SHARED_CHANNELS.id
        );
    }
}
