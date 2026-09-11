//! The `/api/v4/config` reads — `getConfig`, `getClientConfig` and `getEnvironmentConfig`
//! (channels/api4/config.go).
//!
//! # One document, three very different shapes
//!
//! `GET /config` is the whole `model.Config` with its secrets masked. `GET /config/client` is a
//! flat `map[string]string` that Go *builds* rather than projects — every value stringified, the
//! key set decided by whether the caller has a session and by the licence. `GET /config/environment`
//! is a sparse tree of `true`s naming the settings an environment variable overrode. Only the
//! first is a serialisation of the config struct.
//!
//! # Where the configuration comes from, and why it is read per request
//!
//! Go answers all three out of memory: `Store.Load` runs at boot and a config listener refreshes
//! it. This process re-reads the `Configurations` row on every request — see
//! [`mm_app::config::load_model_config`]. The Go server beside us still owns `PUT /config`, so a
//! cached copy here would report a configuration that is no longer in force the moment an admin
//! changes one. The cost is one small `SELECT`; the difference is that we are fresher than Go, not
//! staler.
//!
//! # What is forwarded, and why each thing is
//!
//! - **A licensed installation**, for all three. `GenerateClientConfig`'s licensed block is a
//!   feature matrix over the signed licence body — `MinimumProfessionalLicense`,
//!   `HasSharedChannels`, `license.Features.*` — none of which is ported, and `getConfig` adds a
//!   cloud tag filter when `License().IsCloud()`. Same decision, and the same reason, as
//!   [`crate::license::get_client_license`].
//! - **A session without `manage_system`**, for `getConfig` and `getEnvironmentConfig`. Both run
//!   every leaf of the struct through `readFilter` (config.go:432), which consults the `access:`
//!   tag on the Go *field* — about 1,300 tags that `mm_model::config` deliberately does not
//!   carry. With `manage_system` the filter's fallback returns true for every field and the merge
//!   is the identity, which is the case this serves; anything else would be a guess at which
//!   fields a partially-privileged admin may see. [D-312].
//! - **`?remove_masked=` or `?remove_defaults=`**, for `getConfig`. `RemoveDefaults` diffs against
//!   a `SetDefaults()`-filled config, and `SetDefaults` — several thousand lines of per-field
//!   logic — is not ported. [D-313].
//!
//! Forwarding happens **after** the permission check, so a caller with no system-console read
//! permission gets our 403 rather than Go's; the two are byte-identical and the parity suite says
//! so.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_app::license::LicenseState;
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, SYSCONSOLE_READ_PERMISSIONS, make_permission_error,
};
use mm_model::session::Session;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::auth_writes::OptionalSession;
use crate::channels::query_first;
use crate::error::ApiError;
use crate::proxy;

/// `w.Header().Set("Cache-Control", …)` — set by `getConfig`, `getEnvironmentConfig`,
/// `configReload` and both writes, and **not** by `getClientConfig`, which is the one route here
/// a proxy is allowed to cache.
const NO_STORE: (&str, &str) = ("Cache-Control", "no-cache, no-store, must-revalidate");

/// Port of `getConfig` (api4/config.go:51).
///
/// The body is `model.FilterConfig`'s `map[string]any`, not the struct — so **every object is
/// key-sorted**, which `serde_json::Value` gives for free because its map is a `BTreeMap`, and
/// `json.NewEncoder(w).Encode` appends a newline.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn get_config(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state
        .app
        .session_has_permission_to_any(&session.0, SYSCONSOLE_READ_PERMISSIONS)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            SYSCONSOLE_READ_PERMISSIONS,
        ))
        .into_response();
    }

    // `strconv.ParseBool(r.URL.Query().Get(…))` discards its error, so an unparseable value is
    // false and is *not* a reason to forward — only a value Go would read as true is.
    let filtered = ["remove_masked", "remove_defaults"].iter().any(|param| {
        query_first(request.uri().query(), param)
            .is_some_and(|raw| matches!(raw.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True"))
    });
    if filtered || !reads_every_field(&state, &session.0).await || licensed(&state).await {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }

    let mut config = match mm_app::config::load_model_config(state.app.store().config()).await {
        Ok(config) => config,
        Err(err) => return config_error("getConfig", &err).into_response(),
    };
    mm_app::config::sanitize(&mut config);

    match serde_json::to_value(&config) {
        Ok(body) => json_response(&body, true, true),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the configuration");
            ApiError::from(AppError::new(
                "getConfig",
                "api.filter_config_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// Port of `getClientConfig` (api4/config.go:245).
///
/// `APIHandler`, so **no session is required**; the session decides only *which* map is built.
/// `c.AppContext.Session().UserId == ""` is the test, and an unauthenticated caller gets the
/// limited one — which is the map every Mattermost client reads before it can log in.
///
/// Go writes it with `json.NewEncoder`, so the body ends in a newline, and it sets **no**
/// `Cache-Control` — the only route in this file that does not.
#[tracing::instrument(skip_all, fields(authenticated, keys))]
pub async fn get_client_config(
    State(state): State<AppState>,
    session: OptionalSession,
    request: Request,
) -> Response {
    if licensed(&state).await {
        return proxy::forward_to_go(State(state), request).await;
    }

    // Go reads `Session().UserId`, not the presence of a session object: a session row whose
    // `UserId` is empty takes the limited branch too.
    let authenticated = session
        .0
        .as_ref()
        .is_some_and(|session| !session.user_id.is_empty());
    tracing::Span::current().record("authenticated", authenticated);

    let props = if authenticated {
        state.app.client_config_with_computed().await
    } else {
        state.app.limited_client_config_with_computed().await
    };
    let props = match props {
        Ok(props) => props,
        Err(err) => return config_error("getClientConfig", &err).into_response(),
    };
    tracing::Span::current().record("keys", props.len());

    match serde_json::to_value(&props) {
        Ok(body) => json_response(&body, true, false),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the client configuration");
            ApiError::from(AppError::new(
                "getClientConfig",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// Port of `getEnvironmentConfig` (api4/config.go:256).
///
/// **This reports *this* process's environment, not the Go server's.** The two are separate
/// processes with separate environments, and the route's whole subject is which variables are
/// set — so it agrees with Go only when both are launched with the same `MM_*` overrides. That is
/// what `scripts/mm-api-env.sh` is for, and it is why that file is not merely a convenience.
///
/// `model.StringInterfaceToJSON` + `w.Write` — no encoder, so **no trailing newline**, unlike
/// `getConfig` two handlers up.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn get_environment_config(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `getEnvironmentConfig` has **no permission check of its own** — the only gate is the
    // per-field `readFilter`, which for a session with no system-console permission at all
    // returns false everywhere and leaves an empty object. Forwarding covers that case rather
    // than reproducing it.
    if !reads_every_field(&state, &session.0).await || licensed(&state).await {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }

    let body = mm_app::config::generate_environment_map(&mm_app::config::get_environment());
    json_response(&body, false, true)
}

/// Whether `readFilter` (api4/config.go:432) would pass **every** field for this session.
///
/// Its last line is `SessionHasPermissionTo(session, PermissionManageSystem)`, reached for any
/// field whose `access:` tags did not already grant it — so `manage_system` makes the filter the
/// identity. `IsUnrestricted` short-circuits to the same place for a local-mode session.
async fn reads_every_field(state: &AppState, session: &Session) -> bool {
    session.is_unrestricted()
        || state
            .app
            .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
            .await
}

/// Whether a licence is installed, with a read failure counted as "licensed" so the answer is
/// forwarded rather than guessed at.
async fn licensed(state: &AppState) -> bool {
    match state.app.license_state().await {
        Ok(state_of_licence) => state_of_licence == LicenseState::Licensed,
        Err(err) => {
            tracing::warn!(error = %err, "could not determine the licence state; forwarding");
            true
        }
    }
}

/// `json.NewEncoder(w).Encode(v)` when `newline`, `w.Write(json.Marshal(v))` when not.
///
/// `no_store` is the `Cache-Control` header, which two of these three routes set and
/// `getClientConfig` does not — see [`NO_STORE`].
fn json_response(body: &serde_json::Value, newline: bool, no_store: bool) -> Response {
    let mut bytes = match serde_json::to_vec(body) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(error = %err, "failed to encode a configuration response");
            return ApiError::from(AppError::new(
                "config",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    if newline {
        bytes.push(b'\n');
    }
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers.insert("x-mmrs-served-by", HeaderValue::from_static("rust"));
    if no_store {
        headers.insert(NO_STORE.0, HeaderValue::from_static(NO_STORE.1));
    }
    (StatusCode::OK, headers, bytes).into_response()
}

/// The 500 for a configuration that cannot be read or parsed.
///
/// Go cannot produce this — it is answering from a copy it loaded at boot — so there is no id to
/// port. `api.config.get_config.restricted_merge.app_error` is the nearest thing `getConfig` has
/// and is reused, with the failure in the detail where a log reader will find it.
fn config_error(where_: &str, err: &mm_app::config::ConfigError) -> ApiError {
    tracing::error!(error = %err, "could not read the configuration document");
    ApiError::from(AppError::new(
        where_,
        "api.config.get_config.restricted_merge.app_error",
        None,
        err.to_string(),
        500,
    ))
}
