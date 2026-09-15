//! The certificate and enterprise-gate routes of `api4/saml.go`, `api4/ldap.go` and
//! `api4/audit_logging.go` — 22 HTTP pairs. Their local-mode twins are in
//! [`crate::local_auth_certs`].
//!
//! # What this build answers itself, and what it hands to Go
//!
//! `App.Saml()`, `App.Ldap()` and `App.LdapDiagnostic()` are nil on every build of this tree
//! (see `mm_app::auth_certs`), so a route that ends in one of them ends in a constant, and the
//! whole of it is the gate in front of that constant — served here, in Go's order:
//!
//! | route | order of checks |
//! |---|---|
//! | `POST /ldap/sync`, `/test`, `/test_connection`, `/test_diagnostics` | **licence** (501 `api.ldap_groups.license_error`), then permission (403), then the body |
//! | `POST /ldap/migrateid` | **body** (`toAttribute`, 400), then permission, then licence, then the nil 501 |
//! | `POST /saml/reset_auth_data` | permission, then the body (400 `model.utils.decode_json.app_error`), then the nil 501 |
//! | `POST /saml/metadatafromidp` | permission — on whatever session is present, this is `APIHandler` — then `saml_metadata_url` (400), then the nil 501 **wrapped as a 400** |
//! | `GET /saml/certificate/status` | permission, then three `HasFile` reads |
//! | `POST /ldap/users/{user_id}/group_sync_memberships` | permission, then the user (404), then the auth service (400) — no `RequireUserId`, so `me` is a 404 |
//!
//! The order is on the wire: a plain user posting to `/ldap/sync` on an unlicensed server gets
//! the 501, not the 403; the same user posting `{}` to `/ldap/migrateid` gets the 400. Each order
//! is pinned by a parity row.
//!
//! # Every certificate write forwards after its gate
//!
//! The six adds and the six removes are `SetConfigFile`/`RemoveConfigFile` followed by
//! `UpdateConfig`, and the configuration-document write is not this server's to make (this
//! process's configuration is fixed at construction, and the write belongs to the config family
//! — [D-660]). So each add serves the permission and the multipart parse (the 403 and every 400)
//! and forwards a request that would write; each remove serves the permission and forwards. The
//! forward is decided before anything is written, and the body is re-attached whole. The
//! `application/x-pem-file` arm of `addSamlIdpCertificate` forwards after the permission and the
//! `Content-Type` checks, since `SetSamlIdpCertificateFromMetadata` is a write on any input.
//!
//! # Three parsers, two error ids
//!
//! `parseSamlCertificateRequest` (saml.go:54) and `parseAuditLogCertificateRequest`
//! (audit_logging.go:18) answer a body that is not multipart with `no_file`;
//! `parseLdapCertificateRequest` (ldap.go:399) answers it with `parseform`. The audit parser also
//! refuses **two** `certificate` parts (`multiple_files`) where the other two take the first.
//! Measured, all three, before this module existed.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path as UrlPath, RawQuery, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mm_model::config::LdapSettings;
use mm_model::go_json::{GoFields, remap_object_keys};
use mm_model::ldap::{LdapDiagnosticTestType, USER_AUTH_SERVICE_LDAP};
use mm_model::permission::{
    PERMISSION_ADD_LDAP_PRIVATE_CERT, PERMISSION_ADD_LDAP_PUBLIC_CERT,
    PERMISSION_ADD_SAML_IDP_CERT, PERMISSION_ADD_SAML_PRIVATE_CERT,
    PERMISSION_ADD_SAML_PUBLIC_CERT, PERMISSION_CREATE_LDAP_SYNC_JOB,
    PERMISSION_GET_SAML_CERT_STATUS, PERMISSION_GET_SAML_METADATA_FROM_IDP,
    PERMISSION_MANAGE_SYSTEM, PERMISSION_REMOVE_LDAP_PRIVATE_CERT,
    PERMISSION_REMOVE_LDAP_PUBLIC_CERT, PERMISSION_REMOVE_SAML_IDP_CERT,
    PERMISSION_REMOVE_SAML_PRIVATE_CERT, PERMISSION_REMOVE_SAML_PUBLIC_CERT,
    PERMISSION_SYSCONSOLE_WRITE_EXPERIMENTAL_FEATURES,
    PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS, PERMISSION_TEST_LDAP, Permission,
    make_permission_error,
};
use mm_model::saml::USER_AUTH_SERVICE_SAML;
use mm_model::session::Session;
use mm_model::utils::{AppError, go_json_marshal};
use serde::Deserialize;

use crate::auth::AuthenticatedSession;
use crate::auth_writes::OptionalSession;
use crate::channels::query_first;
use crate::error::ApiError;
use crate::{AppState, partially_migrated, partially_migrated_with_ids, proxy};

/// The registrations of `InitSaml` (api4/saml.go:17), `InitLdap` (api4/ldap.go:22) and
/// `InitAuditLogging` (api4/audit_logging.go:13) that this module serves, merged into the main
/// router. `GET /saml/metadata` and the three `/ldap/groups` routes belong to other modules and
/// are not touched here.
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/v4/saml/certificate/public",
            partially_migrated(
                post(add_saml_public_certificate).delete(remove_saml_public_certificate),
            ),
        )
        .route(
            "/api/v4/saml/certificate/private",
            partially_migrated(
                post(add_saml_private_certificate).delete(remove_saml_private_certificate),
            ),
        )
        .route(
            "/api/v4/saml/certificate/idp",
            partially_migrated(post(add_saml_idp_certificate).delete(remove_saml_idp_certificate)),
        )
        .route(
            "/api/v4/saml/certificate/status",
            partially_migrated(get(get_saml_certificate_status)),
        )
        // `APIHandler`, not `APISessionRequired`: no session is still a request, and the
        // permission check then fails on the empty session — a 403, not a 401.
        .route(
            "/api/v4/saml/metadatafromidp",
            partially_migrated(post(get_saml_metadata_from_idp)),
        )
        .route(
            "/api/v4/saml/reset_auth_data",
            partially_migrated(post(reset_auth_data_to_email)),
        )
        .route("/api/v4/ldap/sync", partially_migrated(post(sync_ldap)))
        .route("/api/v4/ldap/test", partially_migrated(post(test_ldap)))
        .route(
            "/api/v4/ldap/test_connection",
            partially_migrated(post(test_ldap_connection)),
        )
        .route(
            "/api/v4/ldap/test_diagnostics",
            partially_migrated(post(test_ldap_diagnostics)),
        )
        .route(
            "/api/v4/ldap/migrateid",
            partially_migrated(post(migrate_id_ldap)),
        )
        .route(
            "/api/v4/ldap/certificate/public",
            partially_migrated(
                post(add_ldap_public_certificate).delete(remove_ldap_public_certificate),
            ),
        )
        .route(
            "/api/v4/ldap/certificate/private",
            partially_migrated(
                post(add_ldap_private_certificate).delete(remove_ldap_private_certificate),
            ),
        )
        // `{user_id:[A-Za-z0-9]+}` — the mux charset applies, `RequireUserId` does not.
        .route(
            "/api/v4/ldap/users/{user_id}/group_sync_memberships",
            partially_migrated_with_ids(state, post(add_user_to_group_syncables)),
        )
        .route(
            "/api/v4/audit_logs/certificate",
            partially_migrated(
                post(add_audit_log_certificate).delete(remove_audit_log_certificate),
            ),
        )
}

// ---------------------------------------------------------------------------------------------
// shared pieces
// ---------------------------------------------------------------------------------------------

/// Port of `web.ReturnStatusOK` (web/web.go:127): `{"status":"OK"}` through `w.Write`, so **no**
/// trailing newline — unlike the encoder-written bodies below.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE.as_str(), "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}

/// `json.NewEncoder(w).Encode(v)`: Go's marshal, then the newline the encoder appends.
fn encoded<T: serde::Serialize>(where_: &str, value: &T) -> Response {
    match go_json_marshal(value) {
        Ok(mut body) => {
            body.push('\n');
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE.as_str(), "application/json"),
                    ("x-mmrs-served-by", "rust"),
                ],
                body,
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to encode the response");
            ApiError::from(AppError::new(
                where_,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// `c.SetPermissionError(p)` — 403 with the session's user id in the detail.
fn permission_error(session: &Session, permission: &Permission) -> ApiError {
    ApiError::from(make_permission_error(session, &[permission]))
}

/// `model.NewAppError(where, id, nil, "", status)`.
fn app_error(where_: &str, id: &str, status: i32) -> ApiError {
    ApiError::from(AppError::new(where_, id, None, String::new(), status))
}

/// `c.App.Channels().License() == nil || !*License().Features.LDAP` — the 501 that opens every
/// `/ldap` handler except `migrateIDLdap`, where it comes third. `None` when the gate is passed;
/// a licence read that fails is the 500 it is, since Go cannot fail here at all.
async fn ldap_license_gate(state: &AppState, where_: &str) -> Option<ApiError> {
    match state.app.license().await {
        Ok(license)
            if license
                .as_deref()
                .is_some_and(mm_app::auth_certs::license_has_ldap) =>
        {
            None
        }
        Ok(_) => Some(app_error(where_, "api.ldap_groups.license_error", 501)),
        Err(err) => Some(ApiError::from(err)),
    }
}

/// The request body, whole. Every route here either parses it or forwards it, and a body that
/// cannot be read is answered as the caller's own parse failure, which is what Go's decoder or
/// `ParseMultipartForm` would report for a body that ends early.
async fn read_body(body: axum::body::Body) -> Result<axum::body::Bytes, ()> {
    axum::body::to_bytes(body, usize::MAX).await.map_err(|err| {
        tracing::warn!(error = %err, "could not read the request body");
    })
}

/// Which of the three certificate parsers is speaking — they share a shape and differ in two
/// error ids and one extra refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CertificateParser {
    /// `parseSamlCertificateRequest` (api4/saml.go:54).
    Saml,
    /// `parseLdapCertificateRequest` (api4/ldap.go:399).
    Ldap,
    /// `parseAuditLogCertificateRequest` (api4/audit_logging.go:18).
    Audit,
}

impl CertificateParser {
    fn where_(self) -> &'static str {
        match self {
            CertificateParser::Saml => "addSamlCertificate",
            CertificateParser::Ldap => "addLdapCertificate",
            CertificateParser::Audit => "addAuditLogCertificate",
        }
    }

    /// The three parsers, as one. `Ok` carries the part's filename — the only thing the handlers
    /// read off it (for the audit record) before handing the write to Go.
    ///
    /// `ParseMultipartForm(maxFileSize)` takes a memory budget, not a limit: a body over it goes
    /// to a temporary file rather than being refused, so `MaxFileSize` decides nothing on the
    /// wire and is not consulted.
    fn parse(self, content_type: Option<&str>, body: &[u8]) -> Result<String, ApiError> {
        let form = match crate::multipart::parse_form(content_type, body) {
            Ok(form) => form,
            Err(err) => {
                tracing::debug!(error = %err, "the certificate body is not multipart/form-data");
                let id = match self {
                    CertificateParser::Ldap => "api.admin.add_certificate.parseform.app_error",
                    CertificateParser::Saml | CertificateParser::Audit => {
                        "api.admin.add_certificate.no_file.app_error"
                    }
                };
                return Err(app_error(self.where_(), id, 400));
            }
        };
        let files = form.file.get("certificate").map(Vec::as_slice);
        match self {
            CertificateParser::Audit => match files {
                None | Some(&[]) => Err(app_error(
                    self.where_(),
                    "api.admin.add_certificate.no_file.app_error",
                    400,
                )),
                Some(files) if files.len() > 1 => Err(app_error(
                    self.where_(),
                    "api.admin.add_certificate.multiple_files.app_error",
                    400,
                )),
                Some(files) => Ok(files[0].filename.clone()),
            },
            CertificateParser::Saml | CertificateParser::Ldap => match files {
                None => Err(app_error(
                    self.where_(),
                    "api.admin.add_certificate.no_file.app_error",
                    400,
                )),
                // `len(fileArray) <= 0` — unreachable through `readForm`, which never stores an
                // empty list, and kept because the id is Go's.
                Some(&[]) => Err(app_error(
                    self.where_(),
                    "api.admin.add_certificate.array.app_error",
                    400,
                )),
                Some(files) => Ok(files[0].filename.clone()),
            },
        }
    }
}

/// `add*Certificate`: the permission, the parse, then the forward. Shared by all five multipart
/// adds; the permission and the parser are what differ.
#[tracing::instrument(skip_all, fields(filename, forwarded))]
async fn add_certificate(
    state: AppState,
    session: &Session,
    permission: &Permission,
    parser: CertificateParser,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    if !state
        .app
        .session_has_permission_to(session, permission)
        .await
    {
        return permission_error(session, permission).into_response();
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = read_body(body).await else {
        // `ParseMultipartForm` reports a body it cannot read as its own error.
        return parser
            .parse(None, &[])
            .err()
            .map_or_else(status_ok, IntoResponse::into_response);
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    match parser.parse(content_type, &bytes) {
        Ok(filename) => tracing::Span::current().record("filename", filename.as_str()),
        Err(refusal) => return refusal.into_response(),
    };
    tracing::Span::current().record("forwarded", true);
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    proxy::forward_to_go(State(state), request).await
}

/// `remove*Certificate`: the permission, then the forward — the removal is a configuration write.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn remove_certificate(
    state: AppState,
    session: &Session,
    permission: &Permission,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    if !state
        .app
        .session_has_permission_to(session, permission)
        .await
    {
        return permission_error(session, permission).into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}

// ---------------------------------------------------------------------------------------------
// api4/saml.go
// ---------------------------------------------------------------------------------------------

/// Port of `addSamlPublicCertificate` (api4/saml.go:75).
pub async fn add_saml_public_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    add_certificate(
        state,
        &session.0,
        &PERMISSION_ADD_SAML_PUBLIC_CERT,
        CertificateParser::Saml,
        request,
    )
    .await
}

/// Port of `addSamlPrivateCertificate` (api4/saml.go:99).
pub async fn add_saml_private_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    add_certificate(
        state,
        &session.0,
        &PERMISSION_ADD_SAML_PRIVATE_CERT,
        CertificateParser::Saml,
        request,
    )
    .await
}

/// Port of `addSamlIdpCertificate` (api4/saml.go:123) — the one add that branches on
/// `Content-Type`.
///
/// `r.Header.Get("Content-Type")` is `""` for an absent header and for an empty one, and both
/// are the `missing_content_type` 400. `mime.ParseMediaType` failing is `invalid_content_type`;
/// so is any media type other than the two arms — `text/plain` and a bare token alike. The
/// `application/x-pem-file` arm is forwarded whole: `SetSamlIdpCertificateFromMetadata` writes
/// the file and the configuration on any body it can parse, and panics on one it cannot, and
/// neither is this server's to reproduce.
#[tracing::instrument(skip_all, fields(media_type, forwarded))]
pub async fn add_saml_idp_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    let permission = &PERMISSION_ADD_SAML_IDP_CERT;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }

    let raw = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if raw.is_empty() {
        return app_error(
            "addSamlIdpCertificate",
            "api.admin.saml.set_certificate_from_metadata.missing_content_type.app_error",
            400,
        )
        .into_response();
    }
    let Some((media_type, _)) = crate::multipart::parse_media_type(&raw) else {
        return app_error(
            "addSamlIdpCertificate",
            "api.admin.saml.set_certificate_from_metadata.invalid_content_type.app_error",
            400,
        )
        .into_response();
    };
    tracing::Span::current().record("media_type", media_type.as_str());

    match media_type.as_str() {
        "application/x-pem-file" => {
            tracing::Span::current().record("forwarded", true);
            proxy::forward_to_go(State(state), request).await
        }
        "multipart/form-data" => {
            let (parts, body) = request.into_parts();
            let Ok(bytes) = read_body(body).await else {
                return CertificateParser::Saml
                    .parse(None, &[])
                    .err()
                    .map_or_else(status_ok, IntoResponse::into_response);
            };
            if let Err(refusal) = CertificateParser::Saml.parse(Some(&raw), &bytes) {
                return refusal.into_response();
            }
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
        _ => app_error(
            "addSamlIdpCertificate",
            "api.admin.saml.set_certificate_from_metadata.invalid_content_type.app_error",
            400,
        )
        .into_response(),
    }
}

/// Port of `removeSamlPublicCertificate` (api4/saml.go:173).
pub async fn remove_saml_public_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    remove_certificate(
        state,
        &session.0,
        &PERMISSION_REMOVE_SAML_PUBLIC_CERT,
        request,
    )
    .await
}

/// Port of `removeSamlPrivateCertificate` (api4/saml.go:191).
pub async fn remove_saml_private_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    remove_certificate(
        state,
        &session.0,
        &PERMISSION_REMOVE_SAML_PRIVATE_CERT,
        request,
    )
    .await
}

/// Port of `removeSamlIdpCertificate` (api4/saml.go:209).
pub async fn remove_saml_idp_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    remove_certificate(state, &session.0, &PERMISSION_REMOVE_SAML_IDP_CERT, request).await
}

/// Port of `getSamlCertificateStatus` (api4/saml.go:227): the permission, then the three
/// booleans, encoder-written.
#[tracing::instrument(skip_all)]
pub async fn get_saml_certificate_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    let permission = &PERMISSION_GET_SAML_CERT_STATUS;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    match state.app.get_saml_certificate_status().await {
        Ok(status) => encoded("getSamlCertificateStatus", &status),
        Err(err) => crate::config::config_error("getSamlCertificateStatus", &err).into_response(),
    }
}

/// Port of `model.MapFromJSON` (utils.go:507), the way it actually behaves: the decode error is
/// discarded and whatever `objmap` holds is returned. `encoding/json` fills every string-valued
/// key of an object even when a sibling's type is wrong — it "completes the unmarshaling as best
/// it can" and reports the first mismatch afterwards — so `{"saml_metadata_url":"x","n":5}`
/// carries the URL. A body that is not an object, or is not JSON, leaves `objmap` nil: empty.
/// Only the **first** JSON value is read, as `Decoder.Decode` reads one.
fn map_from_json(bytes: &[u8]) -> HashMap<String, String> {
    let Some(Ok(serde_json::Value::Object(object))) = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<serde_json::Value>()
        .next()
    else {
        return HashMap::new();
    };
    object
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(text) => Some((key, text)),
            _ => None,
        })
        .collect()
}

/// Port of `model.StringInterfaceFromJSON` (utils.go:590), the same shape for `map[string]any`
/// — every value fits, so the first JSON value is the whole map or nothing.
fn string_interface_from_json(bytes: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::Deserializer::from_slice(bytes)
        .into_iter::<serde_json::Value>()
        .next()
    {
        Some(Ok(serde_json::Value::Object(object))) => object,
        _ => serde_json::Map::new(),
    }
}

/// Port of `getSamlMetadataFromIdp` (api4/saml.go:238).
///
/// `APIHandler`: the session is optional, and `SessionHasPermissionTo` on the zero-valued session
/// a missing token leaves behind is `false`, so an anonymous caller is a 403 rather than a 401.
/// The nil-interface 501 from the app is wrapped in the handler's own 400
/// (`failure_get_metadata_from_idp`), which is the answer for any URL at all on this build — the
/// `https://` prefixing and the fetch sit behind the nil check.
#[tracing::instrument(skip_all)]
pub async fn get_saml_metadata_from_idp(
    State(state): State<AppState>,
    session: OptionalSession,
    request: Request,
) -> Response {
    let session = session.0.unwrap_or_default();
    let permission = &PERMISSION_GET_SAML_METADATA_FROM_IDP;
    if !state
        .app
        .session_has_permission_to(&session, permission)
        .await
    {
        return permission_error(&session, permission).into_response();
    }
    let Ok(bytes) = read_body(request.into_body()).await else {
        return ApiError::invalid_param("saml_metadata_url").into_response();
    };
    let props = map_from_json(&bytes);
    let url = props
        .get("saml_metadata_url")
        .map(String::as_str)
        .unwrap_or_default();
    if url.is_empty() {
        return ApiError::invalid_param("saml_metadata_url").into_response();
    }
    match state.app.get_saml_metadata_from_idp(url) {
        Ok(metadata) => encoded("getSamlMetadataFromIdp", &metadata),
        Err(err) => {
            tracing::debug!(error = %err.id, "GetSamlMetadataFromIdp refused");
            app_error(
                "getSamlMetadataFromIdp",
                "api.admin.saml.failure_get_metadata_from_idp.app_error",
                400,
            )
            .into_response()
        }
    }
}

/// The handler-local `ResetAuthDataParams` (api4/saml.go:282). Every field is optional because
/// `encoding/json` treats a JSON `null` as "leave it" for a `bool` and a slice alike, and a
/// missing key the same way; the wire never sees the values on this build.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ResetAuthDataParams {
    include_deleted: Option<bool>,
    dry_run: Option<bool>,
    user_ids: Option<Vec<Option<String>>>,
}

/// The `json:` names of [`ResetAuthDataParams`], for the case-insensitive key match
/// `encoding/json` makes and serde does not (see [D-040]).
static RESET_AUTH_DATA_FIELDS: GoFields = GoFields {
    names: &["include_deleted", "dry_run", "user_ids"],
    nested: &[],
};

/// The 33 `json:` names of `model.LdapSettings` (config.go:2680), in declaration order — the
/// struct has no tags, so the names are the field names — for the same key fold.
static LDAP_SETTINGS_FIELDS: GoFields = GoFields {
    names: &[
        "Enable",
        "EnableSync",
        "LdapServer",
        "LdapPort",
        "ConnectionSecurity",
        "BaseDN",
        "BindUsername",
        "BindPassword",
        "MaximumLoginAttempts",
        "UserFilter",
        "GroupFilter",
        "GuestFilter",
        "EnableAdminFilter",
        "AdminFilter",
        "GroupDisplayNameAttribute",
        "GroupIdAttribute",
        "FirstNameAttribute",
        "LastNameAttribute",
        "EmailAttribute",
        "UsernameAttribute",
        "NicknameAttribute",
        "IdAttribute",
        "PositionAttribute",
        "LoginIdAttribute",
        "PictureAttribute",
        "SyncIntervalMinutes",
        "ReAddRemovedMembers",
        "SkipCertificateVerification",
        "PublicCertificateFile",
        "PrivateKeyFile",
        "QueryTimeout",
        "MaxPageSize",
        "LoginFieldName",
    ],
    nested: &[],
};

/// `json.NewDecoder(r.Body).Decode(&v)`, up to the point the target type matters: the **first**
/// JSON value in the body, its object keys folded the way `encoding/json` folds them. `None` is
/// an empty body or a syntax error — `Decode`'s `io.EOF` and `SyntaxError`.
fn decode_first_value(bytes: &[u8], schema: &GoFields) -> Option<serde_json::Value> {
    let mut value = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    remap_object_keys(&mut value, schema);
    Some(value)
}

/// `Decode(&settings)` into a non-pointer `model.LdapSettings`: a JSON `null` is a no-op that
/// leaves the zero value, and every other value must be an object whose members fit.
fn decode_ldap_settings(bytes: &[u8]) -> Option<LdapSettings> {
    match decode_first_value(bytes, &LDAP_SETTINGS_FIELDS)? {
        serde_json::Value::Null => Some(LdapSettings::default()),
        // Only an object fits a struct in Go; serde would also read a *sequence* into one,
        // positionally, and `[]` would then be a settings block rather than the 400 it is.
        value @ serde_json::Value::Object(_) => serde_json::from_value(value).ok(),
        _ => None,
    }
}

/// Port of `resetAuthDataToEmail` (api4/saml.go:270), on the HTTP and the local router alike.
///
/// `Decode(&params)` into a `*ResetAuthDataParams`: a `null` leaves the pointer nil, and the
/// handler refuses that with the same 400 as a body that does not decode. Past the body the
/// answer is the nil-interface 501; `num_affected` is never written on this build.
#[tracing::instrument(skip_all)]
pub async fn reset_auth_data_to_email(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let permission = &PERMISSION_MANAGE_SYSTEM;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    let decode_error = || {
        app_error(
            "resetAuthDataToEmail",
            "model.utils.decode_json.app_error",
            400,
        )
    };
    let Ok(bytes) = read_body(request.into_body()).await else {
        return decode_error().into_response();
    };
    let params: ResetAuthDataParams = match decode_first_value(&bytes, &RESET_AUTH_DATA_FIELDS) {
        // An object, or nothing: `null` leaves the pointer nil, and any other shape is the
        // decoder's type error (serde would read a sequence positionally — see
        // `decode_ldap_settings`).
        Some(value @ serde_json::Value::Object(_)) => match serde_json::from_value(value) {
            Ok(params) => params,
            Err(_) => return decode_error().into_response(),
        },
        _ => return decode_error().into_response(),
    };
    let user_ids: Vec<String> = params
        .user_ids
        .unwrap_or_default()
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect();
    match state.app.reset_saml_auth_data_to_email(
        params.include_deleted.unwrap_or(false),
        params.dry_run.unwrap_or(false),
        &user_ids,
    ) {
        Ok(num_affected) => encoded(
            "resetAuthDataToEmail",
            &serde_json::json!({ "num_affected": num_affected }),
        ),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// api4/ldap.go
// ---------------------------------------------------------------------------------------------

/// Port of `syncLdap` (api4/ldap.go:48): licence, permission, then the job that is never
/// started — `{"status":"OK"}` either way, since `SyncLdap` runs in a goroutine.
#[tracing::instrument(skip_all)]
pub async fn sync_ldap(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    _request: Request,
) -> Response {
    if let Some(refusal) = ldap_license_gate(&state, "api4.syncLdap").await {
        return refusal.into_response();
    }
    let permission = &PERMISSION_CREATE_LDAP_SYNC_JOB;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    state.app.sync_ldap().await;
    status_ok()
}

/// Port of `testLdap` (api4/ldap.go:67): licence, permission, then the nil-interface 501.
#[tracing::instrument(skip_all)]
pub async fn test_ldap(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    _request: Request,
) -> Response {
    if let Some(refusal) = ldap_license_gate(&state, "api4.testLdap").await {
        return refusal.into_response();
    }
    let permission = &PERMISSION_TEST_LDAP;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    match state.app.test_ldap() {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `testLdapConnection` (api4/ldap.go:86): licence, permission, the settings body
/// (`ldap_settings`, 400), then the nil-interface 501.
#[tracing::instrument(skip_all)]
pub async fn test_ldap_connection(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Some(refusal) = ldap_license_gate(&state, "api4.testLdapConnection").await {
        return refusal.into_response();
    }
    let permission = &PERMISSION_TEST_LDAP;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    let Ok(bytes) = read_body(request.into_body()).await else {
        return ApiError::invalid_param("ldap_settings").into_response();
    };
    let Some(settings) = decode_ldap_settings(&bytes) else {
        return ApiError::invalid_param("ldap_settings").into_response();
    };
    match state.app.test_ldap_connection(&settings) {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `testLdapDiagnostics` (api4/ldap.go:105): licence, permission, `?test=` (empty or
/// not one of the three names, 400), the settings body (400), then the nil-interface 501. Note
/// the `where` of its licence error is `Api4.` with a capital, alone among the four.
#[tracing::instrument(skip_all, fields(test))]
pub async fn test_ldap_diagnostics(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    if let Some(refusal) = ldap_license_gate(&state, "Api4.testLdapDiagnostics").await {
        return refusal.into_response();
    }
    let permission = &PERMISSION_TEST_LDAP;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    let test = query_first(query.as_deref(), "test").unwrap_or_default();
    if test.is_empty() {
        return ApiError::invalid_param("test").into_response();
    }
    tracing::Span::current().record("test", test.as_str());
    let test_type = LdapDiagnosticTestType(test);
    if !test_type.is_valid() {
        return ApiError::invalid_param("test").into_response();
    }
    let Ok(bytes) = read_body(request.into_body()).await else {
        return ApiError::invalid_param("ldap_settings").into_response();
    };
    let Some(settings) = decode_ldap_settings(&bytes) else {
        return ApiError::invalid_param("ldap_settings").into_response();
    };
    match state.app.test_ldap_diagnostics(&test_type, &settings) {
        Ok(results) => encoded("testLdapDiagnostics", &results),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `migrateIDLdap` (api4/ldap.go:337), on the HTTP and the local router alike.
///
/// The body comes **first**: `toAttribute` must be a non-empty JSON string (a number is `!ok`),
/// and its absence is a 400 before the permission is looked at. Then the permission, then the
/// licence, then the nil-interface 501 — three refusals in the reverse of the other `/ldap`
/// handlers' order.
#[tracing::instrument(skip_all, fields(to_attribute))]
pub async fn migrate_id_ldap(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let Ok(bytes) = read_body(request.into_body()).await else {
        return ApiError::invalid_param("toAttribute").into_response();
    };
    let props = string_interface_from_json(&bytes);
    let to_attribute = match props.get("toAttribute") {
        Some(serde_json::Value::String(text)) if !text.is_empty() => text.as_str(),
        _ => return ApiError::invalid_param("toAttribute").into_response(),
    };
    tracing::Span::current().record("to_attribute", to_attribute);
    let permission = &PERMISSION_MANAGE_SYSTEM;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    if let Some(refusal) = ldap_license_gate(&state, "api4.idMigrateLdap").await {
        return refusal.into_response();
    }
    match state.app.migrate_id_ldap(to_attribute) {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `addLdapPublicCertificate` (api4/ldap.go:420).
pub async fn add_ldap_public_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    add_certificate(
        state,
        &session.0,
        &PERMISSION_ADD_LDAP_PUBLIC_CERT,
        CertificateParser::Ldap,
        request,
    )
    .await
}

/// Port of `addLdapPrivateCertificate` (api4/ldap.go:444).
pub async fn add_ldap_private_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    add_certificate(
        state,
        &session.0,
        &PERMISSION_ADD_LDAP_PRIVATE_CERT,
        CertificateParser::Ldap,
        request,
    )
    .await
}

/// Port of `removeLdapPublicCertificate` (api4/ldap.go:468).
pub async fn remove_ldap_public_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    remove_certificate(
        state,
        &session.0,
        &PERMISSION_REMOVE_LDAP_PUBLIC_CERT,
        request,
    )
    .await
}

/// Port of `removeLdapPrivateCertificate` (api4/ldap.go:486).
pub async fn remove_ldap_private_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    remove_certificate(
        state,
        &session.0,
        &PERMISSION_REMOVE_LDAP_PRIVATE_CERT,
        request,
    )
    .await
}

/// Port of `addUserToGroupSyncables` (api4/ldap.go:506), up to the write.
///
/// Permission, then `GetUser(c.Params.UserId)` as sent — there is no `RequireUserId`, so `me`
/// is a user whose id is two bytes, a 404 — then the auth-service rule: an LDAP user passes, a
/// SAML user passes only with `SamlSettings.EnableSyncWithLdap` on, anyone else is the 400. Past
/// that, `CreateDefaultMemberships` is unported ([D-661]) and the request is forwarded, so the
/// rows are Go's.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded))]
pub async fn add_user_to_group_syncables(
    State(state): State<AppState>,
    UrlPath(user_id): UrlPath<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    let permission = &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS;
    if !state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        return permission_error(&session.0, permission).into_response();
    }
    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if user.auth_service != USER_AUTH_SERVICE_LDAP
        && (user.auth_service != USER_AUTH_SERVICE_SAML
            || !state.app.config().saml_enable_sync_with_ldap)
    {
        return app_error(
            "addUserToGroupSyncables",
            "api.user.add_user_to_group_syncables.not_ldap_user.app_error",
            400,
        )
        .into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}

// ---------------------------------------------------------------------------------------------
// api4/audit_logging.go
// ---------------------------------------------------------------------------------------------

/// Port of `addAuditLogCertificate` (api4/audit_logging.go:38).
pub async fn add_audit_log_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    add_certificate(
        state,
        &session.0,
        &PERMISSION_SYSCONSOLE_WRITE_EXPERIMENTAL_FEATURES,
        CertificateParser::Audit,
        request,
    )
    .await
}

/// Port of `removeAuditLogCertificate` (api4/audit_logging.go:63).
pub async fn remove_audit_log_certificate(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    remove_certificate(
        state,
        &session.0,
        &PERMISSION_SYSCONSOLE_WRITE_EXPERIMENTAL_FEATURES,
        request,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multipart(parts: &[(&str, &str)]) -> (String, Vec<u8>) {
        const BOUNDARY: &str = "mmrsauthcertsboundary";
        let mut body = Vec::new();
        for (name, content) in parts {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"c.pem\"\r\n\r\n{content}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={BOUNDARY}"), body)
    }

    #[test]
    fn a_non_multipart_body_is_parseform_for_ldap_and_no_file_for_the_other_two() {
        let ldap = CertificateParser::Ldap
            .parse(Some("application/json"), b"{}")
            .unwrap_err();
        assert_eq!(ldap.0.id, "api.admin.add_certificate.parseform.app_error");
        assert_eq!(ldap.0.where_, "addLdapCertificate");
        for parser in [CertificateParser::Saml, CertificateParser::Audit] {
            let err = parser.parse(None, b"{}").unwrap_err();
            assert_eq!(err.0.id, "api.admin.add_certificate.no_file.app_error");
            assert_eq!(err.0.status_code, 400);
        }
    }

    #[test]
    fn a_form_without_a_certificate_part_is_no_file_everywhere() {
        let (content_type, body) = multipart(&[("other", "x")]);
        for parser in [
            CertificateParser::Saml,
            CertificateParser::Ldap,
            CertificateParser::Audit,
        ] {
            let err = parser.parse(Some(&content_type), &body).unwrap_err();
            assert_eq!(err.0.id, "api.admin.add_certificate.no_file.app_error");
        }
    }

    #[test]
    fn two_certificate_parts_are_refused_by_the_audit_parser_only() {
        let (content_type, body) = multipart(&[("certificate", "a"), ("certificate", "b")]);
        let err = CertificateParser::Audit
            .parse(Some(&content_type), &body)
            .unwrap_err();
        assert_eq!(
            err.0.id,
            "api.admin.add_certificate.multiple_files.app_error"
        );
        assert_eq!(
            CertificateParser::Saml
                .parse(Some(&content_type), &body)
                .ok()
                .as_deref(),
            Some("c.pem")
        );
        assert_eq!(
            CertificateParser::Ldap
                .parse(Some(&content_type), &body)
                .ok()
                .as_deref(),
            Some("c.pem")
        );
    }

    #[test]
    fn map_from_json_keeps_the_string_entries_of_a_mixed_object() {
        let props = map_from_json(br#"{"saml_metadata_url":"x","n":5} trailing"#);
        assert_eq!(
            props.get("saml_metadata_url").map(String::as_str),
            Some("x")
        );
        assert!(!props.contains_key("n"));
        assert!(map_from_json(b"null").is_empty());
        assert!(map_from_json(b"[1]").is_empty());
        assert!(map_from_json(b"{\"a\":").is_empty());
        assert!(map_from_json(b"").is_empty());
    }

    #[test]
    fn string_interface_from_json_is_the_first_object_or_nothing() {
        let props = string_interface_from_json(br#"{"toAttribute":"x","n":{}} junk"#);
        assert_eq!(props.len(), 2);
        assert!(string_interface_from_json(b"\"toAttribute\"").is_empty());
        assert!(string_interface_from_json(b"").is_empty());
    }

    #[test]
    fn ldap_settings_decode_follows_gos_decoder() {
        assert!(decode_ldap_settings(b"{}").is_some());
        assert!(
            decode_ldap_settings(b"null").is_some(),
            "null is a no-op in Go"
        );
        assert!(decode_ldap_settings(b"{\"Enable\":null}").is_some());
        assert!(decode_ldap_settings(b"").is_none(), "EOF");
        assert!(decode_ldap_settings(b"[]").is_none());
        assert!(decode_ldap_settings(b"{\"Enable\":\"yes\"}").is_none());
        assert!(
            decode_ldap_settings(b"{\"ENABLE\":\"yes\"}").is_none(),
            "Go matches the key case-insensitively, so the type error is found"
        );
        assert!(decode_ldap_settings(b"{\"LdapPort\":1.5}").is_none());
    }

    #[test]
    fn reset_auth_data_params_null_is_a_missing_body() {
        assert!(matches!(
            decode_first_value(b"null", &RESET_AUTH_DATA_FIELDS),
            Some(serde_json::Value::Null)
        ));
        let value = decode_first_value(
            br#"{"DRY_RUN":true,"user_ids":[null,"a"]}"#,
            &RESET_AUTH_DATA_FIELDS,
        )
        .unwrap();
        let params: ResetAuthDataParams = serde_json::from_value(value).unwrap();
        assert_eq!(params.dry_run, Some(true));
        assert_eq!(params.user_ids.unwrap().len(), 2);
        let value =
            decode_first_value(br#"{"include_deleted":"x"}"#, &RESET_AUTH_DATA_FIELDS).unwrap();
        assert!(serde_json::from_value::<ResetAuthDataParams>(value).is_err());
        assert!(matches!(
            decode_first_value(b"[]", &RESET_AUTH_DATA_FIELDS),
            Some(serde_json::Value::Array(_))
        ));
    }
}
