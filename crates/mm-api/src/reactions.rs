//! Port of the two reaction reads in `channels/api4/reaction.go`:
//! `GET /api/v4/posts/{post_id}/reactions` and `POST /api/v4/posts/ids/reactions`.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::reaction::ReactionWrite;
use mm_model::emoji::EMOJI_NAME_MAX_LENGTH;
use mm_model::permission::{
    PERMISSION_ADD_REACTION, PERMISSION_READ_CHANNEL_CONTENT, PERMISSION_REMOVE_OTHERS_REACTIONS,
    PERMISSION_REMOVE_REACTION, make_permission_error,
};
use mm_model::reaction::Reaction;
use mm_model::utils::{AppError, PAYLOAD_PARSE_ERROR, sorted_array_from_json};
use mm_model::utils::{go_to_lower, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;

/// Port of `getReactions` (api4/reaction.go:74).
///
/// # `null` is the empty answer, and it is load-bearing
///
/// Go writes `json.Marshal(reactions)` where `reactions` is the `[]*model.Reaction` the store
/// left **nil** for a post nothing has reacted to. A nil slice marshals to `null`, not `[]`, so
/// this route answers the four bytes `null` far more often than it answers an array — most
/// posts carry no reactions. `serde_json` renders an empty `Vec` as `[]`, which is why the
/// empty case is spelled out here rather than left to the serialiser.
///
/// # There is no 404
///
/// [`mm_app::App::get_reactions_for_post`] has no not-found branch, so a post id that names
/// nothing answers `null` with a 200 — provided the permission check above it passes. It does
/// not pass for an unknown id: `SessionHasPermissionToReadPost` cannot resolve the channel and
/// falls back to a bare system-level `read_channel_content` check, which an ordinary user
/// fails. So an unknown post is a **403**, and only a system admin sees the `null`.
///
/// # Order
///
/// `RequirePostId` → `SessionHasPermissionToReadPost` → the read. The permission check comes
/// **before** the query here, the reverse of `getPostThread` — see `MIGRATION.md`.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write`, so **no trailing newline** ([D-086]'s rule again). The 403's
/// permission id is `read_channel_content` even when the channel is open and the real refusal
/// came from the `read_public_channel` fallback: `SetPermissionError` is passed a literal, not
/// whatever the check decided.
#[tracing::instrument(skip_all, fields(post_id = %post_id))]
pub async fn get_reactions(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `c.RequirePostId()` (web/context.go:411). The router's `[A-Za-z0-9]+` charset has already
    // turned away the shapes gorilla would 404, leaving a segment of the wrong length to this.
    require_id(&post_id, "post_id")?;

    // The second return value is `is_member`, which Go discards here — `getReactions` builds no
    // audit record ([D-028]).
    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_read_post(&session.0, &post_id)
        .await;
    if !allowed {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    let reactions = state.app.get_reactions_for_post(&post_id).await?;

    let body = if reactions.is_empty() {
        b"null".to_vec()
    } else {
        serde_json::to_vec(&reactions).map_err(|err| {
            tracing::error!(error = %err, "failed to serialise reactions");
            ApiError::from(AppError::new(
                "getReactions",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
        })?
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

/// Port of `getBulkReactions` (api4/reaction.go:118) — `POST /api/v4/posts/ids/reactions`.
///
/// The webapp posts the ids of every post it just loaded, so this fires once per channel switch
/// and is the busier of the two reaction reads by a wide margin.
///
/// # It validates nothing
///
/// There is no `RequirePostId`, no `is_valid_id`, and — unlike `getUsersByIds`,
/// `getRolesByNames` and every other by-ids handler in api4 — **no empty-list check**. A body of
/// `["abc"]` is a legal request that answers `{"abc":[]}` for anyone who clears the permission
/// gate. Only three bodies are refused, and two of them by accident; see below.
///
/// # Order of operations
///
/// `SortedArrayFromJSON` → the permission loop over **every** id → one store call. The gate runs
/// to completion before any read, so a list of fifty readable posts and one unreadable one is a
/// 403 with nothing leaked, and the refusal is the same 403 whichever id caused it.
///
/// # `[]` and `null` are 500s, not 400s
///
/// `SortedArrayFromJSON` returns `(nil, nil)` for a `null` body — no error — and an empty array
/// for `[]`, so both reach the app layer as zero ids. Go's store then builds `PostId IN ()`,
/// which Postgres rejects as a syntax error, and the app wraps that in a 500
/// `app.reaction.bulk_get_for_post_ids.app_error`. Measured against the running Go server, both
/// bodies answer exactly that. It is a bug on Go's side and it is on the wire, so it is ported:
/// the refusal lives in the store, where Go's does, rather than being hoisted into a tidy 400
/// here. The empty-list branch is reached **before** any permission check can fail, so a plain
/// user posting `[]` also gets the 500 rather than a 403.
///
/// A body that is not a JSON array of strings is the one honest refusal: 400
/// `api.payload.parse.error`.
///
/// # Wire format
///
/// `json.Marshal` of a `map[string][]*model.Reaction` then `w.Write` — an object with **bytewise
/// sorted keys**, `[]` (not `null`) for a post with no reactions, and **no trailing newline**.
/// See [`mm_app::App::get_bulk_reactions_for_posts`] for why the empty value differs from
/// [`get_reactions`]'s.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, asked))]
pub async fn get_bulk_reactions(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            payload_parse_error()
        })?;

    let post_ids = sorted_array_from_json(&bytes).map_err(|err| {
        tracing::debug!(error = %err, "post id body did not decode");
        payload_parse_error()
    })?;
    tracing::Span::current().record("asked", post_ids.len());

    for post_id in &post_ids {
        let (allowed, _is_member) = state
            .app
            .session_has_permission_to_read_post(&session.0, post_id)
            .await;
        if !allowed {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&PERMISSION_READ_CHANNEL_CONTENT],
            )));
        }
    }

    let reactions = state.app.get_bulk_reactions_for_posts(&post_ids).await?;

    let body = serde_json::to_vec(&reactions).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise bulk reactions");
        ApiError::from(AppError::new(
            "getBulkReactions",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

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

/// `model.NewAppError("getBulkReactions", model.PayloadParseError, nil, "", 400)`.
fn payload_parse_error() -> ApiError {
    ApiError::from(AppError::new(
        "getBulkReactions",
        PAYLOAD_PARSE_ERROR,
        None,
        String::new(),
        400,
    ))
}

/// Port of `saveReaction` (api4/reaction.go:22) — `POST /api/v4/reactions`.
///
/// # Four gates, four statuses, and the order is load-bearing
///
/// 1. the body must decode — `SetInvalidParamWithErr("reaction")`, 400;
/// 2. **`EmojiName` is lower-cased before validation**, so `:SMILE:` is accepted and stored as
///    `smile`. A port that validated first would reject a name Go normalises;
/// 3. a compound validity test — `IsValidId(UserId) && IsValidId(PostId) && EmojiName != "" &&
///    len(EmojiName) <= 64` — answering **one** id, `api.reaction.save_reaction.invalid.app_error`
///    at 400, whichever part failed. Note this is *not* `Reaction.IsValid()`: it does not test the
///    emoji's character set, so `a b` passes here and is rejected later by the store's validation;
/// 4. reacting as somebody else is **403** with its own id, before any permission check.
///
/// Only then does the channel permission run, and it is `PermissionAddReaction` scoped to the
/// post's channel.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn save_reaction(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();

    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("reaction").into_response();
        }
    };

    let mut reaction: Reaction = match serde_json::from_slice(&bytes) {
        Ok(reaction) => reaction,
        Err(err) => {
            tracing::debug!(error = %err, "reaction body did not decode");
            return ApiError::invalid_param("reaction").into_response();
        }
    };

    // **Before** the length check, and with Go's *simple* case mapping rather than Rust's full
    // one. The order is Go's and it is observable: 22 copies of `İ` are 44 bytes, which Go lowers
    // to 22 bytes of `i` and accepts, while `str::to_lowercase` produces 66 bytes and would fail
    // the 64-byte cap. Measured — Go answers the emoji route's 404, not this handler's 400.
    reaction.emoji_name = go_to_lower(&reaction.emoji_name);

    if !is_valid_id(&reaction.user_id)
        || !is_valid_id(&reaction.post_id)
        || reaction.emoji_name.is_empty()
        || reaction.emoji_name.len() > EMOJI_NAME_MAX_LENGTH
    {
        return ApiError::from(AppError::new(
            "saveReaction",
            "api.reaction.save_reaction.invalid.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    if reaction.user_id != session.0.user_id {
        return ApiError::from(AppError::new(
            "saveReaction",
            "api.reaction.save_reaction.user_id.app_error",
            None,
            String::new(),
            403,
        ))
        .into_response();
    }

    if !state
        .app
        .session_has_permission_to_channel_by_post(
            &session.0,
            &reaction.post_id,
            &PERMISSION_ADD_REACTION,
        )
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_ADD_REACTION],
        ))
        .into_response();
    }

    match state.app.save_reaction_for_post(&reaction).await {
        Ok(ReactionWrite::Done(saved)) => {
            tracing::Span::current().record("forwarded", false);
            // `json.NewEncoder(w).Encode(re)` — an **encoder**, so the body carries a trailing
            // newline. `getReactions` beside it uses `json.Marshal` + `w.Write` and does not.
            // Two adjacent handlers in one file, two framings.
            match mm_model::utils::go_json_marshal(&saved) {
                Ok(json) => (
                    StatusCode::OK,
                    [
                        ("Content-Type", "application/json"),
                        ("x-mmrs-served-by", "rust"),
                    ],
                    json + "\n",
                )
                    .into_response(),
                Err(err) => {
                    tracing::warn!(error = %err, "Error while writing response");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        Ok(ReactionWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(?why, "forwarding the reaction to Go");
            let request = Request::from_parts(parts, Body::from(bytes));
            crate::proxy::forward_to_go(State(state), request).await
        }
        Err(app_error) => ApiError::from(app_error).into_response(),
    }
}

/// Port of `deleteReaction` (api4/reaction.go:82) —
/// `DELETE /api/v4/users/{user_id}/posts/{post_id}/reactions/{emoji_name}`.
///
/// # Two permission checks, and the second is conditional
///
/// `PermissionRemoveReaction` on the post's channel is required of everybody. Removing *somebody
/// else's* reaction additionally requires `PermissionRemoveOthersReactions`, which is a **system**
/// permission and not a channel one — so it is checked with `SessionHasPermissionTo`, against the
/// session's roles rather than the channel's.
///
/// The order matters: a user with neither permission, deleting another user's reaction, is
/// refused for `remove_reaction` — the *channel* permission — not for `remove_others_reactions`.
///
/// Unlike `saveReaction`, the three path parameters are validated by `RequireUserId`,
/// `RequirePostId` and `RequireEmojiName`, whose results Go **checks** (`if c.Err != nil`), so a
/// malformed id really is a 400 here.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn delete_reaction(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path((user_id, post_id, emoji_name)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    // **The router's character classes come first, and they are not the same as the Require
    // checks.** gorilla registers this path as
    // `/users/{user_id:[A-Za-z0-9]+}/posts/{post_id:[A-Za-z0-9]+}/reactions/{emoji_name:[A-Za-z0-9\_\-\+]+}`
    // (api.go:127), so a segment outside its class matches **no route** and Go answers its
    // 404 page — not a 400 from `RequireEmojiName`. axum's `{emoji_name}` matches anything, so
    // without this the port answers 400 where Go answers 404.
    //
    // Measured, not read: `DELETE …/reactions/not%20an%20emoji` is
    // `api.context.404.app_error` on the running server, and its `detailed_error` quotes the URL.
    // Forwarding reproduces that body exactly rather than reconstructing it.
    if !matches_gorilla_class(&user_id, false)
        || !matches_gorilla_class(&post_id, false)
        || !matches_gorilla_class(&emoji_name, true)
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }

    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    if let Err(err) = require_id(&post_id, "post_id") {
        return err.into_response();
    }
    // `RequireEmojiName` (web/context.go:640) is **not** an id check: it lower-cases the value and
    // tests it against `[a-zA-Z0-9\-\+_]+` with a 64-character cap. Every value that reaches it
    // has already passed the router's class above, so only the cap can still fire.
    if let Err(err) = require_emoji_name(&emoji_name) {
        return err.into_response();
    }

    let emoji_name = go_to_lower(&emoji_name);

    if !state
        .app
        .session_has_permission_to_channel_by_post(
            &session.0,
            &post_id,
            &PERMISSION_REMOVE_REACTION,
        )
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_REMOVE_REACTION],
        ))
        .into_response();
    }

    if user_id != session.0.user_id
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_REMOVE_OTHERS_REACTIONS)
            .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_REMOVE_OTHERS_REACTIONS],
        ))
        .into_response();
    }

    let reaction = Reaction {
        user_id,
        post_id,
        emoji_name,
        ..Default::default()
    };

    match state.app.delete_reaction_for_post(&reaction).await {
        Ok(ReactionWrite::Done(())) => {
            tracing::Span::current().record("forwarded", false);
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
        Ok(ReactionWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(?why, "forwarding the reaction delete to Go");
            crate::proxy::forward_to_go(State(state), request).await
        }
        Err(app_error) => ApiError::from(app_error).into_response(),
    }
}

/// gorilla's segment classes for this route: `[A-Za-z0-9]+` for the two ids, and the same plus
/// `_`, `-` and `+` for the emoji name (api.go:127).
///
/// A segment failing its class matches no route on Go, so the request is forwarded and Go answers
/// its own 404 — see [`delete_reaction`].
fn matches_gorilla_class(segment: &str, emoji: bool) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || (emoji && (c == '_' || c == '-' || c == '+')))
}

/// Port of `Context.RequireEmojiName` (web/context.go:640).
///
/// Go lower-cases first, then applies `validEmojiNameRegex` — `^[a-zA-Z0-9\-\+_]+$` — and a
/// length cap of `EmojiNameMaxLength`. Since the value is already lower-cased, the `A-Z` half of
/// the class is dead; it is kept because the *source* keeps it and a reader comparing the two
/// should see the same class.
fn require_emoji_name(emoji_name: &str) -> Result<(), ApiError> {
    let lowered = go_to_lower(emoji_name);
    if lowered.is_empty()
        || lowered.len() > EMOJI_NAME_MAX_LENGTH
        || !lowered
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '+' || c == '_')
    {
        return Err(ApiError::invalid_url_param("emoji_name"));
    }
    Ok(())
}
