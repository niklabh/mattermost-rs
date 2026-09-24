//! Port of `api4/scheduled_post.go` — all four routes, past `requireScheduledPostsEnabled`.
//!
//! | Route | Handler | Success |
//! |---|---|---|
//! | `POST /api/v4/posts/schedule` | `createSchedulePost` | **201** |
//! | `PUT /api/v4/posts/schedule/{scheduled_post_id}` | `updateScheduledPost` | **201** |
//! | `DELETE /api/v4/posts/schedule/{scheduled_post_id}` | `deleteScheduledPost` | **201** |
//! | `GET /api/v4/posts/scheduled/team/{team_id}` | `getTeamScheduledPosts` | 200 |
//!
//! Every success body is `json.NewEncoder(w).Encode`, so it ends in a newline, and all three
//! writes answer **201 Created** — the update and the delete included, because Go's handlers
//! write `http.StatusCreated` in all three.
//!
//! # What is forwarded, and why
//!
//! Nothing depends on private code. Three branches this server cannot decide are handed to Go
//! whole, each before anything is written: a `card` post type (`FeatureFlags.IntegratedBoards`,
//! [`crate::post_writes::post_card_type_check`]), a direct or group channel with a bot in it
//! under `RestrictDirectMessage = team` (a plugin decision), and a listed file whose
//! mini-preview would have to be generated.
//!
//! # An unknown id is a 500, not a 404
//!
//! `SqlScheduledPostStore.Get` wraps `sql.ErrNoRows` like any driver error, so the handlers'
//! `existingScheduledPost == nil` → 404 branches are dead code and an id naming no row answers
//! `app.{update,delete}_scheduled_post.get_scheduled_post.error` at **500**.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_app::scheduled_post::ScheduledPostWrite;
use mm_model::go_json::{GoFields, fold_name, remap_object_keys};
use mm_model::permission::{PERMISSION_UPLOAD_FILE, PERMISSION_VIEW_TEAM, make_permission_error};
use mm_model::scheduled_post::ScheduledPost;
use mm_model::utils::{AppError, is_valid_id};
use mm_store::scheduled_post_store::ScheduledPostStore;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::feature_gates::scheduled_posts_gate;
use crate::post_writes::{
    check_upload_file_permission_for_new_files, post_card_type_check, post_hardened_mode_check,
    post_priority_check, user_create_post_permission_check,
};
use crate::proxy;

/// `model.ConnectionId` — `Connection-Id`, no `X-` prefix; see `drafts.rs`.
const CONNECTION_ID_HEADER: &str = "Connection-Id";

/// The `json:` names of `model.ScheduledPost` — the embedded `Draft`'s twelve first, since Go
/// inlines them — for `encoding/json`'s case-insensitive key match.
const SCHEDULED_POST_FIELDS: GoFields = GoFields {
    names: &[
        "create_at",
        "update_at",
        "delete_at",
        "user_id",
        "channel_id",
        "root_id",
        "message",
        "type",
        "props",
        "file_ids",
        "metadata",
        "priority",
        "id",
        "scheduled_at",
        "processed_at",
        "error_code",
        "repeat_type",
        "repeat_timezone",
    ],
    nested: &[],
};

/// `SetInvalidParamWithErr("schedule_post", err)` — `detailed_error` is wiped on the wire, so it
/// is [`ApiError::invalid_param`].
fn invalid_body() -> Response {
    ApiError::invalid_param("schedule_post").into_response()
}

/// Decode a body into a scheduled post as `encoding/json` would, keys matched case-insensitively.
///
/// A `null` body is the zero value, not an error — the target is a `model.ScheduledPost`, not a
/// pointer — and an array is always an error. `whole` is `json.Unmarshal` (the update route: the
/// whole body must be one value) as against `Decoder.Decode` (create: the first value, and
/// anything after it is never read).
fn decode(bytes: &[u8], whole: bool) -> Option<(ScheduledPost, serde_json::Value)> {
    if bytes
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'[')
    {
        return None;
    }
    let mut value: serde_json::Value = if whole {
        mm_model::utils::unmarshal_from_json(bytes).ok()?
    } else {
        mm_model::utils::decode_one_from_json(bytes).ok()?
    };
    if value.is_null() {
        return Some((ScheduledPost::default(), value));
    }
    let raw = value.clone();
    // A JSON `null` leaves a Go field as it was — for a fresh struct, its zero value — where
    // serde refuses `null` for a `String` or an `i64`. Every field starts at zero here, so
    // dropping the top-level nulls is exactly Go's answer, for the pointer, map and slice fields
    // (nil either way) as much as for the scalars. `"repeat_type": null` is the case a client
    // sends, and the one [`names_repeat_type`] still sees in `raw`.
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, field| !field.is_null());
    }
    remap_object_keys(&mut value, &SCHEDULED_POST_FIELDS);
    let decoded = serde_json::from_value(value).ok()?;
    Some((decoded, raw))
}

/// Whether the body named `repeat_type` at all — `updateScheduledPost`'s second `Unmarshal`,
/// into a struct whose one field is a `json.RawMessage`.
///
/// The key is matched as Go matches it, **case-insensitively**, and a JSON `null` value counts
/// as present: `RawMessage` implements `Unmarshaler`, so it is handed the literal `null` and
/// becomes non-nil. Present-and-null therefore **ends** a series (the typed decode leaves
/// `repeat_type` empty), where an absent key keeps it.
fn names_repeat_type(raw: &serde_json::Value) -> bool {
    let wanted = fold_name("repeat_type");
    raw.as_object()
        .is_some_and(|object| object.keys().any(|key| fold_name(key) == wanted))
}

fn connection_id(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(CONNECTION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// `json.NewEncoder(w).Encode(v)` after `w.WriteHeader(status)`.
fn encoded<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match mm_model::utils::go_json_marshal(value) {
        Ok(json) => (
            status,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            json + "\n",
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to encode scheduled post to return API response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn forward(
    state: AppState,
    parts: axum::http::request::Parts,
    bytes: axum::body::Bytes,
    why: &str,
) -> impl std::future::Future<Output = Response> {
    tracing::Span::current().record("forwarded", true);
    tracing::debug!(reason = why, "handing the scheduled post request to Go");
    proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes)))
}

/// Port of `scheduledPostChecks` (api4/scheduled_post.go:26), in Go's order: `create_post`,
/// hardened mode, priority, the card type, burn-on-read.
///
/// Unlike `createPostChecks` there is **no** `upload_file` check here — each handler does its
/// own before calling this. The priority read is `metadata.priority`, and the burn-on-read check
/// is handed the scheduled post's own `user_id`: the session's on create, **the body's** on an
/// update, which never overwrites it.
async fn scheduled_post_checks(
    where_: &'static str,
    state: &AppState,
    session: &AuthenticatedSession,
    scheduled_post: &ScheduledPost,
) -> Result<(), PrepareError> {
    user_create_post_permission_check(state, session, &scheduled_post.channel_id).await?;
    post_hardened_mode_check(state, session, scheduled_post.get_props())?;
    post_priority_check(
        where_,
        state,
        session,
        scheduled_post.get_priority(),
        &scheduled_post.root_id,
    )
    .await?;
    post_card_type_check(where_, state, &scheduled_post.draft_type)?;
    state
        .app
        .post_burn_on_read_check(
            where_,
            &scheduled_post.user_id,
            &scheduled_post.channel_id,
            &scheduled_post.draft_type,
        )
        .await
}

/// Port of `createSchedulePost` (api4/scheduled_post.go:73).
///
/// The body's `user_id` is replaced by the session's and `SanitizeInput` zeroes `create_at` and
/// the metadata embeds. A post carrying file ids needs `upload_file` on its channel, checked
/// **before** `create_post` — the opposite order to `createPost`.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, scheduled_posts, licensed, forwarded))]
pub async fn create_schedule_post(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let hook_ctx = crate::plugin_context::hook_context_of(&request, Some(&session.0));
    if let Err(refusal) = scheduled_posts_gate(&state, "createSchedulePost").await {
        return refusal.into_response();
    }

    let (parts, body) = request.into_parts();
    let connection_id = connection_id(&parts.headers);
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return invalid_body();
    };
    let Some((mut scheduled_post, _)) = decode(&bytes, false) else {
        return invalid_body();
    };
    scheduled_post.draft.user_id.clone_from(&session.0.user_id);
    scheduled_post.sanitize_input();

    if scheduled_post
        .file_ids
        .as_deref()
        .is_some_and(|ids| !ids.is_empty())
    {
        let (granted, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &scheduled_post.channel_id,
                &PERMISSION_UPLOAD_FILE,
            )
            .await;
        if !granted {
            return ApiError::from(make_permission_error(
                &session.0,
                &[&PERMISSION_UPLOAD_FILE],
            ))
            .into_response();
        }
    }

    match scheduled_post_checks("Api4.createSchedulePost", &state, &session, &scheduled_post).await
    {
        Ok(()) => {}
        Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            return forward(state, parts, bytes, why).await;
        }
    }

    match state
        .app
        .save_scheduled_post(&hook_ctx, scheduled_post, &connection_id)
        .await
    {
        Ok(ScheduledPostWrite::Done(saved)) => encoded(StatusCode::CREATED, &*saved),
        Ok(ScheduledPostWrite::Forward(why)) => forward(state, parts, bytes, why).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `getTeamScheduledPosts` (api4/scheduled_post.go:121).
///
/// The answer is an **object keyed by team id**, holding a list that is `[]` rather than `null`
/// when empty; with `includeDirectChannels=true` — exactly that string — a second key,
/// `directChannels`, holds the posts in channels with no team. Go encodes the map with sorted
/// keys, which a `BTreeMap` reproduces.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, team_id = %team_id, scheduled_posts, licensed, forwarded))]
pub async fn get_team_scheduled_posts(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(refusal) = scheduled_posts_gate(&state, "getTeamScheduledPosts").await {
        return refusal.into_response();
    }
    if !is_valid_id(&team_id) {
        return ApiError::invalid_url_param("team_id").into_response();
    }
    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return ApiError::from(make_permission_error(&session.0, &[&PERMISSION_VIEW_TEAM]))
            .into_response();
    }

    let user_id = &session.0.user_id;
    let include_direct = mm_model::go_url::parse_query(request.uri().query().unwrap_or_default())
        .0
        .get("includeDirectChannels")
        == Some(b"true".as_slice());

    let mut response: BTreeMap<&str, Vec<ScheduledPost>> = BTreeMap::new();
    let lists = [(team_id.as_str(), team_id.as_str()), ("directChannels", "")];
    for (key, team) in lists.into_iter().take(if include_direct { 2 } else { 1 }) {
        match state.app.get_user_team_scheduled_posts(user_id, team).await {
            Ok(posts) => {
                response.insert(key, posts);
            }
            Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
            Err(PrepareError::Unreproducible(why)) => {
                tracing::Span::current().record("forwarded", true);
                tracing::debug!(reason = why, "handing the scheduled post list to Go");
                return proxy::forward_to_go(State(state), request).await;
            }
        }
    }

    encoded(StatusCode::OK, &response)
}

/// Port of `updateScheduledPost` (api4/scheduled_post.go:163).
///
/// # The order a reader would not guess
///
/// Decode (400 `schedule_post`); the body's `id` against the path (400 `scheduled_post_id` —
/// so a body with no `id` is refused here); the stored row (500 if absent); its owner (**403**
/// `app.update_scheduled_post.update_permission.error`, not a permission error); `repeat_type`
/// carried over when the body did not name it; `upload_file` for new files, measured against the
/// **stored** post; then `scheduledPostChecks` against the **body** — its channel, its user.
///
/// Neither `user_id` nor `create_at` is filled in from the session or the row before the app
/// layer's `IsValid`, so an update must carry both.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, scheduled_post_id = %scheduled_post_id, scheduled_posts, licensed, forwarded))]
pub async fn update_scheduled_post(
    State(state): State<AppState>,
    Path(scheduled_post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let hook_ctx = crate::plugin_context::hook_context_of(&request, Some(&session.0));
    if let Err(refusal) = scheduled_posts_gate(&state, "updateScheduledPost").await {
        return refusal.into_response();
    }

    let (parts, body) = request.into_parts();
    let connection_id = connection_id(&parts.headers);
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return invalid_body();
    };
    let Some((mut scheduled_post, raw)) = decode(&bytes, true) else {
        return invalid_body();
    };
    let repeat_type_named = names_repeat_type(&raw);

    if scheduled_post.id != scheduled_post_id {
        return ApiError::invalid_url_param("scheduled_post_id").into_response();
    }

    let user_id = &session.0.user_id;
    let existing = match state
        .app
        .store()
        .scheduled_post()
        .get(&scheduled_post.id)
        .await
    {
        Ok(existing) => existing,
        Err(err) => {
            tracing::error!(error = %err, "scheduled post lookup failed");
            return ApiError::from(AppError::new(
                "updateScheduledPost",
                "app.update_scheduled_post.get_scheduled_post.error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    if existing.user_id != *user_id {
        return ApiError::from(AppError::new(
            "updateScheduledPost",
            "app.update_scheduled_post.update_permission.error",
            None,
            String::new(),
            403,
        ))
        .into_response();
    }

    // "Clients that predate recurring scheduled posts omit the repeat fields entirely, so an
    // absent repeat_type preserves the existing recurrence rather than ending the series."
    if !repeat_type_named {
        scheduled_post.repeat_type.clone_from(&existing.repeat_type);
        scheduled_post
            .repeat_timezone
            .clone_from(&existing.repeat_timezone);
    }

    if let Some(file_ids) = scheduled_post
        .file_ids
        .as_deref()
        .filter(|ids| !ids.is_empty())
    {
        let original = match existing.to_post() {
            Ok(original) => original,
            Err(err) => {
                tracing::error!(error = %err, "scheduled post does not convert to a post");
                return ApiError::from(AppError::new(
                    "updateScheduledPost",
                    "app.update_scheduled_post.convert_to_post.error",
                    None,
                    String::new(),
                    500,
                ))
                .into_response();
            }
        };
        match check_upload_file_permission_for_new_files(&state, &session, file_ids, &original)
            .await
        {
            Ok(()) => {}
            Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
            Err(PrepareError::Unreproducible(why)) => {
                return forward(state, parts, bytes, why).await;
            }
        }
    }

    match scheduled_post_checks(
        "Api4.updateScheduledPost",
        &state,
        &session,
        &scheduled_post,
    )
    .await
    {
        Ok(()) => {}
        Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            return forward(state, parts, bytes, why).await;
        }
    }

    match state
        .app
        .update_scheduled_post(&hook_ctx, user_id, scheduled_post, &connection_id)
        .await
    {
        Ok(ScheduledPostWrite::Done(updated)) => encoded(StatusCode::CREATED, &*updated),
        Ok(ScheduledPostWrite::Forward(why)) => forward(state, parts, bytes, why).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `deleteScheduledPost` (api4/scheduled_post.go:263).
///
/// The row (500 if absent), its owner (403), then the app layer — which reads the row a second
/// time and answers with that read. **201**, with the deleted post as the body.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, scheduled_post_id = %scheduled_post_id, scheduled_posts, licensed))]
pub async fn delete_scheduled_post(
    State(state): State<AppState>,
    Path(scheduled_post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(refusal) = scheduled_posts_gate(&state, "deleteScheduledPost").await {
        return refusal.into_response();
    }

    let user_id = &session.0.user_id;
    let existing = match state
        .app
        .store()
        .scheduled_post()
        .get(&scheduled_post_id)
        .await
    {
        Ok(existing) => existing,
        Err(err) => {
            tracing::error!(error = %err, "scheduled post lookup failed");
            return ApiError::from(AppError::new(
                "deleteScheduledPost",
                "app.delete_scheduled_post.get_scheduled_post.error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    if existing.user_id != *user_id {
        return ApiError::from(AppError::new(
            "deleteScheduledPost",
            "app.delete_scheduled_post.delete_permission.error",
            None,
            String::new(),
            403,
        ))
        .into_response();
    }

    let connection_id = connection_id(request.headers());
    match state
        .app
        .delete_scheduled_post(user_id, &scheduled_post_id, &connection_id)
        .await
    {
        Ok(deleted) => encoded(StatusCode::CREATED, &deleted),
        Err(err) => ApiError::from(err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_names_are_ascii_and_cover_the_wire_form() {
        assert_eq!(
            mm_model::go_json::non_ascii_field_name(&SCHEDULED_POST_FIELDS),
            None
        );
        let wire = serde_json::to_value(ScheduledPost::default()).unwrap();
        for key in wire.as_object().unwrap().keys() {
            assert!(
                SCHEDULED_POST_FIELDS.names.contains(&key.as_str()),
                "{key} is on the wire and not in the schema"
            );
        }
    }

    #[test]
    fn keys_match_case_insensitively_and_a_null_body_is_the_zero_value() {
        let (post, _) = decode(br#"{"ID":"x","Channel_Id":"c","MESSAGE":"m"}"#, true).unwrap();
        assert_eq!(post.id, "x");
        assert_eq!(post.channel_id, "c");
        assert_eq!(post.message, "m");

        let (post, raw) = decode(br#"{"repeat_type":null,"create_at":null}"#, true).unwrap();
        assert_eq!(post.repeat_type, "", "null leaves the zero value");
        assert!(names_repeat_type(&raw), "and still names the key");

        let (post, raw) = decode(b"null", true).unwrap();
        assert_eq!(post, ScheduledPost::default());
        assert!(!names_repeat_type(&raw));

        assert!(decode(b"[]", true).is_none(), "an array is not a struct");
        assert!(decode(b"[]", false).is_none());
        assert!(decode(br#"{"message":5}"#, false).is_none());
    }

    /// `Unmarshal` wants the whole body; `Decode` stops after the first value.
    #[test]
    fn only_the_update_reads_the_whole_body() {
        assert!(decode(br#"{"id":"x"} trailing"#, true).is_none());
        assert_eq!(decode(br#"{"id":"x"} trailing"#, false).unwrap().0.id, "x");
    }

    #[test]
    fn repeat_type_is_named_by_any_casing_and_by_null() {
        let named = |body: &str| names_repeat_type(&serde_json::from_str(body).unwrap());
        assert!(named(r#"{"repeat_type":""}"#));
        assert!(named(r#"{"repeat_type":null}"#));
        assert!(named(r#"{"Repeat_Type":"weekly"}"#));
        assert!(!named(r#"{"repeat_timezone":"UTC"}"#));
        assert!(!named(r#"{"repeattype":"weekly"}"#));
        assert!(!named("null"));
    }
}
