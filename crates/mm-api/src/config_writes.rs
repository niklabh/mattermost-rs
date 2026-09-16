//! The `/api/v4/config` **writes** — `updateConfig`, `patchConfig` and `configReload`
//! (channels/api4/config.go), and their `config_local.go` twins `localUpdateConfig`,
//! `localPatchConfig`, `configReload` under `APILocal` and `localMigrateConfig`.
//!
//! # What is served and what is forwarded, and why the line is where it is
//!
//! Each write handler in Go is a run of **gates** — decode the body, check the permission,
//! refuse a cleared `SiteURL`, refuse the three settings a patch may not touch — followed by the
//! **save**: `config.Merge` through the per-field `access:` tags, `Config.SetDefaults`,
//! `Config.IsValid`, and `Store.Set`, which desanitizes the masked secrets, re-applies the
//! environment, strips the environment overrides back out, persists a new `Configurations` row,
//! and swaps the in-memory copy the Go server answers every later request from.
//!
//! The gates are served here and proved by parity. **The save is forwarded**, on every route, for
//! two reasons that are independent of each other:
//!
//! 1. `SetDefaults` and `IsValid` are not ported (`mm_model::config` is the wire shape only —
//!    seventy Go functions, [D-313]). A save that skipped `IsValid` would persist a document Go
//!    refuses with a 400, and Go's next `Load` would then fail on it and leave the server stuck.
//! 2. **Go holds its configuration in memory and `config.DatabaseStore` has no watcher.** A row
//!    this process wrote would not reach the Go server until something called `ReloadConfig`;
//!    the only route that does is `POST /config/reload`, which is Go's own handler. So during
//!    the strangler phase the process that must *observe* a write is the process that must
//!    *make* it. The converse direction needs nothing: `getConfig` and `localGetConfig` re-read
//!    the row per request (`mm_app::config::load_model_config`), so a write Go made is visible
//!    here on the next request — the parity suite patches through this server and reads the
//!    value back from both. [`mm_app::App::config`], the projection the ported gates consult,
//!    follows the row too: `mm_api::refresh_config_after_write` reloads it after every write
//!    request, forwarded or not, and a timer in `main.rs` catches the writes that reach Go by
//!    another way (`App::refresh_config`, `parity::config_reload`).
//!
//! The first is owed work, tracked as [D-700]; it is not a reason to serve less than the gates.
//!
//! # The gates read the **live** configuration, not the projection
//!
//! `appCfg := c.App.Config()` is the configuration Go runs on: the persisted document with the
//! environment overlaid. `SiteURL`, `PluginSettings.EnableUploads`, `ImportSettings.Directory`
//! and `PluginSettings.MarketplaceURL` are read here the same way, from the row plus this
//! process's environment on every request (`load_model_config`), because the comparison is
//! against what the last write left. The projection follows the row too, but a write made to
//! Go directly reaches it only on the next periodic check, and a gate that compares a patch
//! against the current value is exactly where that window would show.
//!
//! # The body is decoded the way `encoding/json` decodes a struct
//!
//! `json.NewDecoder(r.Body).Decode(&cfg)` reads **one** value — trailing bytes are not an
//! error — and a literal `null` leaves `cfg` nil, which is the same 400 as a syntax error.
//! Every section of `model.Config` is a non-pointer struct, so a `null` section is a **no-op**
//! in Go where serde would refuse it; every setting is a pointer, so a `null` setting is nil,
//! exactly as an absent one is. Both are reproduced by dropping every `null` object member
//! before the typed decode. Object keys are matched case-sensitively here and
//! case-insensitively by Go ([D-040]); the consequence on these routes is bounded, because a key
//! this server fails to see makes it *forward*, and Go's own gate then answers.

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use mm_model::config::Config as ConfigDocument;
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_RELOAD_CONFIG, SYSCONSOLE_WRITE_PERMISSIONS,
    make_permission_error,
};
use mm_model::session::Session;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::config::config_error;
use crate::error::ApiError;
use crate::local::{GoLocalSocket, local_session, partially_migrated};
use crate::proxy;

/// `api.config.update_config.clear_siteurl.app_error` — both writes, 400.
pub const CLEAR_SITEURL_ERROR: &str = "api.config.update_config.clear_siteurl.app_error";
/// `api.config.update_config.not_allowed_security.app_error` — `patchConfig`'s three 403s,
/// each naming the setting in `params.Name`.
pub const NOT_ALLOWED_SECURITY_ERROR: &str =
    "api.config.update_config.not_allowed_security.app_error";

/// `model.ServiceSettingsDefaultSiteURL` (config.go:128) — what `SetDefaults` puts in a nil
/// `SiteURL` when `EnableDeveloper` is set (config.go:512), and the reason an omitted `SiteURL`
/// is not always a cleared one.
const DEFAULT_SITE_URL: &str = "http://localhost:8065";
/// `model.PluginSettingsDefaultMarketplaceURL` (config.go:271).
const DEFAULT_MARKETPLACE_URL: &str = "https://api.integrations.mattermost.com";
/// `model.ImportSettingsDefaultDirectory` (config.go:165).
const DEFAULT_IMPORT_DIRECTORY: &str = "./import";

/// The local-router pairs of `config_local.go` other than the `PUT /config` that
/// `local_misc::routes` registers on its existing `/api/v4/config` method router.
pub(crate) fn local_routes(state: &AppState) -> Router<AppState> {
    let _ = state;
    Router::new()
        .route(
            "/api/v4/config/patch",
            partially_migrated(put(local_patch_config)),
        )
        .route(
            "/api/v4/config/reload",
            partially_migrated(post(local_reload_config)),
        )
        .route(
            "/api/v4/config/migrate",
            partially_migrated(post(local_migrate_config)),
        )
}

/// What a gate needs of `c.App.Config()`: four settings, as the Go server runs them.
///
/// Each is read after `SetDefaults` in Go, so none is nil there; the row was written by Go's
/// `Load` after its own `SetDefaults`, so none is absent here either, and the fallbacks are
/// Go's defaults for the one case — a row written by something else — where they would be.
struct LiveGates {
    site_url: String,
    enable_uploads: bool,
    import_directory: String,
    marketplace_url: String,
}

async fn live_gates(state: &AppState, where_: &str) -> Result<LiveGates, ApiError> {
    let live = mm_app::config::load_model_config(state.app.store().config())
        .await
        .map_err(|err| config_error(where_, &err))?;
    Ok(LiveGates {
        site_url: live.service_settings.site_url.unwrap_or_default(),
        enable_uploads: live.plugin_settings.enable_uploads.unwrap_or(false),
        import_directory: live
            .import_settings
            .directory
            .filter(|directory| !directory.is_empty())
            .unwrap_or_else(|| DEFAULT_IMPORT_DIRECTORY.to_owned()),
        marketplace_url: live
            .plugin_settings
            .marketplace_url
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_MARKETPLACE_URL.to_owned()),
    })
}

/// `json.NewDecoder(r.Body).Decode(&cfg)` with `err != nil || cfg == nil` as one 400 —
/// `SetInvalidParamWithErr("config", err)`. See the module docs for the three Go behaviours
/// this reproduces.
pub(crate) fn decode_config(bytes: &[u8]) -> Result<ConfigDocument, ApiError> {
    let first = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<serde_json::Value>()
        .next();
    let value = match first {
        Some(Ok(value)) => value,
        // A decode error, or `io.EOF` on an empty body — both `err != nil`.
        Some(Err(_)) | None => return Err(ApiError::invalid_param("config")),
    };
    // `null` leaves the `*Config` nil; any other non-object cannot unmarshal into a struct.
    // Checked here because serde will happily build a struct from an empty **array**.
    if !value.is_object() {
        return Err(ApiError::invalid_param("config"));
    }
    let value = without_null_members(value);
    serde_json::from_value(value).map_err(|_| ApiError::invalid_param("config"))
}

/// Drop every object member whose value is `null`, recursively — `encoding/json`'s "unmarshal
/// null into a non-pointer is a no-op, into a pointer is nil", which for a document whose every
/// leaf is a pointer and every branch a struct is exactly "as if the key were absent".
/// Array elements are left alone: a `null` inside `[]string` is `""` in Go, and no route here
/// reaches such a field before forwarding.
fn without_null_members(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(members) => serde_json::Value::Object(
            members
                .into_iter()
                .filter(|(_, member)| !member.is_null())
                .map(|(key, member)| (key, without_null_members(member)))
                .collect(),
        ),
        other => other,
    }
}

/// `*cfg.ServiceSettings.SiteURL` after `cfg.SetDefaults()` (config.go:509-515): the value sent,
/// or — when none was — `ServiceSettingsDefaultSiteURL` if `EnableDeveloper` is set and `""`
/// otherwise. `updateConfig` tests the defaulted value, so an omitted `SiteURL` is a cleared one
/// unless the same body enables developer mode.
fn site_url_after_defaults(cfg: &ConfigDocument) -> &str {
    match cfg.service_settings.site_url.as_deref() {
        Some(site_url) => site_url,
        None if cfg.service_settings.enable_developer == Some(true) => DEFAULT_SITE_URL,
        None => "",
    }
}

fn clear_site_url_error(where_: &str) -> ApiError {
    ApiError::from(AppError::new(
        where_,
        CLEAR_SITEURL_ERROR,
        None,
        String::new(),
        400,
    ))
}

/// `NewAppError("patchConfig", "…not_allowed_security…", map[string]any{"Name": name}, "", 403)`.
fn not_allowed_security_error(name: &str) -> ApiError {
    let mut params = std::collections::HashMap::new();
    params.insert(
        "Name".to_owned(),
        serde_json::Value::String(name.to_owned()),
    );
    ApiError::from(AppError::new(
        "patchConfig",
        NOT_ALLOWED_SECURITY_ERROR,
        Some(params),
        String::new(),
        403,
    ))
}

/// Read the whole body so the gates can decode it and the forward can resend it.
async fn read_body(
    request: Request,
) -> Result<(axum::http::request::Parts, axum::body::Bytes), ApiError> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the configuration body");
            ApiError::invalid_param("config")
        })?;
    Ok((parts, bytes))
}

/// The gates of `updateConfig` (api4/config.go:114) up to the merge, as an `Err` or the
/// go-ahead to forward. Shared with the socket wrapper, which supplies the local session.
async fn update_config_gates(
    state: &AppState,
    session: &Session,
    bytes: &[u8],
) -> Result<(), ApiError> {
    let cfg = decode_config(bytes)?;
    // `cfg.SetDefaults()` runs next in Go and cannot fail; its one observable effect on these
    // gates is the `SiteURL` default, taken in `site_url_after_defaults`.

    if !state
        .app
        .session_has_permission_to_any(session, SYSCONSOLE_WRITE_PERMISSIONS)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            session,
            SYSCONSOLE_WRITE_PERMISSIONS,
        )));
    }

    let live = live_gates(state, "updateConfig").await?;
    if !live.site_url.is_empty() && site_url_after_defaults(&cfg).is_empty() {
        return Err(clear_site_url_error("updateConfig"));
    }
    Ok(())
}

/// Port of `updateConfig` (api4/config.go:114): the decode, the permission and the cleared
/// `SiteURL` are served; the merge and everything after it is forwarded — see the module docs.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn update_config(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = update_config_gates(&state, &session.0, &bytes).await {
        tracing::Span::current().record("forwarded", false);
        return err.into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes))).await
}

/// The gates of `patchConfig` (api4/config.go:279) up to the merge.
///
/// Four refusals after the permission, in Go's order: a `SiteURL` sent as `""` while one is
/// set (400); `PluginSettings.EnableUploads` sent and different (403); `ImportSettings.Directory`
/// sent and different (403); `PluginSettings.MarketplaceURL` sent and different while the
/// patch's own `EnableUploads` — which by then equals the live value — is off (403).
/// `SignaturePublicKeyFiles` is silently preserved rather than refused, which the forward does.
/// A cloud licence adds a fifth refusal on `ComplianceSettings.Directory` (config.go:326); it
/// sits after these four and is Go's, since the request is forwarded past them either way.
async fn patch_config_gates(
    state: &AppState,
    session: &Session,
    bytes: &[u8],
) -> Result<(), ApiError> {
    let cfg = decode_config(bytes)?;

    if !state
        .app
        .session_has_permission_to_any(session, SYSCONSOLE_WRITE_PERMISSIONS)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            session,
            SYSCONSOLE_WRITE_PERMISSIONS,
        )));
    }

    let live = live_gates(state, "patchConfig").await?;

    // `cfg.ServiceSettings.SiteURL != nil && *cfg.ServiceSettings.SiteURL == ""` — no
    // `SetDefaults` on a patch, so only an explicit empty string is a clearing.
    if !live.site_url.is_empty() && cfg.service_settings.site_url.as_deref() == Some("") {
        return Err(clear_site_url_error("patchConfig"));
    }

    let enable_uploads = cfg.plugin_settings.enable_uploads;
    if enable_uploads.is_some_and(|sent| sent != live.enable_uploads) {
        return Err(not_allowed_security_error("PluginSettings.EnableUploads"));
    }

    if cfg
        .import_settings
        .directory
        .as_deref()
        .is_some_and(|sent| sent != live.import_directory)
    {
        return Err(not_allowed_security_error("ImportSettings.Directory"));
    }

    if let (Some(marketplace_url), Some(enable_uploads)) = (
        cfg.plugin_settings.marketplace_url.as_deref(),
        enable_uploads,
    ) && marketplace_url != live.marketplace_url
        && !enable_uploads
    {
        return Err(not_allowed_security_error("PluginSettings.MarketplaceURL"));
    }
    Ok(())
}

/// Port of `patchConfig` (api4/config.go:279): the decode, the permission and the four
/// refusals are served; the merge and the save are forwarded — see the module docs.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn patch_config(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = patch_config_gates(&state, &session.0, &bytes).await {
        tracing::Span::current().record("forwarded", false);
        return err.into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes))).await
}

/// Port of `configReload` (api4/config.go:96): `SessionHasPermissionToAndNotRestrictedAdmin
/// (PermissionReloadConfig)` is served; the reload itself is forwarded, because the copy that
/// needs reloading is the Go process's (module docs, reason 2). This process has nothing to
/// reload — it reads the row per request — so once the Go server is gone the forward becomes
/// `ReturnStatusOK` and nothing else.
///
/// Registered on both routers: `config_local.go:22` puts the same handler under `APILocal`,
/// where the local session is unrestricted and the permission passes by that alone.
async fn serve_reload(state: AppState, session: &Session, request: Request) -> Response {
    if !state
        .app
        .session_has_permission_to_and_not_restricted_admin(session, &PERMISSION_RELOAD_CONFIG)
        .await
    {
        tracing::Span::current().record("forwarded", false);
        return ApiError::from(make_permission_error(session, &[&PERMISSION_RELOAD_CONFIG]))
            .into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}

/// `POST /api/v4/config/reload` on the HTTP router — see [`serve_reload`].
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn reload_config(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    serve_reload(state, &session.0, request).await
}

// ---------------------------------------------------------------------------------------------
// `config_local.go`
// ---------------------------------------------------------------------------------------------

/// Port of `localUpdateConfig` (api4/config_local.go:53): the decode is the HTTP route's; there
/// is **no** permission and **no** `SiteURL` gate — `SetDefaults`, `HandleMessageExportConfig`,
/// `IsValid` and `SaveConfig` follow the decode directly, and all four are forwarded over the
/// socket.
#[tracing::instrument(skip_all, fields(forwarded))]
pub(crate) async fn local_update_config(
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = decode_config(&bytes) {
        tracing::Span::current().record("forwarded", false);
        return err.into_response();
    }
    tracing::Span::current().record("forwarded", true);
    crate::local::forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await
}

/// Port of `localPatchConfig` (api4/config_local.go:98): the decode, then a merge whose field
/// filter is `return true` — none of the HTTP patch's four refusals exist here — `IsValid`,
/// `SaveConfig`, and a response that is `GetSanitizedConfig()` rather than the read-filtered
/// merge. Everything after the decode is forwarded over the socket.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_patch_config(Extension(go): Extension<GoLocalSocket>, request: Request) -> Response {
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = decode_config(&bytes) {
        tracing::Span::current().record("forwarded", false);
        return err.into_response();
    }
    tracing::Span::current().record("forwarded", true);
    crate::local::forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await
}

/// `configReload` under `APILocal` (api4/config_local.go:22) — [`serve_reload`] with the local
/// session, whose `IsUnrestricted` short-circuits the permission. The forward goes over the
/// socket, because the request carries [`GoLocalSocket`].
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_reload_config(State(state): State<AppState>, request: Request) -> Response {
    serve_reload(state, &local_session().0, request).await
}

/// Port of `localMigrateConfig` (api4/config_local.go:167).
///
/// `model.StringInterfaceFromJSON(r.Body)` decodes one value into a `map[string]any` and
/// **discards the error**, so what the handler sees on a malformed body is whatever the decoder
/// managed to fill before it stopped — a map serde cannot reproduce, since it drops the lot.
/// A body that does not decode is therefore forwarded rather than answered ([D-026] describes the
/// same salvage on another route); a body that does is checked as Go checks it: `from` must be a string, then
/// `to`, each a `SetInvalidParam` 400. The permission after them is `manage_system`, which
/// the local session holds by being unrestricted, and `config.Migrate(from, to)` — two backing
/// stores opened from two DSNs and one copied into the other — is forwarded over the socket.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_migrate_config(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let decoded = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<serde_json::Value>()
        .next();
    let props = match decoded {
        Some(Ok(serde_json::Value::Object(props))) => props,
        // `null` and a non-object both leave `objmap` nil → an empty map → `from` is missing.
        Some(Ok(_)) => serde_json::Map::new(),
        // A decode error Go would have partially applied; only Go knows what it kept.
        Some(Err(_)) | None => {
            tracing::Span::current().record("forwarded", true);
            return crate::local::forward_over_unix(
                &go.0,
                Request::from_parts(parts, Body::from(bytes)),
            )
            .await;
        }
    };
    if let Err(err) = migrate_params(&props) {
        tracing::Span::current().record("forwarded", false);
        return err.into_response();
    }

    // `SessionHasPermissionTo(PermissionManageSystem)` — the local session is unrestricted, so
    // this is true by construction; it is evaluated rather than assumed so the handler reads
    // as Go's does.
    let session = local_session().0;
    if !state
        .app
        .session_has_permission_to(&session, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        tracing::Span::current().record("forwarded", false);
        return ApiError::from(make_permission_error(
            &session,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    tracing::Span::current().record("forwarded", true);
    crate::local::forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await
}

/// `props["from"].(string)` then `props["to"].(string)`, in that order — a missing `to` is
/// not reported while `from` is wrong.
fn migrate_params(props: &serde_json::Map<String, serde_json::Value>) -> Result<(), ApiError> {
    for name in ["from", "to"] {
        if !props.get(name).is_some_and(serde_json::Value::is_string) {
            return Err(ApiError::invalid_param(name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three shapes `Decode(&cfg)` accepts or refuses, without a server.
    #[test]
    fn decode_reads_one_value_and_refuses_null_and_garbage() {
        assert!(decode_config(b"").is_err(), "EOF is a decode error");
        assert!(
            decode_config(b"null").is_err(),
            "a nil *Config is the same 400"
        );
        assert!(decode_config(b"[]").is_err());
        assert!(decode_config(b"{").is_err());
        assert!(
            decode_config(b"{} trailing garbage").is_ok(),
            "one value, then stop"
        );
        let cfg = decode_config(br#"{"ServiceSettings":{"SiteURL":"http://x"}}"#).unwrap();
        assert_eq!(cfg.service_settings.site_url.as_deref(), Some("http://x"));
        let err = decode_config(b"").unwrap_err();
        assert_eq!(err.0.id, "api.context.invalid_body_param.app_error");
        assert_eq!(err.0.params.as_ref().unwrap()["Name"], "config");
    }

    /// `null` is a no-op for a struct section and nil for a setting, as in Go.
    #[test]
    fn null_members_are_absent_members() {
        let cfg = decode_config(
            br#"{"ServiceSettings":null,"TeamSettings":{"SiteName":null,"MaxUsersPerTeam":null}}"#,
        )
        .unwrap();
        assert_eq!(cfg.service_settings, Default::default());
        assert_eq!(cfg.team_settings.site_name, None);
        assert_eq!(cfg.team_settings.max_users_per_team, None);
    }

    /// config.go:509-515 — the one `SetDefaults` branch these gates can see.
    #[test]
    fn an_omitted_site_url_defaults_to_empty_unless_developer_mode_is_enabled() {
        let cfg = decode_config(b"{}").unwrap();
        assert_eq!(site_url_after_defaults(&cfg), "");
        let cfg = decode_config(br#"{"ServiceSettings":{"EnableDeveloper":true}}"#).unwrap();
        assert_eq!(site_url_after_defaults(&cfg), DEFAULT_SITE_URL);
        let cfg = decode_config(br#"{"ServiceSettings":{"EnableDeveloper":false}}"#).unwrap();
        assert_eq!(site_url_after_defaults(&cfg), "");
        let cfg =
            decode_config(br#"{"ServiceSettings":{"EnableDeveloper":true,"SiteURL":""}}"#).unwrap();
        assert_eq!(
            site_url_after_defaults(&cfg),
            "",
            "a sent value is never defaulted"
        );
    }

    /// `from` before `to`, and a non-string is as missing as an absent key.
    #[test]
    fn migrate_checks_from_then_to_as_strings() {
        let props = |raw: &str| serde_json::from_str::<serde_json::Map<_, _>>(raw).unwrap();
        let name = |err: ApiError| err.0.params.unwrap()["Name"].as_str().unwrap().to_owned();
        assert_eq!(name(migrate_params(&props("{}")).unwrap_err()), "from");
        assert_eq!(
            name(migrate_params(&props(r#"{"to":"b"}"#)).unwrap_err()),
            "from"
        );
        assert_eq!(
            name(migrate_params(&props(r#"{"from":1,"to":"b"}"#)).unwrap_err()),
            "from"
        );
        assert_eq!(
            name(migrate_params(&props(r#"{"from":"a"}"#)).unwrap_err()),
            "to"
        );
        assert!(migrate_params(&props(r#"{"from":"a","to":"b"}"#)).is_ok());
    }

    /// The 403's `params.Name` is the setting's Go path, which the webapp displays.
    #[test]
    fn the_security_refusal_names_the_setting() {
        let err = not_allowed_security_error("ImportSettings.Directory");
        assert_eq!(err.0.status_code, 403);
        assert_eq!(err.0.id, NOT_ALLOWED_SECURITY_ERROR);
        assert_eq!(err.0.params.unwrap()["Name"], "ImportSettings.Directory");
    }
}
