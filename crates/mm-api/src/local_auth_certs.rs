//! `api4/ldap_local.go` and `InitSamlLocal` (api4/saml.go:35) on the local socket: the seven
//! pairs of `POST /ldap/{migrateid,sync,test}`, `POST`/`DELETE /ldap/certificate/{public,private}`
//! and `POST /saml/reset_auth_data` reached through `APILocal`.
//!
//! Every one is the HTTP handler in [`crate::auth_certs`] under [`local_session`] and nothing
//! else: Go registers the same functions, and a local session is unrestricted, so the permission
//! checks pass and the answer is decided by what comes after them — the licence gate on the
//! stack's Go (which the socket belongs to) for `sync` and `test`, the body for `migrateid` and
//! `reset_auth_data`, the multipart parse for the two adds. The two removes and a well-formed add
//! forward, and the forward goes over the **socket** through `proxy::forward_to_go`'s
//! [`crate::local::GoLocalSocket`] intercept, so Go answers as `APILocal` rather than as
//! `APISessionRequired`.
//!
//! `GET /ldap/groups` is registered by `InitLdapLocal` too and is not this module's; it falls
//! through to the socket fallback.

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::post;

use crate::AppState;
use crate::auth_certs;
use crate::local::{local_session, partially_migrated};

/// The seven registrations, merged into [`crate::local::router`].
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v4/ldap/migrateid",
            partially_migrated(post(local_migrate_id_ldap)),
        )
        .route(
            "/api/v4/ldap/sync",
            partially_migrated(post(local_sync_ldap)),
        )
        .route(
            "/api/v4/ldap/test",
            partially_migrated(post(local_test_ldap)),
        )
        .route(
            "/api/v4/ldap/certificate/public",
            partially_migrated(
                post(local_add_ldap_public_certificate)
                    .delete(local_remove_ldap_public_certificate),
            ),
        )
        .route(
            "/api/v4/ldap/certificate/private",
            partially_migrated(
                post(local_add_ldap_private_certificate)
                    .delete(local_remove_ldap_private_certificate),
            ),
        )
        .route(
            "/api/v4/saml/reset_auth_data",
            partially_migrated(post(local_reset_auth_data_to_email)),
        )
}

/// `migrateIDLdap` through `APILocal` (ldap_local.go:9).
async fn local_migrate_id_ldap(state: State<AppState>, request: Request) -> Response {
    auth_certs::migrate_id_ldap(state, local_session(), request).await
}

/// `syncLdap` through `APILocal` (ldap_local.go:10).
async fn local_sync_ldap(state: State<AppState>, request: Request) -> Response {
    auth_certs::sync_ldap(state, local_session(), request).await
}

/// `testLdap` through `APILocal` (ldap_local.go:11).
async fn local_test_ldap(state: State<AppState>, request: Request) -> Response {
    auth_certs::test_ldap(state, local_session(), request).await
}

/// `addLdapPublicCertificate` through `APILocal` (ldap_local.go:13).
async fn local_add_ldap_public_certificate(state: State<AppState>, request: Request) -> Response {
    auth_certs::add_ldap_public_certificate(state, local_session(), request).await
}

/// `addLdapPrivateCertificate` through `APILocal` (ldap_local.go:14).
async fn local_add_ldap_private_certificate(state: State<AppState>, request: Request) -> Response {
    auth_certs::add_ldap_private_certificate(state, local_session(), request).await
}

/// `removeLdapPublicCertificate` through `APILocal` (ldap_local.go:15).
async fn local_remove_ldap_public_certificate(
    state: State<AppState>,
    request: Request,
) -> Response {
    auth_certs::remove_ldap_public_certificate(state, local_session(), request).await
}

/// `removeLdapPrivateCertificate` through `APILocal` (ldap_local.go:16).
async fn local_remove_ldap_private_certificate(
    state: State<AppState>,
    request: Request,
) -> Response {
    auth_certs::remove_ldap_private_certificate(state, local_session(), request).await
}

/// `resetAuthDataToEmail` through `APILocal` (saml.go:36).
async fn local_reset_auth_data_to_email(state: State<AppState>, request: Request) -> Response {
    auth_certs::reset_auth_data_to_email(state, local_session(), request).await
}
