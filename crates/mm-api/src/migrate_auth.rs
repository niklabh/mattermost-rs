//! The two authentication migrations — `migrateAuthToLDAP` (api4/user.go:3816) and
//! `migrateAuthToSaml` (api4/user.go:3875) — `POST /api/v4/users/migrate_auth/{ldap,saml}`.
//!
//! # Every request that clears its checks is the 501, licensed or not
//!
//! Each handler validates the body, requires `manage_system`, then tests the licence for the
//! LDAP or SAML feature and, past that, `c.App.AccountMigration()` — an enterprise interface
//! that nothing in this tree registers ([D-571]). Both the licence arm and the nil-interface arm
//! answer the same id (`api.admin.ldap.not_available.app_error` / `…saml…`) at 501, so the
//! licensed pair and the unlicensed one agree, and the migration itself is reachable on neither.
//! This port therefore answers the 501 after the checks without consulting the licence at all:
//! the two arms are indistinguishable on the wire and the second is always taken here.
//!
//! What is checked, in Go's order: the body is a free map (`StringInterfaceFromJSON` — anything
//! but an object is an empty map), `from` must be a string among the accepted providers
//! (`email`, `gitlab`, `google`, `office365`, and `saml` for the LDAP migration or `ldap` for the
//! SAML one — an empty string fails too), then `force`/`match_field` (LDAP) or `auto`/`matches`
//! (SAML) must be present with the right JSON type — each missing or mistyped field its own 400.
//! Only then the permission.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `model.StringInterfaceFromJSON(r.Body)`: an object, or an empty map for anything else.
async fn props(request: Request) -> serde_json::Map<String, serde_json::Value> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

/// The `from` provider check both handlers share, with each one's third accepted provider.
///
/// Go tests `from == ""` first and then the five providers; the empty test is redundant, since
/// `""` is none of them, and a mutation dropping it survived the batch for that reason. The
/// list alone is the check.
fn from_is_valid(from: &str, third: &str) -> bool {
    from == "email" || from == "gitlab" || from == third || from == "google" || from == "office365"
}

fn not_available(where_: &'static str, id: &'static str) -> Response {
    ApiError::from(AppError::new(where_, id, None, String::new(), 501)).into_response()
}

/// Port of `migrateAuthToLDAP` — `POST /api/v4/users/migrate_auth/ldap`.
#[tracing::instrument(skip_all)]
pub async fn migrate_auth_to_ldap(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let props = props(request).await;
    let Some(from) = props.get("from").and_then(|v| v.as_str()) else {
        return ApiError::invalid_param("from").into_response();
    };
    if !from_is_valid(from, "saml") {
        return ApiError::invalid_param("from").into_response();
    }
    if props.get("force").and_then(|v| v.as_bool()).is_none() {
        return ApiError::invalid_param("force").into_response();
    }
    if props.get("match_field").and_then(|v| v.as_str()).is_none() {
        return ApiError::invalid_param("match_field").into_response();
    }

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

    // The licence's LDAP feature, then `AccountMigration()`: the same 501 either way here.
    not_available(
        "api.migrateAuthToLDAP",
        "api.admin.ldap.not_available.app_error",
    )
}

/// Port of `migrateAuthToSaml` — `POST /api/v4/users/migrate_auth/saml`.
#[tracing::instrument(skip_all)]
pub async fn migrate_auth_to_saml(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let props = props(request).await;
    let Some(from) = props.get("from").and_then(|v| v.as_str()) else {
        return ApiError::invalid_param("from").into_response();
    };
    if !from_is_valid(from, "ldap") {
        return ApiError::invalid_param("from").into_response();
    }
    if props.get("auto").and_then(|v| v.as_bool()).is_none() {
        return ApiError::invalid_param("auto").into_response();
    }
    if props.get("matches").and_then(|v| v.as_object()).is_none() {
        return ApiError::invalid_param("matches").into_response();
    }

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

    not_available(
        "api.migrateAuthToSaml",
        "api.admin.saml.not_available.app_error",
    )
}
