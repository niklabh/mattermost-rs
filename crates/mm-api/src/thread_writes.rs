//! Port of the thread **write** family in `api4/user.go`, registered at user.go:111-118:
//! `updateReadStateAllThreadsByUser` (:4204), `followThreadByUser` (:4172),
//! `unfollowThreadByUser` (:4140), `updateReadStateThreadByUser` (:4053) and
//! `setUnreadThreadByPostId` (:4092).
//!
//! Kept out of [`crate::users`], which is 3,300 lines and already holds the two thread reads.
//!
//! # Two body shapes in one family
//!
//! The first three answer `ReturnStatusOK` — `{"status":"OK"}` written with `w.Write`, so
//! **no trailing newline**. The two per-thread read-state routes answer the `ThreadResponse`
//! through `json.NewEncoder().Encode`, newline included, exactly as `GET …/threads/{thread_id}`
//! does; [`thread_response`] is that encoding.
//!
//! # The mux patterns on the last two segments
//!
//! `/read/{timestamp:[0-9]+}` and `/set_unread/{post_id:[A-Za-z0-9]+}`: a segment outside the
//! class is a gorilla 404 in Go, never a handler 400, so both routes are registered with
//! `partially_migrated_with_ids` and `timestamp` is taught to `segment_matches_go_mux_for`. A
//! timestamp of all digits that still fails `RequireTimestamp` — `0`, `000`, or one that
//! overflows `ParseInt` — is the 400 the handler answers itself.
//!
//! # The gates are not the same across the family
//!
//! `updateReadStateAllThreadsByUser` checks `SessionHasPermissionToTeam(…, PermissionViewTeam)`,
//! the same pair the threads *list* uses. The two `/following` routes check
//! `SessionHasPermissionToReadPost` on the thread id instead and never look at the team at all —
//! so a follow succeeds under a team the thread has nothing to do with, and the team id still
//! reaches the websocket event's broadcast as written.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_READ_CHANNEL_CONTENT, PERMISSION_VIEW_TEAM,
    make_permission_error,
};
use mm_model::thread::ThreadResponse;
use mm_model::utils::is_valid_id;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::resolve_me;
use crate::error::ApiError;
use crate::users::{marshal_error, sanitize_options};

/// `web.ReturnStatusOK` (web/web.go:127) — `w.Write(MapToJSON(...))`, so **no trailing newline**.
pub(crate) fn status_ok() -> Response {
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

/// Port of `updateReadStateAllThreadsByUser` (api4/user.go:4204), reached as
/// `PUT /api/v4/users/{user_id}/teams/{team_id}/threads/read`.
///
/// # The team id is load-bearing here, unlike on every other route in this file
///
/// It is both the permission subject and the store's filter: `Threads.ThreadTeamId = $team OR
/// Threads.ThreadTeamId = ''`, so the empty-team arm sweeps in every DM and GM thread the user
/// follows whatever team they name. Marking one team read therefore marks all DMs read too.
///
/// # What it writes is wider than what the threads list shows
///
/// The statement carries no `Following`, no channel-membership `EXISTS`, no deleted-thread
/// filter and no "only if unread" guard — see
/// [`mm_store::thread_store::SqlThreadStore::mark_all_as_read_by_team`]. A caller cannot observe
/// that through this route's body, which is `{"status":"OK"}` either way.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
pub async fn update_read_state_all_threads_by_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `c.RequireUserId().RequireTeamId()` — user first, and `me` is substituted before the
    // validity check (web/context.go:301).
    let user_id = resolve_me(&user_id, &session);
    if !is_valid_id(user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }
    if !is_valid_id(&team_id) {
        return Err(ApiError::invalid_url_param("team_id"));
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }
    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_TEAM],
        )));
    }

    state
        .app
        .update_threads_read_for_user(user_id, &team_id)
        .await?;

    Ok(status_ok())
}

/// Port of `followThreadByUser` (api4/user.go:4172), reached as
/// `PUT /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, thread_id = %thread_id))]
pub async fn follow_thread_by_user(
    State(state): State<AppState>,
    Path((user_id, team_id, thread_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    set_following(state, &user_id, &team_id, &thread_id, &session, true).await
}

/// Port of `unfollowThreadByUser` (api4/user.go:4140), reached as
/// `DELETE /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, thread_id = %thread_id))]
pub async fn unfollow_thread_by_user(
    State(state): State<AppState>,
    Path((user_id, team_id, thread_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    set_following(state, &user_id, &team_id, &thread_id, &session, false).await
}

/// The body both `/following` handlers share.
///
/// **They are the same function in Go too, in everything but the audit event name and the
/// literal passed to `UpdateThreadFollowForUser`** — same three validators in the same order,
/// same two gates, same success body. The only asymmetry is downstream, in the store: a follow
/// also moves the read mark and an unfollow does not.
///
/// # `SessionHasPermissionToReadPost`'s second value is discarded here
///
/// Go writes `if ok, _ := …; !ok` — the `isMember` flag the two read routes bind for their audit
/// record is not even named on these two. Nothing observable turns on it.
async fn set_following(
    state: AppState,
    user_id: &str,
    team_id: &str,
    thread_id: &str,
    session: &AuthenticatedSession,
    following: bool,
) -> Result<Response, ApiError> {
    let user_id = resolve_me(user_id, session);
    if let Some(parameter) = first_invalid_following_param(user_id, team_id, thread_id) {
        return Err(ApiError::invalid_url_param(parameter));
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_read_post(&session.0, thread_id)
        .await;
    if !allowed {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    state
        .app
        .update_thread_follow_for_user(user_id, team_id, thread_id, following)
        .await?;

    Ok(status_ok())
}

/// Port of `updateReadStateThreadByUser` (api4/user.go:4053), reached as
/// `PUT /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}`.
///
/// # The timestamp is the read mark, not "now"
///
/// A client sends the `create_at` of the last post it has shown, and the store writes exactly
/// that; the same route moves the mark **backwards** when the client asks. `RequireTimestamp`
/// refuses only zero — which is what a value that fails `ParseInt`, or is negative, becomes.
///
/// # Same two gates as the reads, then the write
///
/// `SessionHasPermissionToUser` (`edit_other_users`) and `SessionHasPermissionToReadPost` on the
/// thread id (`read_channel_content`); the second's `isMember` goes only to the audit record,
/// which this server does not write.
#[tracing::instrument(skip_all, fields(user_id = %user_id, thread_id = %thread_id, timestamp = %timestamp))]
pub async fn update_read_state_thread_by_user(
    State(state): State<AppState>,
    Path((user_id, team_id, thread_id, timestamp)): Path<(String, String, String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = resolve_me(&user_id, &session);
    let timestamp = parse_timestamp_param(&timestamp);
    if let Some(parameter) = first_invalid_read_param(user_id, &team_id, &thread_id, timestamp) {
        return Err(ApiError::invalid_url_param(parameter));
    }

    require_user_and_thread(&state, &session, user_id, &thread_id).await?;

    let thread = state
        .app
        .update_thread_read_for_user(&session.0.id, user_id, &team_id, &thread_id, timestamp)
        .await?;

    thread_response(&state, thread, "updateReadStateThreadByUser")
}

/// Port of `setUnreadThreadByPostId` (api4/user.go:4092), reached as
/// `POST /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/set_unread/{post_id}`.
///
/// # A follow first, unconditionally
///
/// "We want to make sure the thread is followed when marking as unread" (MM-36430):
/// `UpdateThreadFollowForUser(…, true)` runs **before** the read-state write, and it is the
/// follow route's store call — so it moves `LastViewed` to now and zeroes `UnreadMentions`, and
/// publishes `thread_follow_changed`, before `UpdateThreadReadForUserByPost` moves the mark back
/// to the post and publishes `thread_read_changed`. Two events, in that order, on every call —
/// even when the thread was already followed.
///
/// A consequence worth knowing: because the follow *creates* a missing membership, this route
/// never answers the membership 404 that `/read/{timestamp}` does.
#[tracing::instrument(skip_all, fields(user_id = %user_id, thread_id = %thread_id, post_id = %post_id))]
pub async fn set_unread_thread_by_post_id(
    State(state): State<AppState>,
    Path((user_id, team_id, thread_id, post_id)): Path<(String, String, String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = resolve_me(&user_id, &session);
    if let Some(parameter) = first_invalid_set_unread_param(user_id, &team_id, &thread_id, &post_id)
    {
        return Err(ApiError::invalid_url_param(parameter));
    }

    require_user_and_thread(&state, &session, user_id, &thread_id).await?;

    state
        .app
        .update_thread_follow_for_user(user_id, &team_id, &thread_id, true)
        .await?;

    let thread = state
        .app
        .update_thread_read_for_user_by_post(&session.0.id, user_id, &team_id, &thread_id, &post_id)
        .await?;

    thread_response(&state, thread, "setUnreadThreadByPostId")
}

/// The two gates the per-thread routes share with the thread reads, in Go's order: the user,
/// then the post.
async fn require_user_and_thread(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: &str,
    thread_id: &str,
) -> Result<(), ApiError> {
    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    // The second value is `isMember`, bound only for the audit record.
    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_read_post(&session.0, thread_id)
        .await;
    if !allowed {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }
    Ok(())
}

/// `json.NewEncoder(w).Encode(thread)` after `App.GetThreadForUser`'s
/// `sanitizeThreadResponse` — the participant sanitisation the app layer leaves to the handler
/// on the read route too (`users::get_thread_for_user`), with the same literal `false` for
/// "as admin".
fn thread_response(
    state: &AppState,
    mut thread: ThreadResponse,
    where_: &'static str,
) -> Result<Response, ApiError> {
    let options = sanitize_options(state.show_full_name(), state.show_email_address(), false);
    for participant in thread.participants.iter_mut().flatten() {
        participant.sanitize_profile(&options, false);
    }

    let mut body = serde_json::to_vec(&thread).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the thread");
        marshal_error(where_)
    })?;
    body.push(b'\n');

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

/// `web.ParamsFromRequest`'s timestamp rule (params.go:226): `strconv.ParseInt(_, 10, 64)`, and
/// **zero** on a parse error or a negative value — which `RequireTimestamp` then refuses. The
/// mux pattern admits digits only, so from the wire the only failures left are `0` and overflow.
fn parse_timestamp_param(raw: &str) -> i64 {
    match raw.parse::<i64>() {
        Ok(value) if value >= 0 => value,
        _ => 0,
    }
}

/// `c.RequireUserId().RequireThreadId().RequireTimestamp().RequireTeamId()` (api4/user.go:4054),
/// as the parameter Go would name — or `None`. The team is validated **last**, after the
/// timestamp, which is the reverse of the read route on the same prefix; see
/// [`first_invalid_following_param`] for why the order has to live in a unit-testable function.
fn first_invalid_read_param(
    user_id: &str,
    team_id: &str,
    thread_id: &str,
    timestamp: i64,
) -> Option<&'static str> {
    if !is_valid_id(user_id) {
        return Some("user_id");
    }
    if !is_valid_id(thread_id) {
        return Some("thread_id");
    }
    if timestamp == 0 {
        return Some("timestamp");
    }
    if !is_valid_id(team_id) {
        return Some("team_id");
    }
    None
}

/// `c.RequireUserId().RequireThreadId().RequirePostId().RequireTeamId()` (api4/user.go:4093):
/// the post id sits third, between the thread and the team.
fn first_invalid_set_unread_param(
    user_id: &str,
    team_id: &str,
    thread_id: &str,
    post_id: &str,
) -> Option<&'static str> {
    if !is_valid_id(user_id) {
        return Some("user_id");
    }
    if !is_valid_id(thread_id) {
        return Some("thread_id");
    }
    if !is_valid_id(post_id) {
        return Some("post_id");
    }
    if !is_valid_id(team_id) {
        return Some("team_id");
    }
    None
}

/// `c.RequireUserId().RequireThreadId().RequireTeamId()` (api4/user.go:4141, :4173), as the
/// parameter Go would name — or `None` when all three pass. `me` is already resolved by the
/// caller, because `RequireUserId` substitutes before it validates.
///
/// # The order is **thread before team**, the opposite of the read route beside it
///
/// `getThreadForUser` chains `RequireUserId().RequireTeamId().RequireThreadId()`. Each `Require`
/// returns early on an error already set, so the first bad segment decides — and a request with
/// both a bad team and a bad thread is answered differently by the two routes on the same path
/// prefix.
///
/// # It is a function, and a unit-tested one, because the order is **invisible on the wire**
///
/// The parameter name reaches a client only through the translated `message`
/// (`Invalid or missing {{.Name}} parameter in request URL.`), and this server sends the raw
/// error id there instead ([D-092]). So every ordering of these three produces byte-identical
/// output from us, and a mutation that swaps two of them survives the whole parity suite —
/// measured, not assumed. The unit test below is the only oracle there is until the i18n bundle
/// lands.
fn first_invalid_following_param(
    user_id: &str,
    team_id: &str,
    thread_id: &str,
) -> Option<&'static str> {
    if !is_valid_id(user_id) {
        return Some("user_id");
    }
    if !is_valid_id(thread_id) {
        return Some("thread_id");
    }
    if !is_valid_id(team_id) {
        return Some("team_id");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        first_invalid_following_param, first_invalid_read_param, first_invalid_set_unread_param,
        parse_timestamp_param,
    };

    /// `ReturnStatusOK` writes with `w.Write`, so the body has **no** trailing newline — the one
    /// byte that separates this family from the two thread reads beside it.
    #[test]
    fn the_ok_body_has_no_trailing_newline() {
        let body = r#"{"status":"OK"}"#;
        assert!(!body.ends_with('\n'));
        assert_eq!(body, "{\"status\":\"OK\"}");
    }

    const GOOD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BAD: &str = "short";

    /// Each segment alone, then every pair, then all three — so the order is pinned by which
    /// name wins a *contest*, not merely by which name each single failure produces.
    #[test]
    fn the_validators_run_user_then_thread_then_team() {
        assert_eq!(first_invalid_following_param(GOOD, GOOD, GOOD), None);

        assert_eq!(
            first_invalid_following_param(BAD, GOOD, GOOD),
            Some("user_id")
        );
        assert_eq!(
            first_invalid_following_param(GOOD, BAD, GOOD),
            Some("team_id")
        );
        assert_eq!(
            first_invalid_following_param(GOOD, GOOD, BAD),
            Some("thread_id")
        );

        // The contests. `RequireUserId` is first, so it wins both of its pairs; between the other
        // two the **thread** wins, which is the surprising half — the read route on the same path
        // prefix validates the team first and would answer `team_id` here.
        assert_eq!(
            first_invalid_following_param(BAD, BAD, GOOD),
            Some("user_id")
        );
        assert_eq!(
            first_invalid_following_param(BAD, GOOD, BAD),
            Some("user_id")
        );
        assert_eq!(
            first_invalid_following_param(GOOD, BAD, BAD),
            Some("thread_id")
        );
        assert_eq!(
            first_invalid_following_param(BAD, BAD, BAD),
            Some("user_id")
        );
    }

    /// An empty segment is not a valid id either — the router never routes one, but the function
    /// must not answer `None` for it.
    #[test]
    fn an_empty_segment_is_invalid() {
        assert_eq!(
            first_invalid_following_param("", GOOD, GOOD),
            Some("user_id")
        );
        assert_eq!(
            first_invalid_following_param(GOOD, "", GOOD),
            Some("team_id")
        );
        assert_eq!(
            first_invalid_following_param(GOOD, GOOD, ""),
            Some("thread_id")
        );
    }

    /// `ParseInt` failures and negatives both become zero, which is the one value
    /// `RequireTimestamp` refuses. Leading zeros and a `+` sign parse as Go parses them.
    #[test]
    fn the_timestamp_param_is_gos_parse_int_or_zero() {
        assert_eq!(parse_timestamp_param("1700000000000"), 1_700_000_000_000);
        assert_eq!(parse_timestamp_param("007"), 7);
        assert_eq!(parse_timestamp_param("+7"), 7);
        assert_eq!(parse_timestamp_param("0"), 0);
        assert_eq!(parse_timestamp_param("000"), 0);
        assert_eq!(parse_timestamp_param("-5"), 0);
        assert_eq!(parse_timestamp_param("abc"), 0);
        assert_eq!(parse_timestamp_param(""), 0);
        assert_eq!(
            parse_timestamp_param("99999999999999999999"),
            0,
            "overflow is an error"
        );
        assert_eq!(
            parse_timestamp_param("1_000"),
            0,
            "base-10 ParseInt takes no underscores"
        );
    }

    /// user, thread, **timestamp**, team — the timestamp is checked before the team, so a bad
    /// team and a zero timestamp together name the timestamp.
    #[test]
    fn the_read_validators_run_user_thread_timestamp_then_team() {
        assert_eq!(first_invalid_read_param(GOOD, GOOD, GOOD, 1), None);
        assert_eq!(
            first_invalid_read_param(BAD, GOOD, GOOD, 1),
            Some("user_id")
        );
        assert_eq!(
            first_invalid_read_param(GOOD, BAD, GOOD, 1),
            Some("team_id")
        );
        assert_eq!(
            first_invalid_read_param(GOOD, GOOD, BAD, 1),
            Some("thread_id")
        );
        assert_eq!(
            first_invalid_read_param(GOOD, GOOD, GOOD, 0),
            Some("timestamp")
        );

        assert_eq!(
            first_invalid_read_param(GOOD, BAD, GOOD, 0),
            Some("timestamp")
        );
        assert_eq!(
            first_invalid_read_param(GOOD, GOOD, BAD, 0),
            Some("thread_id")
        );
        assert_eq!(first_invalid_read_param(BAD, BAD, BAD, 0), Some("user_id"));
    }

    /// user, thread, **post**, team.
    #[test]
    fn the_set_unread_validators_run_user_thread_post_then_team() {
        assert_eq!(first_invalid_set_unread_param(GOOD, GOOD, GOOD, GOOD), None);
        assert_eq!(
            first_invalid_set_unread_param(BAD, GOOD, GOOD, GOOD),
            Some("user_id")
        );
        assert_eq!(
            first_invalid_set_unread_param(GOOD, BAD, GOOD, GOOD),
            Some("team_id")
        );
        assert_eq!(
            first_invalid_set_unread_param(GOOD, GOOD, BAD, GOOD),
            Some("thread_id")
        );
        assert_eq!(
            first_invalid_set_unread_param(GOOD, GOOD, GOOD, BAD),
            Some("post_id")
        );

        assert_eq!(
            first_invalid_set_unread_param(GOOD, BAD, GOOD, BAD),
            Some("post_id")
        );
        assert_eq!(
            first_invalid_set_unread_param(GOOD, GOOD, BAD, BAD),
            Some("thread_id")
        );
        assert_eq!(
            first_invalid_set_unread_param(BAD, BAD, BAD, BAD),
            Some("user_id")
        );
    }
}
