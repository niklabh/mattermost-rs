//! Port of the thread **write** family in `api4/user.go`, registered at user.go:111-118:
//! `updateReadStateAllThreadsByUser` (:4204), `followThreadByUser` (:4172) and
//! `unfollowThreadByUser` (:4140).
//!
//! Kept out of [`crate::users`], which is 3,300 lines and already holds the two thread reads.
//!
//! # Two routes of this family are not here
//!
//! `updateReadStateThreadByUser` (`PUT …/read/{timestamp}`) and `setUnreadThreadByPostId`
//! (`POST …/set_unread/{post_id}`) both reach `App.UpdateThreadReadForUser`, whose first job is
//! `countThreadMentions` — the markdown mention parser plus `Group().GetGroups`,
//! `GetGroupsByChannel` and `GetGroupsByTeam`. None of those store methods exist yet. They stay
//! forwarded; see [D-250].
//!
//! # Every route here answers `{"status":"OK"}`
//!
//! `ReturnStatusOK` writes with `w.Write`, so there is **no trailing newline** — unlike the two
//! thread reads beside them, which go through `json.NewEncoder().Encode` and do have one.
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
use mm_model::utils::is_valid_id;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::resolve_me;
use crate::error::ApiError;

/// `web.ReturnStatusOK` (web/web.go:127) — `w.Write(MapToJSON(...))`, so **no trailing newline**.
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
    use super::first_invalid_following_param;

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
}
