//! Port of `getDrafts` (channels/api4/drafts.go:68), reached as
//! `GET /api/v4/users/{user_id}/teams/{team_id}/drafts`.
//!
//! The webapp asks for this once per team load when synced drafts are on, which is the default.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_model::permission::{PERMISSION_CREATE_POST, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// Port of `getDrafts` (api4/drafts.go:68).
///
/// # `{user_id}` is in the path and is never read
///
/// The handler passes `c.AppContext.Session().UserId` to the app layer, not `c.Params.UserId`.
/// So `GET /users/{anybody}/teams/{team}/drafts` returns **the caller's own drafts**, whoever the
/// segment names — measured against the running Go server with a second user's id in the path.
/// It is not an information leak, because the answer never depends on the segment; it is a
/// segment that does nothing, and a port that "helpfully" honoured it would return data Go does
/// not.
///
/// # Nothing in this handler validates an id
///
/// There is no `RequireUserId`, no `RequireTeamId` — the handler's first statement is
/// `if c.Err != nil`, and nothing before it sets `Err` except authentication. The router's
/// `[A-Za-z0-9]+` charset is the only filter, so `/users/short/teams/{team}/drafts` is a **200**,
/// not the 400 every neighbouring route gives it. Measured. The team id reaches
/// `SessionHasPermissionToTeam` unvalidated, where a nonexistent team falls back to the
/// system-wide `view_team` check — which an admin passes, so a bogus team id answers `null` for
/// an admin and 403 for anybody else.
///
/// # The feature gate is a 501 and it is first
///
/// `AllowSyncedDrafts` is checked before the permission check, so a caller holding nothing gets
/// **501 `api.drafts.disabled.app_error`** rather than a 403. `App::get_drafts_for_user` carries
/// the same gate with a different id (`app.draft.feature_disabled`); only this one is reachable.
///
/// # The permission it checks and the permission it reports are different
///
/// The check is `view_team`. The refusal is `SetPermissionError(model.PermissionCreatePost)`.
/// Both are Go's, so both are reproduced — but the mismatch is **not observable from the wire**:
/// `SetPermissionError` puts the permission in `DetailedError`, which the api boundary wipes
/// unless `EnableDeveloper` is on. It reaches the server log and nothing else, which is also why
/// no test asserts it.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(drafts)` — a **trailing newline**, and `null` rather than `[]` when
/// there are no drafts, because Go's slice is left nil by `SelectBuilder`. Both measured.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, team_id = %team_id, forwarded))]
pub async fn get_drafts(
    State(state): State<AppState>,
    Path((_path_user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match serve_drafts(&state, &team_id, &session).await {
        Ok(response) => response,
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the drafts read to Go");
            proxy::forward_to_go(State(state), request).await
        }
    }
}

async fn serve_drafts(
    state: &AppState,
    team_id: &str,
    session: &AuthenticatedSession,
) -> Result<Response, PrepareError> {
    if !state.app.config().allow_synced_drafts {
        return Err(PrepareError::App(AppError::boxed(
            "getDrafts",
            "api.drafts.disabled.app_error",
            None,
            String::new(),
            501,
        )));
    }

    if !state
        .app
        .session_has_permission_to_team(
            &session.0,
            team_id,
            &mm_model::permission::PERMISSION_VIEW_TEAM,
        )
        .await
    {
        return Err(PrepareError::App(make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_POST],
        )));
    }

    // The session's user, not the path's — see the note above.
    let drafts = state
        .app
        .get_drafts_for_user(&session.0.user_id, team_id)
        .await?;

    let body = if drafts.is_empty() {
        b"null\n".to_vec()
    } else {
        let mut body = serde_json::to_vec(&drafts).map_err(|err| {
            tracing::error!(error = %err, "failed to serialise the drafts");
            PrepareError::App(AppError::boxed(
                "getDrafts",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
        })?;
        body.push(b'\n');
        body
    };

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}
