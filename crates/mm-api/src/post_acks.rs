//! Port of `acknowledgePost` and `unacknowledgePost` (api4/post.go:1429, :1468) —
//! `POST` and `DELETE /api/v4/users/{user_id}/posts/{post_id}/ack`.
//!
//! # The licence test is the first statement, and it is a tier test
//!
//! `model.MinimumProfessionalLicense(c.App.Srv().License())` runs **above**
//! `c.RequirePostId().RequireUserId()` and above both permission gates, so on a server below the
//! Professional rung — no licence, or a licence whose SKU the tier ladder does not rank — a
//! malformed `{post_id}`, a `{user_id}` the caller may not act for and a post in a channel the
//! caller cannot read all answer the same 501. Nothing else about the request is consulted.
//! Until 2026-09-13 that refusal was this module's whole content, in `licensed_features.rs`; the
//! licence is readable now and the pair lives here, beside the work it gates ([D-422]).
//!
//! # The pair does not share an error id, and one of them has no id at all
//!
//! | route | `id` on the wire |
//! |---|---|
//! | `POST` | `<untranslated>` — `model.NoTranslation` |
//! | `DELETE` | `license_error.feature_unavailable` |
//!
//! Both carry the same `detailed_error` in Go ("feature is not available for the current
//! license"), and both have it wiped before it reaches a client, so the id is the *only* thing
//! separating them. `<untranslated>` contains `<` and `>`, so it reaches the wire as
//! `<untranslated>` — see [`crate::error::ApiError::into_wire`]. Go passes `""` as the
//! `where` for both; `AppError.Where` is `json:"-"`, so the handler names kept here are for the
//! trace.
//!
//! # Past the gate, in Go's order
//!
//! 1. `RequirePostId` then `RequireUserId` — 400 `api.context.invalid_url_param.app_error`
//!    naming `post_id` first; `me` resolves to the session's user in the second.
//! 2. `SessionHasPermissionToUser` — 403 naming `edit_other_users`.
//! 3. `SessionHasPermissionToReadPost` — 403 naming `read_channel_content`; a post that does not
//!    exist falls into this check's fallback and is a 403 here too, **not** a 404.
//! 4. `DELETE` only: `GetSinglePost`, whose 404 is propagated — reachable only by a post that
//!    vanished between step 3 and here, since step 3 already found its channel.
//! 5. `App.SaveAcknowledgementForPost` / `DeleteAcknowledgementForPost`, see
//!    [`mm_app::App::save_acknowledgement_for_post`].
//!
//! # Two bodies
//!
//! `POST` writes `json.Marshal(acknowledgement)` — the four-field object, `remote_id` absent, **no
//! trailing newline**. `DELETE` writes `ReturnStatusOK`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post_acknowledgement::AcknowledgementWrite;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_READ_CHANNEL_CONTENT, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::resolve_me;
use crate::error::ApiError;
use crate::proxy;

/// `acknowledgePost` refuses with `model.NoTranslation` as its **id**, so the literal
/// `<untranslated>` lands in both `id` and `message`. Not a placeholder this port chose — it is
/// the id Go sends.
const ACKNOWLEDGE_POST_LICENSE_ERROR: &str = mm_model::utils::NO_TRANSLATION;

/// `unacknowledgePost` — four lines below its twin and a different id.
const UNACKNOWLEDGE_POST_LICENSE_ERROR: &str = "license_error.feature_unavailable";

/// The detail both refusals carry, wiped before the wire — kept because the trace has it.
const LICENSE_DETAIL: &str = "feature is not available for the current license";

/// Everything both handlers share ahead of the app call, in Go's order: the tier test, the two
/// id checks, the two permission gates. `Ok((post_id, user_id))` is the pair to act on, with
/// `me` resolved.
async fn gate(
    state: &AppState,
    session: &AuthenticatedSession,
    license_error_id: &'static str,
    where_: &'static str,
    user_id: &str,
    post_id: &str,
) -> Result<(String, String), ApiError> {
    let license = state.app.license().await?;
    let licensed = mm_model::license::minimum_professional_license(license.as_deref());
    tracing::Span::current().record("licensed", licensed);
    if !licensed {
        return Err(ApiError::from(refusal(where_, license_error_id)));
    }

    // `c.RequirePostId().RequireUserId()` — post first, and `me` resolved in the second.
    if !is_valid_id(post_id) {
        return Err(ApiError::invalid_url_param("post_id"));
    }
    let user_id = resolve_me(user_id, session);
    if !is_valid_id(user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let (may_read, _) = state
        .app
        .session_has_permission_to_read_post(&session.0, post_id)
        .await;
    if !may_read {
        return Err(ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    Ok((post_id.to_owned(), user_id.to_owned()))
}

fn refusal(where_: &'static str, id: &'static str) -> AppError {
    AppError::new(where_, id, None, LICENSE_DETAIL.to_owned(), 501)
}

/// Port of `acknowledgePost` (api4/post.go:1429).
#[tracing::instrument(skip_all, fields(post_id = %post_id, licensed, forwarded = false))]
pub async fn acknowledge_post(
    State(state): State<AppState>,
    Path((user_id, post_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (post_id, user_id) = match gate(
        &state,
        &session,
        ACKNOWLEDGE_POST_LICENSE_ERROR,
        "acknowledgePost",
        &user_id,
        &post_id,
    )
    .await
    {
        Ok(ids) => ids,
        Err(err) => return err.into_response(),
    };

    match state
        .app
        .save_acknowledgement_for_post(&post_id, &user_id)
        .await
    {
        Ok(AcknowledgementWrite::Done(acknowledgement)) => {
            match serde_json::to_vec(&acknowledgement) {
                // `json.Marshal` + `w.Write` — no encoder, no trailing newline.
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
                    tracing::error!(error = %err, "failed to serialise the acknowledgement");
                    ApiError::from(AppError::new(
                        "acknowledgePost",
                        "api.marshal_error",
                        None,
                        String::new(),
                        500,
                    ))
                    .into_response()
                }
            }
        }
        Ok(AcknowledgementWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the acknowledgement to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => ApiError::from(*err).into_response(),
    }
}

/// Port of `unacknowledgePost` (api4/post.go:1468).
#[tracing::instrument(skip_all, fields(post_id = %post_id, licensed, forwarded = false))]
pub async fn unacknowledge_post(
    State(state): State<AppState>,
    Path((user_id, post_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (post_id, user_id) = match gate(
        &state,
        &session,
        UNACKNOWLEDGE_POST_LICENSE_ERROR,
        "unacknowledgePost",
        &user_id,
        &post_id,
    )
    .await
    {
        Ok(ids) => ids,
        Err(err) => return err.into_response(),
    };

    // `c.App.GetSinglePost(c.AppContext, c.Params.PostId, false)` — its error propagated as is.
    if let Err(err) = state.app.get_single_post(&post_id, false).await {
        return ApiError::from(*err).into_response();
    }

    match state
        .app
        .delete_acknowledgement_for_post(&post_id, &user_id)
        .await
    {
        Ok(AcknowledgementWrite::Done(())) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            r#"{"status":"OK"}"#,
        )
            .into_response(),
        Ok(AcknowledgementWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the un-acknowledgement to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => ApiError::from(*err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pair's two ids, and that they are **not** the same string.
    ///
    /// This is the whole parity risk of the refusal. They are four lines apart in `api4/post.go`,
    /// they share a gate, a status and a (wiped) detail, and a reader who copied one into the
    /// other would produce a server that is right on `POST` and wrong on `DELETE` with nothing
    /// else on the wire to show it.
    #[test]
    fn the_acknowledgement_pair_refuses_with_two_different_ids() {
        assert_eq!(ACKNOWLEDGE_POST_LICENSE_ERROR, "<untranslated>");
        assert_eq!(
            UNACKNOWLEDGE_POST_LICENSE_ERROR,
            "license_error.feature_unavailable"
        );
        assert_ne!(
            ACKNOWLEDGE_POST_LICENSE_ERROR, UNACKNOWLEDGE_POST_LICENSE_ERROR,
            "the POST and the DELETE do not share an id"
        );
        // The id is `model.NoTranslation` itself, not a string that merely looks like it — if the
        // model constant moved, this route's wire format moves with it.
        assert_eq!(
            ACKNOWLEDGE_POST_LICENSE_ERROR,
            mm_model::utils::NO_TRANSLATION
        );
    }

    /// `<untranslated>` survives to the wire **escaped**, because Go's `json.Marshal` escapes
    /// `<` and `>` and this project reproduces that. Asserted on the real response rather than on
    /// the constant, since the escaping happens in `into_wire` and not here.
    #[test]
    fn the_acknowledge_refusal_escapes_its_angle_brackets_and_wipes_the_detail() {
        let err = ApiError::from(refusal("acknowledgePost", ACKNOWLEDGE_POST_LICENSE_ERROR));
        let (status, body) = err.into_wire();
        assert_eq!(status.as_u16(), 501);
        let body = String::from_utf8(body.expect("a body")).expect("utf8");
        assert!(
            body.contains(r"\u003cuntranslated\u003e"),
            "angle brackets must be escaped as Go escapes them: {body}"
        );
        assert!(
            !body.contains("<untranslated>"),
            "the raw form must not appear: {body}"
        );
        assert!(
            !body.contains("feature is not available"),
            "the detail is wiped: {body}"
        );
    }
}
