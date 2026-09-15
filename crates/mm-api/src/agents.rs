//! Port of `api4/agents.go` — `getAgents` (:42), `getAgentsStatus` (:23) and `getLLMServices`
//! (:60): `GET /api/v4/agents`, `GET /api/v4/agents/status` and `GET /api/v4/llmservices`.
//!
//! Three session-required reads with no permission check, each `json.Marshal` + `w.Write` (no
//! trailing newline). What they answer on a server that hosts no plugins is
//! [`mm_app::agents`]'s subject; the handlers themselves have nothing to decide.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::agents::AgentsIntegrityResponse;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

fn marshalled(where_: &'static str, value: &impl serde::Serialize) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the response");
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

/// Port of `getAgentsStatus` (api4/agents.go:23) — `GET /api/v4/agents/status`.
#[tracing::instrument(skip_all)]
pub async fn get_agents_status(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
) -> Response {
    let (available, reason) = state.app.ai_plugin_bridge_status();
    marshalled(
        "Api4.getAgentsStatus",
        &AgentsIntegrityResponse {
            available,
            reason: reason.to_owned(),
        },
    )
}

/// Port of `getAgents` (api4/agents.go:42) — `GET /api/v4/agents`. A bridge failure would be
/// the 500 `app.agents.get_agents.app_error`; the bridge is never called here.
#[tracing::instrument(skip_all)]
pub async fn get_agents(State(state): State<AppState>, _session: AuthenticatedSession) -> Response {
    match state.app.agents() {
        Ok(agents) => marshalled("Api4.getAgents", &agents),
        Err(err) => ApiError::from(AppError::new(
            "Api4.getAgents",
            "app.agents.get_agents.app_error",
            None,
            err.to_string(),
            500,
        ))
        .into_response(),
    }
}

/// Port of `getLLMServices` (api4/agents.go:60) — `GET /api/v4/llmservices`.
#[tracing::instrument(skip_all)]
pub async fn get_llm_services(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
) -> Response {
    match state.app.llm_services() {
        Ok(services) => marshalled("Api4.getLLMServices", &services),
        Err(err) => ApiError::from(AppError::new(
            "Api4.getLLMServices",
            "app.agents.get_services.app_error",
            None,
            err.to_string(),
            500,
        ))
        .into_response(),
    }
}
