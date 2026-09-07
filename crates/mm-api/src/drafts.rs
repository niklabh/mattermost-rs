//! Port of `getDrafts` (channels/api4/drafts.go:68), reached as
//! `GET /api/v4/users/{user_id}/teams/{team_id}/drafts`.
//!
//! The webapp asks for this once per team load when synced drafts are on, which is the default.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::draft::DraftWrite;
use mm_app::post::PrepareError;
use mm_model::draft::Draft;
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

/// `model.ConnectionId` — the header a client uses to say which of its own websocket connections
/// made the request, so the resulting event can skip it.
///
/// **`Connection-Id`, with no `X-` prefix.** The constant lives in `client4.go`, which this
/// project never reads, so the value is repeated here rather than imported — the same choice
/// `mm-model` made for `StatusFail`. It was measured rather than assumed: the first version used
/// `X-Connection-Id` and Go published an empty `omit_connection_id` for a request that carried it.
const CONNECTION_ID_HEADER: &str = "Connection-Id";

/// Port of `upsertDraft` (api4/drafts.go:23) — `POST /api/v4/drafts`.
///
/// # The permission check has two arms and the second is a fallback
///
/// `create_post` on the channel, **or** — for an open channel only — `create_post_public` on its
/// team. Go's own comment calls the second arm "a temporary permission check method until
/// advanced permissions, please do not copy", and its `GetChannel` error is *swallowed*: a
/// nonexistent channel simply fails the fallback and answers 403, before the app layer's
/// channel-shaped 400 can be reached.
///
/// So a bad `channel_id` is a **403**, not a 400 — the app layer's `draft.channel_id` invalid-param
/// is reachable only when the caller holds `create_post` system-wide, which an admin does.
/// Measured both ways.
///
/// # Two fields are overwritten from the request, not read from the body
///
/// `DeleteAt = 0` and `UserId = session.UserId`. A client that sends somebody else's `user_id`
/// saves a draft for itself, and one that sends a `delete_at` has it ignored.
///
/// # `201 Created`, and `null` when the message was empty
///
/// The status is `Created` for every success, including the one that **deleted** a row — see
/// [`mm_app::draft::DraftWrite::DeletedBecauseEmpty`]. The body is `json.NewEncoder(w).Encode`,
/// so it carries a trailing newline either way.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, forwarded))]
pub async fn upsert_draft(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().allow_synced_drafts {
        return ApiError::from(AppError::new(
            "upsertDraft",
            "api.drafts.disabled.app_error",
            None,
            String::new(),
            501,
        ))
        .into_response();
    }

    let (parts, body) = request.into_parts();
    let connection_id = parts
        .headers
        .get(CONNECTION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("draft").into_response();
        }
    };

    let mut draft: Draft = match serde_json::from_slice(&bytes) {
        Ok(draft) => draft,
        // `SetInvalidParam("draft")` — note the *plain* form, without the wrapped error that
        // `saveReaction` uses, so this one carries no `detailed_error`.
        Err(err) => {
            tracing::debug!(error = %err, "draft body did not decode");
            return ApiError::invalid_param("draft").into_response();
        }
    };

    draft.delete_at = 0;
    draft.user_id = session.0.user_id.clone();

    if !has_draft_permission(&state, &session, &draft.channel_id).await {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_POST],
        ))
        .into_response();
    }

    match state.app.upsert_draft(&draft, &connection_id).await {
        Ok(DraftWrite::Saved(saved)) => {
            tracing::Span::current().record("forwarded", false);
            match mm_model::utils::go_json_marshal(&*saved) {
                Ok(json) => created(json + "\n"),
                Err(err) => {
                    tracing::warn!(error = %err, "Error while writing response");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        // Go returns `(nil, nil)` and the handler encodes the nil pointer: `201` with a body of
        // `null` and a newline, for a request that deleted a row.
        Ok(DraftWrite::DeletedBecauseEmpty) => {
            tracing::Span::current().record("forwarded", false);
            created("null\n".to_owned())
        }
        Ok(DraftWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the draft write to Go");
            let request = Request::from_parts(parts, Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
        Err(app_error) => ApiError::from(app_error).into_response(),
    }
}

fn created(body: String) -> Response {
    (
        StatusCode::CREATED,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

/// The two-armed permission check of `upsertDraft` (api4/drafts.go:38).
async fn has_draft_permission(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
) -> bool {
    let (granted, _) = state
        .app
        .session_has_permission_to_channel(&session.0, channel_id, &PERMISSION_CREATE_POST)
        .await;
    if granted {
        return true;
    }

    // The fallback, and Go swallows the lookup's error: a channel that does not exist simply
    // fails here.
    let Ok(channel) = state.app.get_channel(channel_id).await else {
        return false;
    };
    channel.channel_type == mm_model::channel::CHANNEL_TYPE_OPEN
        && state
            .app
            .session_has_permission_to_team(
                &session.0,
                &channel.team_id,
                &mm_model::permission::PERMISSION_CREATE_POST_PUBLIC,
            )
            .await
}

/// Port of `deleteDraft` (api4/drafts.go:97) — both
/// `DELETE /api/v4/users/{user_id}/channels/{channel_id}/drafts` and the `/{thread_id}` form.
///
/// # A draft that is not there is a **200**, not a 404
///
/// `GetDraft`'s 404 is caught and turned into `ReturnStatusOK` — "if the draft doesn't exist in
/// the server, we don't need to delete". Only a 500 from the lookup reaches the client. So this
/// route is idempotent in a way its neighbours are not, and a port that let the 404 through would
/// break a client that deletes twice.
///
/// # `{user_id}` is read only to be ignored, again
///
/// The draft is fetched for `c.AppContext.Session().UserId`. The path's user id is never
/// consulted — and the ownership check below compares the session against the *fetched draft*,
/// which was fetched by that same session, so it can never fail. Reproduced as written.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, channel_id, thread_id))]
pub async fn delete_draft(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(params): Path<Vec<(String, String)>>,
    request: Request,
) -> Response {
    if !state.app.config().allow_synced_drafts {
        return ApiError::from(AppError::new(
            "deleteDraft",
            "api.drafts.disabled.app_error",
            None,
            String::new(),
            501,
        ))
        .into_response();
    }

    let channel_id = params
        .iter()
        .find(|(name, _)| name == "channel_id")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    tracing::Span::current().record("channel_id", &channel_id);
    // `c.Params.ThreadId` is the empty string on the route without the segment, and the empty
    // string is exactly what `RootId` holds for a channel-level draft — so the two routes differ
    // only in which draft they name, not in what they do.
    let root_id = params
        .iter()
        .find(|(name, _)| name == "thread_id")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    tracing::Span::current().record("thread_id", &root_id);

    let connection_id = request
        .headers()
        .get(CONNECTION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    let draft = match state
        .app
        .get_draft(&session.0.user_id, &channel_id, &root_id)
        .await
    {
        Ok(draft) => draft,
        Err(err) if err.status_code == 404 => return status_ok(),
        Err(err) => return ApiError::from(err).into_response(),
    };

    // Unreachable — the draft was fetched *for* this session — but Go writes it, and a reader
    // comparing the two files should see the same check.
    if session.0.user_id != draft.user_id {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&mm_model::permission::PERMISSION_DELETE_POST],
        ))
        .into_response();
    }

    if let Err(app_error) = state.app.delete_draft(&draft, &connection_id).await {
        return ApiError::from(app_error).into_response();
    }

    status_ok()
}

fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}
