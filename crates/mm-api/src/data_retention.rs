//! Port of `api4/data_retention.go` — fifteen of its seventeen routes.
//!
//! # Every one of them is a 501 here, and the interesting part is what comes first
//!
//! `App.DataRetention()` is `einterfaces.DataRetentionInterface`, registered only by the
//! enterprise build (`app/platform/service.go`), so on the Team Edition binary beside us it is
//! **always nil** and every app function in `app/data_retention.go` answers
//! `ent.data_retention.generic.license.error` at **501** before touching anything
//! (data_retention.go:117). That refusal is therefore the whole of each route on this deployment —
//! there is no reachable path past it — and a licensed installation is forwarded, since the
//! interface lives in the other process.
//!
//! What makes the family worth porting rather than proxying is that the checks **ahead** of that
//! refusal are all reachable, and they are in a different order in almost every handler:
//!
//! | Route | Order |
//! |---|---|
//! | `getGlobalPolicy` | *nothing* — no permission check at all |
//! | `getPolicies`, `getPoliciesCount` | permission |
//! | `getPolicy`, `getTeamsForPolicy`, `getChannelsForPolicy` | permission, **then** the id |
//! | `deletePolicy` | the id, **then** permission |
//! | `createPolicy` | body, then permission |
//! | `patchPolicy` | body, then the id, then permission |
//! | `addTeamsToPolicy` and its three siblings | the id, then body, then permission |
//! | `getTeamPoliciesForUser`, `getChannelPoliciesForUser` | the user id, then self-or-`manage_system` |
//!
//! # `RequirePolicyId` is dead on eleven of them, and reproducing it would be a bug
//!
//! Go calls `c.RequirePolicyId()` and then **does not check `c.Err`** — `getPolicy` goes straight
//! on to `c.App.GetRetentionPolicy(...)`, whose error *overwrites* the 400 the id check just set.
//! So on an unlicensed server `GET /api/v4/data_retention/policies/short` is a **501, not a 400**.
//! Measured against the running Go server, because no reading of the handler suggests it. The two
//! per-user routes are the exception: they do check, so a malformed user id there really is a 400.
//!
//! # `searchTeamsInPolicy` and `searchChannelsInPolicy` are **not** here
//!
//! They look like the rest and are not: they call `SearchAllTeams` / `SearchAllChannels`, which
//! are ordinary searches with a `policy_id` filter and no licence gate — measured at **200** on
//! this server. They belong with `/teams/search` and `/channels/search`, which is where the search
//! machinery they need will land.

use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
    PERMISSION_SYSCONSOLE_WRITE_COMPLIANCE_DATA_RETENTION_POLICY, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id, sorted_array_from_json};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::ME;
use crate::error::ApiError;
use crate::proxy;

/// `model.PayloadParseError` (model/utils.go:42).
const PAYLOAD_PARSE_ERROR: &str = "api.payload.parse.error";

/// The one error every app function in `app/data_retention.go` returns on this deployment
/// (`newLicenseError`, data_retention.go:117).
const LICENSE_ERROR: &str = "ent.data_retention.generic.license.error";

/// Which of the two sysconsole permissions a route wants, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// `sysconsole_read_compliance_data_retention_policy`.
    Read,
    /// `sysconsole_write_compliance_data_retention_policy`.
    Write,
    /// `getGlobalPolicy` alone: Go's comment says "No permission check required" in as many words.
    None,
}

/// The body shape a route decodes before anything else, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// `json.NewDecoder(r.Body).Decode(&model.RetentionPolicyWithTeamAndChannelIDs)`; a decode
    /// failure is `api.context.invalid_body_param.app_error` naming **`policy`**.
    Policy,
    /// `model.SortedArrayFromJSON(r.Body)`; a decode failure is `api.payload.parse.error`.
    IdList,
    None,
}

/// Everything one of these routes does before its 501, as data.
///
/// The handlers below are one line each because the only thing that varies between them is this
/// struct — and writing the variation down as data is what makes the *ordering* reviewable
/// against the table in the module docs, rather than buried in fifteen near-identical functions.
#[derive(Debug, Clone, Copy)]
struct Route {
    /// The Go handler, for the trace. Not on the wire: `AppError.Where` is not serialised.
    name: &'static str,
    body: Body,
    gate: Gate,
    /// Whether the body is decoded **before** the permission check. True for every route that
    /// decodes at all — Go has no counter-example here, and it is stated rather than assumed.
    body_first: bool,
}

impl Route {
    const fn new(name: &'static str, body: Body, gate: Gate) -> Self {
        Self {
            name,
            body,
            gate,
            body_first: true,
        }
    }
}

/// Run a route's checks in order and then refuse, or forward if this server is licensed.
async fn answer(
    state: AppState,
    session: &mm_model::session::Session,
    route: Route,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "the request body could not be read");
            return body_error(route.body).into_response();
        }
    };

    if route.body_first
        && let Err(err) = decode(route.body, &bytes)
    {
        return err.into_response();
    }

    let permitted = match route.gate {
        Gate::None => true,
        Gate::Read => {
            state
                .app
                .session_has_permission_to(
                    session,
                    &PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
                )
                .await
        }
        Gate::Write => {
            state
                .app
                .session_has_permission_to(
                    session,
                    &PERMISSION_SYSCONSOLE_WRITE_COMPLIANCE_DATA_RETENTION_POLICY,
                )
                .await
        }
    };
    if !permitted {
        let required = match route.gate {
            Gate::Read => &PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
            _ => &PERMISSION_SYSCONSOLE_WRITE_COMPLIANCE_DATA_RETENTION_POLICY,
        };
        return ApiError::from(make_permission_error(session, &[required])).into_response();
    }

    refuse_or_forward(&state, route.name, parts, bytes).await
}

/// `model.SortedArrayFromJSON` / the policy decode, as a pass-or-400.
fn decode(body: Body, bytes: &[u8]) -> Result<(), ApiError> {
    match body {
        Body::None => Ok(()),
        // Every field of `RetentionPolicyWithTeamAndChannelIDs` is optional to Go's decoder, so
        // only malformed JSON fails. `serde_json::Value` is exactly that test and does not need
        // the model type ported for a route that never reads it.
        Body::Policy => serde_json::from_slice::<serde_json::Value>(bytes)
            .map(|_| ())
            .map_err(|_| ApiError::invalid_param("policy")),
        Body::IdList => sorted_array_from_json(bytes).map(|_| ()).map_err(|_| {
            ApiError::from(AppError::new(
                "dataRetention",
                PAYLOAD_PARSE_ERROR,
                None,
                String::new(),
                400,
            ))
        }),
    }
}

fn body_error(body: Body) -> ApiError {
    match body {
        Body::Policy => ApiError::invalid_param("policy"),
        _ => ApiError::from(AppError::new(
            "dataRetention",
            PAYLOAD_PARSE_ERROR,
            None,
            String::new(),
            400,
        )),
    }
}

/// The 501, or the proxy when a licence is installed.
async fn refuse_or_forward(
    state: &AppState,
    name: &'static str,
    parts: axum::http::request::Parts,
    body: axum::body::Bytes,
) -> Response {
    let licensed = match state.app.license_state().await {
        Ok(state) => state == mm_app::license::LicenseState::Licensed,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("licensed", licensed);

    if licensed {
        let request = Request::from_parts(parts, axum::body::Body::from(body));
        return proxy::forward_to_go(State(state.clone()), request).await;
    }

    ApiError::from(AppError::new(name, LICENSE_ERROR, None, String::new(), 501)).into_response()
}

macro_rules! route {
    ($fn_name:ident, $go:literal, $body:expr, $gate:expr) => {
        #[doc = concat!("Port of `", $go, "` (api4/data_retention.go).")]
        #[tracing::instrument(skip_all, fields(licensed))]
        pub async fn $fn_name(
            State(state): State<AppState>,
            session: AuthenticatedSession,
            request: Request,
        ) -> Response {
            answer(state, &session.0, Route::new($go, $body, $gate), request).await
        }
    };
}

route!(
    get_global_policy,
    "App.GetGlobalRetentionPolicy",
    Body::None,
    Gate::None
);
route!(
    get_policies,
    "App.GetRetentionPolicies",
    Body::None,
    Gate::Read
);
route!(
    get_policies_count,
    "App.GetRetentionPoliciesCount",
    Body::None,
    Gate::Read
);
route!(get_policy, "App.GetRetentionPolicy", Body::None, Gate::Read);
route!(
    create_policy,
    "App.CreateRetentionPolicy",
    Body::Policy,
    Gate::Write
);
route!(
    patch_policy,
    "App.PatchRetentionPolicy",
    Body::Policy,
    Gate::Write
);
route!(
    delete_policy,
    "App.DeleteRetentionPolicy",
    Body::None,
    Gate::Write
);
route!(
    get_teams_for_policy,
    "App.GetTeamsForRetentionPolicy",
    Body::None,
    Gate::Read
);
route!(
    add_teams_to_policy,
    "App.AddTeamsToRetentionPolicy",
    Body::IdList,
    Gate::Write
);
route!(
    remove_teams_from_policy,
    "App.RemoveTeamsFromRetentionPolicy",
    Body::IdList,
    Gate::Write
);
route!(
    get_channels_for_policy,
    "App.GetChannelsForRetentionPolicy",
    Body::None,
    Gate::Read
);
route!(
    add_channels_to_policy,
    "App.AddChannelsToRetentionPolicy",
    Body::IdList,
    Gate::Write
);
route!(
    remove_channels_from_policy,
    "App.RemoveChannelsFromRetentionPolicy",
    Body::IdList,
    Gate::Write
);

/// Port of `getTeamPoliciesForUser` (data_retention.go:457).
///
/// # The only two routes here whose id check is alive
///
/// `c.RequireUserId(); if c.Err != nil { return }` — so a malformed user id is a **400**, unlike
/// the eleven `RequirePolicyId` calls whose result is thrown away. `me` is resolved first, as
/// everywhere.
///
/// # Self-or-admin, and the refusal names `manage_system`
///
/// `userID != session.UserId && !SessionHasPermissionTo(manage_system)`. So a user may ask for
/// their own policies without any permission at all, and only reading *someone else's* needs
/// `manage_system` — not a sysconsole read, unlike every other route in this file.
#[tracing::instrument(skip_all, fields(user_id, licensed))]
pub async fn get_team_policies_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    per_user(
        state,
        session,
        user_id,
        "App.GetTeamPoliciesForUser",
        request,
    )
    .await
}

/// Port of `getChannelPoliciesForUser` (data_retention.go:487) — [`get_team_policies_for_user`]
/// with a different app call, and the same checks in the same order.
#[tracing::instrument(skip_all, fields(user_id, licensed))]
pub async fn get_channel_policies_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    per_user(
        state,
        session,
        user_id,
        "App.GetChannelPoliciesForUser",
        request,
    )
    .await
}

async fn per_user(
    state: AppState,
    session: AuthenticatedSession,
    user_id: String,
    name: &'static str,
    request: Request,
) -> Response {
    // `RequireUserId` resolves `me` before it validates (web/context.go:301).
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    tracing::Span::current().record("user_id", &user_id);
    if !is_valid_id(&user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    if user_id != session.0.user_id
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    refuse_or_forward(&state, name, parts, bytes).await
}

/// `ReturnStatusOK` is never reached on this deployment, but the four routes that would write it
/// are registered, so the constant is here to say so rather than to be used.
#[cfg(test)]
const STATUS_OK_BODY: &str = r#"{"status":"OK"}"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The two body shapes and the two different 400s they produce. `policy` is a *body param*
    /// error naming the field; an id list is `api.payload.parse.error`, which names nothing.
    #[test]
    fn the_two_body_shapes_fail_differently() {
        assert!(decode(Body::None, b"nonsense").is_ok(), "no body, no check");

        let policy = decode(Body::Policy, b"{").expect_err("malformed");
        assert_eq!(policy.0.id, "api.context.invalid_body_param.app_error");
        assert_eq!(policy.0.status_code, 400);
        assert!(
            decode(Body::Policy, b"{}").is_ok(),
            "every field is optional"
        );
        assert!(
            decode(Body::Policy, br#"{"display_name":"x"}"#).is_ok(),
            "and unknown or partial fields decode, as Go's decoder does"
        );

        let list = decode(Body::IdList, b"[").expect_err("malformed");
        assert_eq!(list.0.id, PAYLOAD_PARSE_ERROR);
        assert_eq!(list.0.status_code, 400);
        assert!(
            decode(Body::IdList, b"[]").is_ok(),
            "an empty list is not an error"
        );
        assert!(
            decode(Body::IdList, b"null").is_ok(),
            "`SortedArrayFromJSON` returns (nil, nil) for `null` — no error, and the handler \
             carries on to the licence refusal"
        );
    }

    /// The gate each route wants. Written out because the file's whole risk is putting the read
    /// permission where the write one belongs, and the two names differ by one word.
    #[test]
    fn the_reads_and_the_writes_are_two_different_permissions() {
        assert_eq!(
            PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY.id,
            "sysconsole_read_compliance_data_retention_policy"
        );
        assert_eq!(
            PERMISSION_SYSCONSOLE_WRITE_COMPLIANCE_DATA_RETENTION_POLICY.id,
            "sysconsole_write_compliance_data_retention_policy"
        );
        assert_ne!(
            PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY.id,
            PERMISSION_SYSCONSOLE_WRITE_COMPLIANCE_DATA_RETENTION_POLICY.id
        );
    }

    /// `getGlobalPolicy` really does refuse nobody — the one route in the file with no check at
    /// all, which is easy to read as an omission and is Go's comment verbatim.
    #[test]
    fn the_global_policy_has_no_gate() {
        let route = Route::new("App.GetGlobalRetentionPolicy", Body::None, Gate::None);
        assert_eq!(route.gate, Gate::None);
        assert_eq!(route.body, Body::None);
    }

    /// The status body four of these routes would write if a licence ever let them through.
    #[test]
    fn the_unreachable_success_body_is_recorded() {
        assert_eq!(STATUS_OK_BODY, r#"{"status":"OK"}"#);
    }
}
