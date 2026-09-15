//! The access-control-policy family on the local socket: the port of
//! `api4/access_control_local.go`.
//!
//! `InitAccessControlPolicyLocal` registers fourteen of the sixteen HTTP pairs — everything but
//! `GET /{policy_id}/activate` (the deprecated single toggle) and `POST /cel/simulate_users`,
//! which the local mux has never heard of and 404s — and every one of them is the **HTTP
//! handler** through `APILocal`: the same Go function with `model.Session{Local: true}`. There
//! are no `local*` variants in this family, so each registration here is the HTTP handler in
//! `crate::access_control_policies` called with [`local_session`] and nothing else.
//!
//! What the unrestricted session changes is decided inside those handlers: `SessionHasPermission
//! To(manage_system)` is `true`, so every delegated-admin rung is skipped and each route goes
//! straight to its app function — the nil-service 501, or, for the autocomplete, a field search
//! whose caller id is the local session's **empty** user id (`RequestContextWithCallerID` takes
//! the raw id, not the `system:local_admin` tag the properties API uses).

use axum::Router;
use axum::extract::{Path as UrlPath, RawQuery, Request, State};
use axum::response::Response;
use axum::routing::{get, post, put};

use crate::AppState;
use crate::access_control_policies as http;
use crate::local::{local_session, partially_migrated, partially_migrated_with_ids};

/// The registrations of `InitAccessControlPolicyLocal` (api4/access_control_local.go:8),
/// merged into [`crate::local::router`].
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/v4/access_control_policies",
            partially_migrated(put(local_create_access_control_policy)),
        )
        .route(
            "/api/v4/access_control_policies/search",
            partially_migrated(post(local_search_access_control_policies)),
        )
        .route(
            "/api/v4/access_control_policies/activate",
            partially_migrated(put(local_set_active_status)),
        )
        .route(
            "/api/v4/access_control_policies/cel/check",
            partially_migrated(post(local_check_expression)),
        )
        .route(
            "/api/v4/access_control_policies/cel/test",
            partially_migrated(post(local_test_expression)),
        )
        .route(
            "/api/v4/access_control_policies/cel/validate_requester",
            partially_migrated(post(local_validate_expression_against_requester)),
        )
        .route(
            "/api/v4/access_control_policies/cel/autocomplete/fields",
            partially_migrated(get(local_get_fields_autocomplete)),
        )
        .route(
            "/api/v4/access_control_policies/cel/visual_ast",
            partially_migrated(post(local_convert_to_visual_ast)),
        )
        .route(
            "/api/v4/access_control_policies/{policy_id}",
            partially_migrated_with_ids(
                state,
                get(local_get_access_control_policy).delete(local_delete_access_control_policy),
            ),
        )
        .route(
            "/api/v4/access_control_policies/{policy_id}/assign",
            partially_migrated_with_ids(state, post(local_assign_access_policy)),
        )
        .route(
            "/api/v4/access_control_policies/{policy_id}/unassign",
            partially_migrated_with_ids(state, axum::routing::delete(local_unassign_access_policy)),
        )
        .route(
            "/api/v4/access_control_policies/{policy_id}/resources/channels",
            partially_migrated_with_ids(state, get(local_get_channels_for_access_control_policy)),
        )
        .route(
            "/api/v4/access_control_policies/{policy_id}/resources/channels/search",
            partially_migrated_with_ids(
                state,
                post(local_search_channels_for_access_control_policy),
            ),
        )
}

async fn local_create_access_control_policy(
    state: State<AppState>,
    query: RawQuery,
    request: Request,
) -> Response {
    http::create_access_control_policy(state, query, local_session(), request).await
}

async fn local_search_access_control_policies(
    state: State<AppState>,
    request: Request,
) -> Response {
    http::search_access_control_policies(state, local_session(), request).await
}

async fn local_set_active_status(state: State<AppState>, request: Request) -> Response {
    http::set_active_status(state, local_session(), request).await
}

async fn local_check_expression(state: State<AppState>, request: Request) -> Response {
    http::check_expression(state, local_session(), request).await
}

async fn local_test_expression(state: State<AppState>, request: Request) -> Response {
    http::test_expression(state, local_session(), request).await
}

async fn local_validate_expression_against_requester(
    state: State<AppState>,
    request: Request,
) -> Response {
    http::validate_expression_against_requester(state, local_session(), request).await
}

async fn local_get_fields_autocomplete(state: State<AppState>, query: RawQuery) -> Response {
    http::get_fields_autocomplete(state, query, local_session()).await
}

async fn local_convert_to_visual_ast(state: State<AppState>, request: Request) -> Response {
    http::convert_to_visual_ast(state, local_session(), request).await
}

async fn local_get_access_control_policy(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Response {
    http::get_access_control_policy(state, path, query, local_session()).await
}

async fn local_delete_access_control_policy(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Response {
    http::delete_access_control_policy(state, path, query, local_session()).await
}

async fn local_assign_access_policy(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    http::assign_access_policy(state, path, local_session(), request).await
}

async fn local_unassign_access_policy(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    http::unassign_access_policy(state, path, local_session(), request).await
}

async fn local_get_channels_for_access_control_policy(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
) -> Response {
    http::get_channels_for_access_control_policy(state, path, query, local_session()).await
}

async fn local_search_channels_for_access_control_policy(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
    request: Request,
) -> Response {
    http::search_channels_for_access_control_policy(state, path, query, local_session(), request)
        .await
}
