//! Thirteen reads that refuse before they read anything, across eleven `api4` files.
//!
//! # Why one module rather than eight
//!
//! `licensed_features.rs` holds the routes whose licence test is the **first statement** of the
//! handler, so nothing about the request is ever consulted. These are the next shape along: each
//! has a gate that fires on every request this deployment can make, but the gate is not always
//! first, is not always the licence, and is not always the same status. Grouping them says that
//! out loud — a reader who adds a permission check "for symmetry" with a neighbour would be wrong
//! in about half the cases here.
//!
//! | route | what runs first | the refusal |
//! |---|---|---|
//! | `/hosted_customer/signup_available` | nothing at all | 501 `api.server.hosted_signup_unavailable.error` |
//! | `/trial-license/prev` | nothing | 403 `api.license.upgrade_needed.app_error` |
//! | `/saml/metadata` | nothing — and **no session either** | 501 `api.admin.saml.not_available.app_error` |
//! | `/ldap/groups` | a permission | 501 `api.ldap_groups.license_error` |
//! | `/system/support_packet` | a permission, and the *restricted-admin* variant | 403 `api.no_license` |
//! | `/custom_profile_attributes/group` | nothing | 403 `app.property.license_error` |
//! | `/users/sessions/attributes/manifest` | nothing — **no session** | 501 `api.user.session_attributes.disabled.app_error` |
//! | `/oauth/outgoing_connections` | a three-way permission | 501 `…configuration_disabled` |
//! | `/oauth/outgoing_connections/{id}` | a **different** permission | 501 `…configuration_disabled` |
//! | `/jobs/{job_id}/download` | `RequireJobId` | 501 `app.job.download_export_results_not_enabled` |
//! | `/files/{file_id}/link` | `RequireFileId` | 403 `api.file.get_public_link.disabled.app_error` |
//! | `/cloud/preview/modal_data` | nothing | 404 `app.cloud.preview_modal_bucket_url_not_configured` |
//! | `/license/load_metric` | nothing | **200** `{"load":0}` — the one member that is not a refusal |
//!
//! # Three gates that are not the licence, and one that is not even a licence question
//!
//! - **`Saml()` and `LicenseManager()` are nil interfaces**, not licence tests. They are
//!   implemented only in the out-of-scope `enterprise/` tree, so a Team Edition binary refuses
//!   whatever its `Systems.ActiveLicenseId` says. Routed through the licence gate anyway, which
//!   forwards a licensed installation — Go answers the same refusal there, so the forward costs a
//!   round trip and buys correctness if the forward target ever becomes an Enterprise build.
//! - **`ServiceSettings.EnableOutgoingOAuthConnections` is a *setting*.** Closed, it gives
//!   `configuration_disabled`; open, the very next line asks for an Enterprise licence and gives
//!   `api.license.upgrade_needed.app_error` — same status, different id. The setting therefore
//!   chooses *which* refusal a Team Edition server gives, never whether it refuses.
//! - **`FeatureFlags.SessionAttributes` is neither.** It is `false` at the pinned SHA and cannot
//!   be persisted (Go strips `FeatureFlags` before writing the document), so the licence half of
//!   `sessionAttributesEnabled` is never reached and this route needs no licence question at all.
//!
//! # One of them is a 200
//!
//! `getLicenseLoadMetric` is in this module because it is the same *shape* — an answer fixed by
//! the absence of a licence — and not because it refuses. Unlicensed, `license.Features.Users` is
//! nil, `licenseUsers` stays 0, the `if licenseUsers > 0` guard is not taken and the metric stays
//! 0, so the body is `{"load":0}` **without a database read**. Putting it anywhere else would hide
//! that its zero is the licence's doing rather than a real measurement.
//!
//! # Two routes take no session
//!
//! `getSamlMetadata` and `getSessionAttributesManifest` are registered with `APIHandler`, not
//! `APISessionRequired` (saml.go:18, user.go:86). An unauthenticated request reaches the handler
//! and gets the refusal, where every other route here answers 401 first. Reproduced by simply not
//! extracting a session — and pinned by a test, because adding the extractor "for consistency"
//! would turn a 501 into a 401 for exactly the callers these routes exist for.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_MANAGE_OUTGOING_OAUTH_CONNECTIONS, PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS,
    PERMISSION_MANAGE_OWN_SLASH_COMMANDS, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{LicenceGate, licence_gate, query_first};
use crate::error::ApiError;

/// Build the refusal. `where_` is the Go handler's own name and is **not on the wire** —
/// `AppError.Where` is not serialised — so it exists for the trace and to keep each route's
/// identity in the source.
fn refusal(where_: &'static str, id: &'static str, status: i32) -> Response {
    ApiError::from(AppError::new(where_, id, None, String::new(), status)).into_response()
}

/// The licence-gated shape: refuse when nothing says this server is licensed, forward when
/// something does. The decision is [`crate::channels::licence_gate`], shared with every other
/// licence-gated route rather than re-derived.
async fn refuse_or_forward(
    state: AppState,
    where_: &'static str,
    id: &'static str,
    status: i32,
    request: Request,
) -> Response {
    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => refusal(where_, id, status),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// Port of `handleSignupAvailable` (api4/hosted_customer.go:19).
///
/// **The whole handler is the error.** Two lines, no condition — this is the only route in the
/// file that is unconditional, and it is unconditional in Go too rather than merely unreachable
/// here. `MakeAuditRecord` is not called either, so there is nothing else to reproduce.
#[tracing::instrument(skip_all)]
pub async fn handle_signup_available(_session: AuthenticatedSession) -> Response {
    refusal(
        "Api4.handleSignupAvailable",
        "api.server.hosted_signup_unavailable.error",
        501,
    )
}

/// Port of `getPrevTrialLicense` (api4/license.go:273).
///
/// `Platform().LicenseManager()` is an `einterfaces` implementation that exists only in the
/// enterprise tree, so it is nil on this build regardless of any licence row — the same class of
/// fact as the cluster interface in [D-087]. **403**, not the 501 most licence refusals use.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_prev_trial_license(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    refuse_or_forward(
        state,
        "getPrevTrialLicense",
        "api.license.upgrade_needed.app_error",
        403,
        request,
    )
    .await
}

/// Port of `getSamlMetadata` (api4/saml.go:39) via `App.GetSamlMetadata` (app/saml.go:27).
///
/// **No session extractor**, because Go registers this with `APIHandler`. The refusal comes from
/// `a.Saml() == nil` in the app layer, not from the handler, which is why the `where` is
/// `GetSamlMetadata` and not `Api4.getSamlMetadata`.
///
/// On success this route writes XML with a `Content-Disposition` — not JSON — which is another
/// reason it is only ever forwarded rather than served on a licensed server.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_saml_metadata(State(state): State<AppState>, request: Request) -> Response {
    refuse_or_forward(
        state,
        "GetSamlMetadata",
        "api.admin.saml.not_available.app_error",
        501,
        request,
    )
    .await
}

/// Port of `getLdapGroups` (api4/ldap.go:152).
///
/// **The permission runs first**, so a caller without
/// `sysconsole_read_user_management_groups` gets a 403 naming it and never learns whether the
/// server is licensed. Only then does the licence test give the 501 — and note its id is
/// `api.ldap_groups.license_error`, the same id `getGroupStats` uses at **403**. One id, two
/// statuses, in two files.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_ldap_groups(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS,
        )
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS],
        ))
        .into_response();
    }

    refuse_or_forward(
        state,
        "api4.getLdapGroups",
        "api.ldap_groups.license_error",
        501,
        request,
    )
    .await
}

/// Port of `generateSupportPacket` (api4/system.go:83).
///
/// Two things a port would get wrong by pattern-matching its neighbours:
///
/// - the permission is `SessionHasPermissionToAndNotRestrictedAdmin`, **not** the plain check, so
///   `ExperimentalSettings.RestrictSystemAdmin` denies a system admin outright rather than falling
///   through to a role test;
/// - the licence refusal is a **403** carrying `api.no_license`, where the file's other licence
///   gates are 501s. Go's comment calls it "e10 or e20", but the test is `License() == nil`.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn generate_support_packet(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state
        .app
        .session_has_permission_to_and_not_restricted_admin(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    refuse_or_forward(
        state,
        "Api4.generateSupportPacket",
        "api.no_license",
        403,
        request,
    )
    .await
}

/// Port of `getCPAGroup` (api4/custom_profile_attributes.go:295).
///
/// Go's own comment explains the oddity: every other CPA endpoint gets its licence check from the
/// property service's `LicenseCheckHook`, and `GetPropertyGroup` is not hooked, so this one
/// enforces `MinimumEnterpriseLicense` inline — at **403**, with a `detailed_error` of "an
/// Enterprise license is required" that `WipeDetailed` removes before it reaches a client.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_cpa_group(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    refuse_or_forward(
        state,
        "getCPAGroup",
        "app.property.license_error",
        403,
        request,
    )
    .await
}

/// Port of `getSessionAttributesManifest` (api4/user.go:2695) via
/// `App.GetSessionAttributesManifest` (app/session_attributes.go:238).
///
/// **No session extractor** — `APIHandler` again — and **no licence question**.
/// `sessionAttributesEnabled` is `FeatureFlags.SessionAttributes && MinimumEnterpriseAdvancedLicense`,
/// and the flag is `false` at the pinned SHA with no way to persist it, so the `&&` short-circuits
/// before the licence is consulted. When the flag *is* set the licence half needs an Enterprise
/// Advanced tier this server cannot establish, so that case forwards.
#[tracing::instrument(skip_all, fields(flag_enabled))]
pub async fn get_session_attributes_manifest(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let enabled = state.app.config().feature_flag_session_attributes;
    tracing::Span::current().record("flag_enabled", enabled);
    if enabled {
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    refusal(
        "GetSessionAttributesManifest",
        "api.user.session_attributes.disabled.app_error",
        501,
    )
}

/// `ensureOutgoingOAuthConnectionInterface` (api4/outgoing_oauth_connection.go:59), shared by both
/// connection reads.
///
/// Two arms at the **same status** with different ids, and the setting picks which. The second arm
/// also tests `OutgoingOAuthConnections() == nil` — another enterprise-only interface — so an open
/// setting refuses on this build whatever the licence says; forwarding is what covers that.
const OUTGOING_OAUTH_WHERE: &str = "Api4.outgoingOAuthConnection";
const OUTGOING_OAUTH_DISABLED: &str =
    "api.context.outgoing_oauth_connection.not_available.configuration_disabled";
const OUTGOING_OAUTH_UNLICENSED: &str = "api.license.upgrade_needed.app_error";

async fn outgoing_oauth_gate(state: AppState, request: Request) -> Response {
    if !state.app.config().enable_outgoing_oauth_connections {
        tracing::Span::current().record("configured", false);
        return refusal(OUTGOING_OAUTH_WHERE, OUTGOING_OAUTH_DISABLED, 501);
    }
    tracing::Span::current().record("configured", true);
    refuse_or_forward(
        state,
        OUTGOING_OAUTH_WHERE,
        OUTGOING_OAUTH_UNLICENSED,
        501,
        request,
    )
    .await
}

/// Port of `listOutgoingOAuthConnections` (api4/outgoing_oauth_connection.go:129).
///
/// The permission is a **three-way or**, and two of its arms are *team*-scoped on the `team_id`
/// query parameter — so the same caller is admitted or refused depending on a query string. With
/// no `team_id` the two team arms are false by construction (`SessionHasPermissionToTeam` returns
/// false for an empty id before anything else), leaving `manage_outgoing_oauth_connections` alone.
///
/// The refusal names **two** permissions and neither of them is the one that would have admitted a
/// system admin — Go passes `manage_own_outgoing_webhooks` and `manage_own_slash_commands` to
/// `SetPermissionError`. The names are in the detail, which is wiped, so this shows only in a log.
#[tracing::instrument(skip_all, fields(team_id, configured, licensed))]
pub async fn list_outgoing_oauth_connections(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let team_id = query_first(query.as_deref(), "team_id").unwrap_or_default();
    tracing::Span::current().record("team_id", &team_id);

    let allowed = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OUTGOING_OAUTH_CONNECTIONS)
        .await
        || state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS,
            )
            .await
        || state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
            )
            .await;

    if !allowed {
        return ApiError::from(make_permission_error(
            &session.0,
            &[
                &PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS,
                &PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
            ],
        ))
        .into_response();
    }

    outgoing_oauth_gate(state, request).await
}

/// Port of `getOutgoingOAuthConnection` (api4/outgoing_oauth_connection.go:180).
///
/// **A read guarded by the write permission.** Go's own comment says it is "intended for system
/// admins to manage (setup) outgoing oauth connections", so `manage_outgoing_oauth_connections` is
/// the only way in — the three-way check its list sibling uses does not apply here. Two routes in
/// one file, two permission rules, and the stricter one is on the `GET` of a single item.
///
/// `RequireOutgoingOAuthConnectionId` runs **after** the interface check and its result is
/// discarded — `c.RequireOutgoingOAuthConnectionId()` is called without testing `c.Err`
/// (outgoing_oauth_connection.go:190) — so a malformed id never produces a 400 through this route.
/// Unreachable here, and named because it is the kind of thing a port would "fix".
#[tracing::instrument(skip_all, fields(connection_id = %connection_id, configured, licensed))]
pub async fn get_outgoing_oauth_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &connection_id;
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

    outgoing_oauth_gate(state, request).await
}

/// Port of `downloadJob` (api4/job.go:59) — `GET /api/v4/jobs/{job_id}/download`.
///
/// **`RequireJobId` comes first and the config check second**, so a well-formed id that names no
/// job is the 501 and not a 404: the job is fetched only after the setting has been consulted.
/// That ordering is the whole observable behaviour of this route on a stock server.
///
/// Behind it lies the export filestore, which this server does not have — so an enabled setting
/// forwards. It is the last of `job.go`'s four reads and the only one that streams bytes.
#[tracing::instrument(skip_all, fields(job_id = %job_id, enabled))]
pub async fn download_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&job_id) {
        return ApiError::invalid_url_param("job_id").into_response();
    }

    let enabled = state.app.config().message_export_download_export_results;
    tracing::Span::current().record("enabled", enabled);
    if enabled {
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    refusal(
        "downloadExportResultsNotEnabled",
        "app.job.download_export_results_not_enabled",
        501,
    )
}

/// Port of `getFileLink` (api4/file.go:709) — `GET /api/v4/files/{file_id}/link`.
///
/// `FileSettings.EnablePublicLink` is checked **after** `RequireFileId` and before everything
/// else — before the file is fetched, before the channel is read, before any permission. So a
/// malformed id is a 400 and every well-formed one is the same 403, whether or not the file
/// exists.
#[tracing::instrument(skip_all, fields(file_id = %file_id, enabled))]
pub async fn get_file_link(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&file_id) {
        return ApiError::invalid_url_param("file_id").into_response();
    }
    public_link_gate(state, "getPublicLink", request).await
}

/// The `EnablePublicLink` gate. Open, the route needs the file backend and a signed-hash
/// comparison, neither of which is ported — so it forwards.
///
/// Shared with `getPublicFile` in Go and **not** here: that route's path is outside `/api/`, so
/// `web.Handler` renders a signed HTML page rather than the JSON `AppError`, and it is not served.
/// See [D-170].
async fn public_link_gate(state: AppState, where_: &'static str, request: Request) -> Response {
    let enabled = state.app.config().enable_public_link;
    tracing::Span::current().record("enabled", enabled);
    if enabled {
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    refusal(where_, "api.file.get_public_link.disabled.app_error", 403)
}

/// Port of `getPreviewModalData` (api4/cloud.go:618) via `App.GetPreviewModalData`
/// (app/cloud.go:43).
///
/// `CloudSettings.PreviewModalBucketURL` is empty on a stock server, and Go's test is
/// `bucketURL == nil || *bucketURL == ""` — the two are one answer. Set, the route fetches JSON
/// over HTTP from that bucket, which this server does not do, so it forwards.
#[tracing::instrument(skip_all, fields(configured))]
pub async fn get_preview_modal_data(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let configured = !state.app.config().cloud_preview_modal_bucket_url.is_empty();
    tracing::Span::current().record("configured", configured);
    if configured {
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    refusal(
        "GetPreviewModalData",
        "app.cloud.preview_modal_bucket_url_not_configured",
        404,
    )
}

/// Port of `getLicenseLoadMetric` (api4/license.go:300) — `GET /api/v4/license/load_metric`.
///
/// **A 200, and the only one in this module.** Unlicensed, `license.Features.Users` is nil so
/// `licenseUsers` stays 0, the `if licenseUsers > 0` guard is not taken, and `loadMetric` keeps its
/// zero value — so the monthly-active-user count is **never queried** and the body is `{"load":0}`.
/// A port that computed the ratio anyway would divide by zero to reach the same number, which is
/// the kind of accident that stops being the same number the moment a licence appears.
///
/// `map[string]int` with one key, `json.NewEncoder(w).Encode` — so a trailing newline — and an
/// explicit `Content-Type` that Go sets by hand a line earlier.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_license_load_metric(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            b"{\"load\":0}\n".to_vec(),
        )
            .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The statuses are **not** uniform: four of the twelve refusals are 403s and one is a 404. A port that reached for
    /// 501 everywhere — the status most licence refusals use — would be wrong on
    /// `trial-license/prev`, `system/support_packet` and `custom_profile_attributes/group`.
    #[test]
    fn the_family_carries_three_statuses_and_eleven_ids() {
        let cases: &[(&str, i32)] = &[
            ("api.server.hosted_signup_unavailable.error", 501),
            ("api.license.upgrade_needed.app_error", 403),
            ("api.admin.saml.not_available.app_error", 501),
            ("api.ldap_groups.license_error", 501),
            ("api.no_license", 403),
            ("app.property.license_error", 403),
            ("api.user.session_attributes.disabled.app_error", 501),
            (OUTGOING_OAUTH_DISABLED, 501),
            (OUTGOING_OAUTH_UNLICENSED, 501),
            ("app.job.download_export_results_not_enabled", 501),
            ("api.file.get_public_link.disabled.app_error", 403),
            ("app.cloud.preview_modal_bucket_url_not_configured", 404),
        ];

        // Ten refusals and **nine** ids: the two outgoing-OAuth routes share
        // `configuration_disabled` because they share `ensureOutgoingOAuthConnectionInterface`.
        // Everything else in the family has an id of its own, which is why the ids are listed
        // here rather than derived from a shared constant.
        let ids: std::collections::BTreeSet<&str> = cases.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), 11, "twelve refusals, eleven distinct ids");

        let statuses: std::collections::BTreeSet<i32> =
            cases.iter().map(|(_, status)| *status).collect();
        assert_eq!(
            statuses,
            [403, 404, 501].into_iter().collect(),
            "three statuses — and the 404 is a *configuration* answer, not a missing resource"
        );
    }

    /// `api.ldap_groups.license_error` is used by **two** routes at **two** statuses —
    /// `getLdapGroups` at 501 here and `getGroupStats` at 403 in `group.go`. Pinned so that a
    /// reader who finds the id in one file does not copy the other's status.
    #[test]
    fn the_ldap_groups_id_is_shared_and_the_status_is_not() {
        let ours = refusal("api4.getLdapGroups", "api.ldap_groups.license_error", 501);
        assert_eq!(ours.status().as_u16(), 501);
        let theirs = refusal("Api4.getGroupStats", "api.ldap_groups.license_error", 403);
        assert_eq!(theirs.status().as_u16(), 403);
    }
}
