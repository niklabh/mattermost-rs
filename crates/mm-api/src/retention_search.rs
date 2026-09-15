//! Port of `searchTeamsInPolicy` (api4/data_retention.go:244) and `searchChannelsInPolicy`
//! (:360) — `POST /api/v4/data_retention/policies/{policy_id}/teams/search` and
//! `POST /api/v4/data_retention/policies/{policy_id}/channels/search`.
//!
//! These are the two routes [`crate::data_retention`] left out: they never touch
//! `App.DataRetention()`, so there is no nil enterprise interface in the way. Each is an
//! ordinary `SearchAllTeams` / `SearchAllChannels` with the policy id pinned and
//! `IncludePolicyID` on, behind the `sysconsole_read_compliance_data_retention_policy`
//! permission — measured at 200 on the unlicensed stack.
//!
//! # `RequirePolicyId` is dead, and the corpse is on the wire
//!
//! Both handlers call `c.RequirePolicyId()` and do not test `c.Err`. Everything after it runs;
//! a later refusal *replaces* the 400, and a success writes its body with a 200 — and then the
//! handler wrapper (web/handlers.go) sees `c.Err` still set and writes the error too. The
//! `WriteHeader(400)` after a written body is a no-op, so the answer to a short policy id with
//! a valid body and permission is **200, `[]` followed by the 400's JSON**. Measured. It is
//! reproduced here: the 400 is kept aside and appended to the success body, its `request_id`
//! fresh, exactly as the wrapper appends it. The order of the two checks differs between the
//! handlers — teams tests the permission before the body, channels the body before the
//! permission — and a caller who fails both is told about the first.
//!
//! # `json.Marshal`, not the encoder
//!
//! Both bodies are `json.Marshal` + `w.Write`: no trailing newline, and `<`, `>` and `&` in a
//! display name are escaped, which [`go_json_marshal`] reproduces.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::channel::ChannelSearchOpts;
use mm_model::permission::{
    PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY, make_permission_error,
};
use mm_model::team_search::TeamSearch;
use mm_model::utils::{AppError, decode_one_from_json, go_json_marshal, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// The dead `RequirePolicyId`: `Some` when the segment is not a 26-character id.
fn pending_policy_id_error(policy_id: &str) -> Option<ApiError> {
    (!is_valid_id(policy_id)).then(|| ApiError::invalid_url_param("policy_id"))
}

/// A 200 whose body is the marshalled value, with the wrapper's appended error when the id
/// check failed and nothing later replaced it.
fn marshalled(
    where_: &'static str,
    value: &impl serde::Serialize,
    pending: Option<ApiError>,
) -> Response {
    let mut body = match go_json_marshal(value) {
        Ok(body) => body.into_bytes(),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the response");
            return ApiError::from(AppError::new(
                where_,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    if let Some(pending) = pending {
        tracing::debug!("appending the dead RequirePolicyId error to the 200 body, as Go does");
        if let (_, Some(error_body)) = pending.into_wire() {
            body.extend_from_slice(&error_body);
        }
    }
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

async fn read_body(request: Request) -> Result<axum::body::Bytes, ()> {
    axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
        })
}

/// Port of `searchTeamsInPolicy` (api4/data_retention.go:244).
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn search_teams_in_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let pending = pending_policy_id_error(&policy_id);

    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
        )
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY],
        ))
        .into_response();
    }

    let Ok(bytes) = read_body(request).await else {
        return ApiError::invalid_param("team_search").into_response();
    };
    // `json.Decoder.Decode(&props)` into a value: one JSON value, `null` leaves the zero struct.
    let mut props: TeamSearch = match decode_one_from_json::<Option<TeamSearch>>(&bytes) {
        Ok(decoded) => decoded.unwrap_or_default(),
        Err(err) => {
            tracing::debug!(error = %err, "team search body did not decode");
            return ApiError::invalid_param("team_search").into_response();
        }
    };

    props.policy_id = Some(policy_id);
    props.include_policy_id = Some(true);

    let (mut teams, _total) = match state.app.search_all_teams(&props).await {
        Ok(found) => found,
        Err(err) => return ApiError::from(err).into_response(),
    };
    state.app.sanitize_teams(&session.0, &mut teams).await;

    marshalled("searchTeamsInPolicy", &teams, pending)
}

/// Port of `searchChannelsInPolicy` (api4/data_retention.go:360). The body comes before the
/// permission here, and a `null` body is the 400 (`props == nil`).
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn search_channels_in_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let pending = pending_policy_id_error(&policy_id);

    let props = match crate::channels::decode_full_channel_search(request).await {
        Ok(props) => props,
        Err(err) => return err.into_response(),
    };

    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
        )
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY],
        ))
        .into_response();
    }

    // The seven fields Go copies (data_retention.go:374); everything else stays zero.
    let opts = ChannelSearchOpts {
        policy_id,
        include_policy_id: true,
        deleted: props.deleted,
        include_deleted: props.include_deleted,
        public: props.public,
        private: props.private,
        team_ids: props.team_ids.clone().unwrap_or_default(),
        ..ChannelSearchOpts::default()
    };

    let (channels, _total) = match state.app.search_all_channels(&props.term, &opts).await {
        Ok(found) => found,
        Err(err) => return ApiError::from(err).into_response(),
    };

    marshalled("searchChannelsInPolicy", &channels, pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dead_id_check_is_kept_aside_only_for_a_short_id() {
        assert!(pending_policy_id_error("abc").is_some());
        assert!(pending_policy_id_error("abcdefghijklmnopqrstuvwxyz").is_none());
    }

    /// The 200 body carries the marshalled value first and the error JSON second.
    #[tokio::test]
    async fn the_appended_error_follows_the_body() {
        let response = marshalled(
            "test",
            &Vec::<String>::new(),
            Some(ApiError::invalid_url_param("policy_id")),
        );
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.starts_with("[]{\"id\":\"api.context.invalid_url_param.app_error\""),
            "{text}"
        );
        assert!(text.ends_with("\"status_code\":400}"), "{text}");
    }
}
