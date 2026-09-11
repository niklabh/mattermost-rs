//! Cross-server parity for the thread write family:
//!
//! ```text
//! PUT    /api/v4/users/{user_id}/teams/{team_id}/threads/read
//! PUT    /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following
//! DELETE /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following
//! ```
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity thread_writes
//! ```
//!
//! # A write route cannot be compared by fetching it twice
//!
//! The body is `{"status":"OK"}` whatever happens, so it carries no evidence at all. Every
//! semantic test here runs the write against Go on one thread and against us on a **twin**
//! thread planted with identical prior state, then compares the `ThreadMemberships` rows the two
//! writes left behind. That is the only place the difference between these routes is visible.
//!
//! Prior state is planted with SQL rather than driven through the API, because the API cannot
//! express it: there is no route that sets `LastViewed` to a chosen value without also following,
//! and no route at all that leaves `UnreadMentions` non-zero on demand. `MaintainMembership`
//! reads the row inside its own transaction with no cache in front of it, so a planted row is
//! what both servers see.
//!
//! # `LastViewed` and `LastUpdated` are wall-clock, so they are compared as an ordering
//!
//! Go and we call `GetMillis()` at different instants; the assertions are "moved past the planted
//! value" and "the two columns are equal to each other", which is what a single clock read means
//! and what two would break.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, add_user_to_channel, assert_error_bodies_match_except_known_gaps,
    client, create_channel_typed, create_direct_channel, create_plain_user, create_team,
    go_minted_token, logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
};

/// Planted `LastViewed` — large enough to be a plausible timestamp, small enough that any
/// wall-clock write is unmistakably greater.
const PLANTED_VIEWED: i64 = 1_000_000;
/// Planted `LastUpdated`, distinct from [`PLANTED_VIEWED`] so a port that writes one where it
/// means the other is caught rather than hidden by two equal columns.
const PLANTED_UPDATED: i64 = 2_000_000;
/// Planted `UnreadMentions` — non-zero, so "zeroed" and "left alone" are different answers.
const PLANTED_MENTIONS: i64 = 3;

struct Fixture {
    team_id: String,
    other_team_id: String,
    plain_id: String,
    plain_token: String,
    /// Twin roots, each with a reply, so each has a `Threads` row. Used in pairs — even index is
    /// Go's, odd is ours — and **no two tests share a pair**, because `cargo test` runs the tests
    /// in this binary concurrently and a follow rewrites the row a neighbour planted.
    ///
    /// ```text
    ///  0, 1  the_success_body_is_byte_identical_and_has_no_newline
    ///  2, 3  a_follow_marks_the_thread_read_and_an_unfollow_does_not  (the follow half)
    ///  4, 5  a_follow_marks_the_thread_read_and_an_unfollow_does_not  (the unfollow half)
    ///  6, 7  following_an_already_followed_thread_still_moves_the_mark
    ///  8, 9  unfollowing_an_unfollowed_thread_writes_nothing
    /// 10,11  unfollowing_a_thread_with_no_row_creates_one
    ///    12  the_follow_event_is_identical_on_both   (one thread, followed by both servers)
    /// 13,14  marking_all_read_sweeps_the_team_and_the_dms_only  (rows owned by the sweepers)
    ///    15  the_me_alias_resolves_to_the_session_user
    /// ```
    roots: Vec<String>,
    /// A root **with no replies**, so there is no `Threads` row behind it at all.
    go_lonely_root: String,
    rs_lonely_root: String,
    /// In a private channel the plain user is not in.
    unreadable_root: String,
    /// The sweep users for `PUT …/threads/read`, one per server, so one sweep cannot disturb the
    /// other's rows — `MarkAllAsReadByTeam` is scoped to a user and a team and nothing else.
    go_sweeper: common::PlainUser,
    rs_sweeper: common::PlainUser,
    /// A thread whose `ThreadTeamId` is `''` because its channel is a DM.
    dm_root: String,
    /// A thread in `other_team_id`.
    other_team_root: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "thrw").await;
            let other_team_id = create_team(client, token, "thrwb").await;
            let channel = create_channel_typed(client, token, &team_id, "thrw", "O").await;
            let other_channel =
                create_channel_typed(client, token, &other_team_id, "thrwb", "O").await;
            let closed = create_channel_typed(client, token, &team_id, "thrwshut", "P").await;

            let plain = create_plain_user(client, token, &team_id, "thrw").await;
            add_user_to_channel(client, token, &channel, &plain.id).await;

            // Sixteen roots, each with one reply so a `Threads` row exists. The
            // reply is the admin's, not the plain user's: a reply *by* the plain user would have
            // Go write them a membership from a goroutine, and every test here plants the row it
            // wants rather than racing that.
            let mut roots = Vec::new();
            for index in 0..16 {
                let root =
                    post_message(client, token, &channel, &format!("root {index}"), None).await;
                post_message(client, token, &channel, "reply", Some(&root)).await;
                roots.push(root);
            }

            let go_lonely_root = post_message(client, token, &channel, "no replies", None).await;
            let rs_lonely_root = post_message(client, token, &channel, "no replies", None).await;

            let unreadable_root = post_message(client, token, &closed, "not yours", None).await;
            post_message(client, token, &closed, "reply", Some(&unreadable_root)).await;

            let go_sweeper = create_plain_user(client, token, &team_id, "thrwgo").await;
            let rs_sweeper = create_plain_user(client, token, &team_id, "thrwrs").await;

            let dm = create_direct_channel(client, token, logged_in_user_id(), &plain.id).await;
            let dm_root = post_message(client, token, &dm, "dm root", None).await;
            post_message(client, token, &dm, "reply", Some(&dm_root)).await;

            let other_team_root =
                post_message(client, token, &other_channel, "other team", None).await;
            post_message(
                client,
                token,
                &other_channel,
                "reply",
                Some(&other_team_root),
            )
            .await;

            Fixture {
                team_id,
                other_team_id,
                plain_id: plain.id,
                plain_token: plain.token,
                roots,
                go_lonely_root,
                rs_lonely_root,
                unreadable_root,
                go_sweeper,
                rs_sweeper,
                dm_root,
                other_team_root,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// the database, which is where a write route's evidence is
// ---------------------------------------------------------------------------------------------

/// One short-lived connection, capped so a missing database fails fast rather than sitting on
/// sqlx's 30-second default.
async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set for this suite");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the fixture database is reachable")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Membership {
    following: bool,
    last_viewed: i64,
    last_updated: i64,
    unread_mentions: i64,
}

/// Put a `ThreadMemberships` row into an exact state. `INSERT … ON CONFLICT DO UPDATE` so a
/// re-run of the suite plants over whatever the last run left.
async fn plant_membership(user_id: &str, post_id: &str, state: Membership) {
    let pool = test_pool().await;
    sqlx::query(
        "INSERT INTO threadmemberships (postid, userid, following, lastviewed, lastupdated, unreadmentions) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (postid, userid) DO UPDATE \
            SET following = $3, lastviewed = $4, lastupdated = $5, unreadmentions = $6",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(state.following)
    .bind(state.last_viewed)
    .bind(state.last_updated)
    .bind(state.unread_mentions)
    .execute(&pool)
    .await
    .expect("the membership plants");
}

async fn drop_membership(user_id: &str, post_id: &str) {
    let pool = test_pool().await;
    sqlx::query("DELETE FROM threadmemberships WHERE userid = $1 AND postid = $2")
        .bind(user_id)
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("the membership clears");
}

async fn read_membership(user_id: &str, post_id: &str) -> Option<Membership> {
    let pool = test_pool().await;
    let row: Option<(bool, i64, i64, i64)> = sqlx::query_as(
        "SELECT COALESCE(following, FALSE), COALESCE(lastviewed, 0), COALESCE(lastupdated, 0), \
                COALESCE(unreadmentions, 0) \
           FROM threadmemberships WHERE userid = $1 AND postid = $2",
    )
    .bind(user_id)
    .bind(post_id)
    .fetch_optional(&pool)
    .await
    .expect("the membership reads");
    row.map(
        |(following, last_viewed, last_updated, unread_mentions)| Membership {
            following,
            last_viewed,
            last_updated,
            unread_mentions,
        },
    )
}

/// The planted state every follow/unfollow test starts from, parameterised on `following`.
fn planted(following: bool) -> Membership {
    Membership {
        following,
        last_viewed: PLANTED_VIEWED,
        last_updated: PLANTED_UPDATED,
        unread_mentions: PLANTED_MENTIONS,
    }
}

// ---------------------------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------------------------

fn following_path(user_id: &str, team_id: &str, thread_id: &str) -> String {
    format!("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following")
}

fn read_all_path(user_id: &str, team_id: &str) -> String {
    format!("/api/v4/users/{user_id}/teams/{team_id}/threads/read")
}

/// One request. The `x-mmrs-served-by` header is **not** asserted here: one test in this suite
/// is about a method we deliberately forward, so the check belongs at the call sites that mean
/// it rather than in the helper.
async fn send(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    token: &str,
    path: &str,
) -> (u16, Vec<u8>) {
    let (status, body, _) = send_with_header(client, base, method, token, path).await;
    (status, body)
}

/// [`send`] with the `x-mmrs-served-by` header returned alongside, for the tests that assert
/// which server answered.
async fn send_with_header(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    token: &str,
    path: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        served_by,
    )
}

async fn follow(client: &reqwest::Client, base: &str, token: &str, path: &str) -> (u16, Vec<u8>) {
    send(client, base, reqwest::Method::PUT, token, path).await
}

async fn unfollow(client: &reqwest::Client, base: &str, token: &str, path: &str) -> (u16, Vec<u8>) {
    send(client, base, reqwest::Method::DELETE, token, path).await
}

/// The same write on both servers against *different* threads, with the answers returned in
/// `(go, rust)` order.
async fn both<'a, F, Fut>(
    client: &'a reqwest::Client,
    token: &'a str,
    go_path: &'a str,
    rs_path: &'a str,
    run: F,
) -> ((u16, Vec<u8>), (u16, Vec<u8>))
where
    F: Fn(&'a reqwest::Client, &'static str, &'a str, &'a str) -> Fut,
    Fut: std::future::Future<Output = (u16, Vec<u8>)>,
{
    let go = run(client, GO, token, go_path).await;
    let rs = run(client, RUST, token, rs_path).await;
    (go, rs)
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// `ReturnStatusOK` — `{"status":"OK"}` with **no** trailing newline, unlike the thread reads.
#[tokio::test]
async fn the_success_body_is_byte_identical_and_has_no_newline() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let go_path = following_path(&f.plain_id, &f.team_id, &f.roots[0]);
    let rs_path = following_path(&f.plain_id, &f.team_id, &f.roots[1]);
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, follow).await;

    assert_eq!(go.0, 200, "Go: {}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, go.0);
    assert_eq!(
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    assert_eq!(go.1, br#"{"status":"OK"}"#.to_vec());
    assert_ne!(go.1.last(), Some(&b'\n'), "w.Write, not Encode");

    // The whole family is answered here, not forwarded — checked on paths that **400 before
    // they write anything**, because a probe that actually swept this user's threads would race
    // every other test in this file.
    for (method, path) in [
        (
            reqwest::Method::PUT,
            following_path(&f.plain_id, &f.team_id, "short"),
        ),
        (
            reqwest::Method::DELETE,
            following_path(&f.plain_id, &f.team_id, "short"),
        ),
        (reqwest::Method::PUT, read_all_path("short", &f.team_id)),
    ] {
        let (_, _, served_by) =
            send_with_header(&client, RUST, method.clone(), &f.plain_token, &path).await;
        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{method} {path} must be served locally"
        );
    }

    // And the same for the DELETE half.
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, unfollow).await;
    assert_eq!(go.0, 200);
    assert_eq!(rs.0, go.0);
    assert_eq!(go.1, rs.1);
}

/// **The one line a reader gets wrong**: `UpdateViewedTimestamp` is `state`, not `true`.
///
/// So a follow moves `LastViewed` to now and zeroes `UnreadMentions`, and an unfollow leaves both
/// exactly where they were while still moving `LastUpdated`. Planted state makes "left alone" and
/// "rewritten to the same value" different answers.
#[tokio::test]
async fn a_follow_marks_the_thread_read_and_an_unfollow_does_not() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // --- follow, from a row that is not following ---
    let (go_root, rs_root) = (&f.roots[2], &f.roots[3]);
    plant_membership(&f.plain_id, go_root, planted(false)).await;
    plant_membership(&f.plain_id, rs_root, planted(false)).await;

    let go_path = following_path(&f.plain_id, &f.team_id, go_root);
    let rs_path = following_path(&f.plain_id, &f.team_id, rs_root);
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, follow).await;
    assert_eq!(go.0, 200);
    assert_eq!(rs.0, 200);

    for (who, root) in [("Go", go_root), ("us", rs_root)] {
        let after = read_membership(&f.plain_id, root)
            .await
            .unwrap_or_else(|| panic!("{who} kept the row"));
        assert!(after.following, "{who}: the follow set Following");
        assert_eq!(
            after.unread_mentions, 0,
            "{who}: UpdateViewedTimestamp zeroes the mentions"
        );
        assert!(
            after.last_viewed > PLANTED_UPDATED,
            "{who}: LastViewed moved to now, not {}",
            after.last_viewed
        );
        assert_eq!(
            after.last_viewed, after.last_updated,
            "{who}: one GetMillis() feeds both columns"
        );
    }

    // --- unfollow, from a row that is following, with unread state to preserve ---
    let (go_root, rs_root) = (&f.roots[4], &f.roots[5]);
    plant_membership(&f.plain_id, go_root, planted(true)).await;
    plant_membership(&f.plain_id, rs_root, planted(true)).await;

    let go_path = following_path(&f.plain_id, &f.team_id, go_root);
    let rs_path = following_path(&f.plain_id, &f.team_id, rs_root);
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, unfollow).await;
    assert_eq!(go.0, 200);
    assert_eq!(rs.0, 200);

    for (who, root) in [("Go", go_root), ("us", rs_root)] {
        let after = read_membership(&f.plain_id, root)
            .await
            .unwrap_or_else(|| panic!("{who} kept the row"));
        assert!(!after.following, "{who}: the unfollow cleared Following");
        assert_eq!(
            after.last_viewed, PLANTED_VIEWED,
            "{who}: an unfollow must not touch the read mark"
        );
        assert_eq!(
            after.unread_mentions, PLANTED_MENTIONS,
            "{who}: nor the mention count"
        );
        assert!(
            after.last_updated > PLANTED_UPDATED,
            "{who}: but LastUpdated does move"
        );
    }
}

/// Following a thread you already follow is **not** a no-op: the read mark still jumps to now.
///
/// `followingNeedsUpdate` is false, but `UpdateViewedTimestamp` is `true` for a follow, so the
/// guarded branch still fires. A port that short-circuited on "already following" would leave a
/// stale `LastViewed` behind and no test of the response body could see it.
#[tokio::test]
async fn following_an_already_followed_thread_still_moves_the_mark() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go_root, rs_root) = (&f.roots[6], &f.roots[7]);
    plant_membership(&f.plain_id, go_root, planted(true)).await;
    plant_membership(&f.plain_id, rs_root, planted(true)).await;

    let go_path = following_path(&f.plain_id, &f.team_id, go_root);
    let rs_path = following_path(&f.plain_id, &f.team_id, rs_root);
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, follow).await;
    assert_eq!(go.0, 200);
    assert_eq!(rs.0, 200);

    for (who, root) in [("Go", go_root), ("us", rs_root)] {
        let after = read_membership(&f.plain_id, root)
            .await
            .unwrap_or_else(|| panic!("{who} kept the row"));
        assert!(after.following, "{who}");
        assert!(
            after.last_viewed > PLANTED_UPDATED,
            "{who}: a redundant follow still marks the thread read"
        );
        assert_eq!(after.unread_mentions, 0, "{who}");
    }
}

/// The mirror image: unfollowing a thread that is already unfollowed writes **nothing at all**,
/// so `LastUpdated` does not move.
///
/// `followingNeedsUpdate` is false and both `IncrementMentions` and `UpdateViewedTimestamp` are
/// false for an unfollow, so the whole update branch is skipped. `LastUpdated` is what the
/// websocket reconnect query and the retention policy read, so an unconditional write here would
/// be visible well outside this route.
#[tokio::test]
async fn unfollowing_an_unfollowed_thread_writes_nothing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go_root, rs_root) = (&f.roots[8], &f.roots[9]);
    plant_membership(&f.plain_id, go_root, planted(false)).await;
    plant_membership(&f.plain_id, rs_root, planted(false)).await;

    let go_path = following_path(&f.plain_id, &f.team_id, go_root);
    let rs_path = following_path(&f.plain_id, &f.team_id, rs_root);
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, unfollow).await;
    assert_eq!(go.0, 200);
    assert_eq!(rs.0, 200);

    for (who, root) in [("Go", go_root), ("us", rs_root)] {
        let after = read_membership(&f.plain_id, root)
            .await
            .unwrap_or_else(|| panic!("{who} kept the row"));
        assert_eq!(
            after,
            planted(false),
            "{who}: the row is untouched, LastUpdated included"
        );
    }
}

/// Unfollowing a thread you have **no row for** creates one, following `false`.
///
/// The insert branch is not guarded by any of the three update flags, and it takes its
/// `Following` from `opts.Following` regardless of `UpdateFollowing`. The row it leaves changes
/// what `GET …/threads/{id}` answers: the same caller moves from
/// `app.user.get_thread_membership_for_user.not_found` to
/// `app.user.get_threads_for_user.not_found`.
#[tokio::test]
async fn unfollowing_a_thread_with_no_row_creates_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go_root, rs_root) = (&f.roots[10], &f.roots[11]);
    drop_membership(&f.plain_id, go_root).await;
    drop_membership(&f.plain_id, rs_root).await;

    let go_path = following_path(&f.plain_id, &f.team_id, go_root);
    let rs_path = following_path(&f.plain_id, &f.team_id, rs_root);
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, unfollow).await;
    assert_eq!(go.0, 200);
    assert_eq!(rs.0, 200);

    for (who, root) in [("Go", go_root), ("us", rs_root)] {
        let after = read_membership(&f.plain_id, root)
            .await
            .unwrap_or_else(|| panic!("{who} inserted the row"));
        assert!(!after.following, "{who}");
        assert_eq!(
            after.last_viewed, 0,
            "{who}: UpdateViewedTimestamp is false, so LastViewed stays zero"
        );
        assert_eq!(after.unread_mentions, 0, "{who}");
        assert!(after.last_updated > 0, "{who}: LastUpdated is now");
    }

    // And the read route now answers the store's 404 rather than the lookup's — the observable
    // consequence of a row existing.
    let read = format!(
        "/api/v4/users/{}/teams/{}/threads/{}",
        f.plain_id, f.team_id, go_root
    );
    let response = client
        .get(format!("{GO}{read}"))
        .header("Authorization", format!("Bearer {}", f.plain_token))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.expect("JSON");
    assert_eq!(body["id"], "app.user.get_threads_for_user.not_found");
}

/// A root post with **no replies has no `Threads` row**, and the follow still succeeds.
///
/// `Thread().Get` returns `nil, nil` there — the one getter in that file that does — and
/// `UpdateThreadFollowForUser` reads `ReplyCount` off it only for the websocket event. A port
/// that treated the miss as `ErrNotFound` would 500 the first follow of every fresh root.
#[tokio::test]
async fn following_a_root_with_no_thread_row_succeeds() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The premise, not an assumption: no `Threads` row behind either root.
    let pool = test_pool().await;
    for root in [&f.go_lonely_root, &f.rs_lonely_root] {
        let count: (i64,) = sqlx::query_as("SELECT count(*) FROM threads WHERE postid = $1")
            .bind(root)
            .fetch_one(&pool)
            .await
            .expect("the count runs");
        assert_eq!(count.0, 0, "{root} must have no thread metadata");
    }

    drop_membership(&f.plain_id, &f.go_lonely_root).await;
    drop_membership(&f.plain_id, &f.rs_lonely_root).await;

    let go_path = following_path(&f.plain_id, &f.team_id, &f.go_lonely_root);
    let rs_path = following_path(&f.plain_id, &f.team_id, &f.rs_lonely_root);
    let (go, rs) = both(&client, &f.plain_token, &go_path, &rs_path, follow).await;
    assert_eq!(go.0, 200, "Go: {}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, 200, "us: {}", String::from_utf8_lossy(&rs.1));

    for (who, root) in [("Go", &f.go_lonely_root), ("us", &f.rs_lonely_root)] {
        let after = read_membership(&f.plain_id, root)
            .await
            .unwrap_or_else(|| panic!("{who} inserted the row"));
        assert!(after.following, "{who}");
        assert!(after.last_viewed > 0, "{who}: a follow sets the mark");
    }
}

/// `PUT …/threads/read` sweeps **this team and the empty-team threads**, and nothing else.
///
/// The statement carries no `Following`, no channel-membership `EXISTS`, no deleted filter and no
/// "only if unread" guard, so the rows it moves include ones the threads list would never show.
/// Both sweep users are planted identically and neither is a member of the DM channel or of the
/// other team's channel — which is exactly what makes the missing membership predicate visible.
#[tokio::test]
async fn marking_all_read_sweeps_the_team_and_the_dms_only() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // One row per thread class, per server's sweeper.
    let in_team = &f.roots[13];
    // Unfollowed and in this team: swept anyway, because there is no `Following` predicate.
    let unfollowed_in_team = &f.roots[14];
    let classes = [
        (in_team.as_str(), true),
        (unfollowed_in_team.as_str(), false),
        (f.dm_root.as_str(), true),
        (f.other_team_root.as_str(), true),
    ];
    for sweeper in [&f.go_sweeper, &f.rs_sweeper] {
        for (root, following) in classes {
            plant_membership(&sweeper.id, root, planted(following)).await;
        }
    }

    let go_path = read_all_path(&f.go_sweeper.id, &f.team_id);
    let rs_path = read_all_path(&f.rs_sweeper.id, &f.team_id);

    // The event is collected here rather than in a test of its own: the sweep is scoped to a
    // user and a team, so two tests firing it at the same sweeper would each see the other's
    // write and neither could assert what it planted.
    let mut go_socket = SocketProbe::connect(GO, &f.go_sweeper.token).await;
    let mut rs_socket = SocketProbe::connect(RUST, &f.rs_sweeper.token).await;

    let go = send(
        &client,
        GO,
        reqwest::Method::PUT,
        &f.go_sweeper.token,
        &go_path,
    )
    .await;
    let rs = send(
        &client,
        RUST,
        reqwest::Method::PUT,
        &f.rs_sweeper.token,
        &rs_path,
    )
    .await;
    assert_eq!(go.0, 200, "Go: {}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, go.0);
    assert_eq!(go.1, rs.1);
    assert_eq!(go.1, br#"{"status":"OK"}"#.to_vec());

    for (who, sweeper) in [("Go", &f.go_sweeper), ("us", &f.rs_sweeper)] {
        for root in [in_team, unfollowed_in_team, &f.dm_root] {
            let after = read_membership(&sweeper.id, root)
                .await
                .unwrap_or_else(|| panic!("{who} kept the row"));
            assert_eq!(after.unread_mentions, 0, "{who}: {root} was swept");
            assert!(
                after.last_viewed > PLANTED_UPDATED,
                "{who}: {root}'s LastViewed moved to now"
            );
            assert_eq!(
                after.last_viewed, after.last_updated,
                "{who}: one GetMillis() feeds both columns"
            );
        }

        let untouched = read_membership(&sweeper.id, &f.other_team_root)
            .await
            .unwrap_or_else(|| panic!("{who} kept the row"));
        assert_eq!(
            untouched,
            planted(true),
            "{who}: another team's thread is outside the sweep"
        );
    }

    // **The event carries no data at all**, unlike the per-thread `thread_read_changed` the
    // `/read/{timestamp}` route publishes under the same name. A client has to re-read the list.
    go_socket.collect_for(Duration::from_millis(900)).await;
    rs_socket.collect_for(Duration::from_millis(900)).await;

    let go_events = go_socket.events_named("thread_read_changed");
    let rs_events = rs_socket.events_named("thread_read_changed");
    assert_eq!(go_events.len(), 1, "Go published one: {:?}", go_socket.raw);
    assert_eq!(rs_events.len(), 1, "we published one: {:?}", rs_socket.raw);
    assert_eq!(
        go_events[0]["data"], rs_events[0]["data"],
        "the sweep event's data differs"
    );
    assert_eq!(go_events[0]["data"], serde_json::json!({}));
    assert_eq!(
        go_events[0]["broadcast"]["team_id"],
        f.team_id.as_str(),
        "addressed to the team"
    );
    assert_eq!(rs_events[0]["broadcast"]["team_id"], f.team_id.as_str());
    assert_eq!(rs_events[0]["broadcast"]["user_id"], f.rs_sweeper.id);
    assert_eq!(rs_events[0]["broadcast"]["channel_id"], "");
}

/// The websocket events, compared frame for frame.
///
/// The same thread is followed by both servers in turn, so `reply_count` and `thread_id` are the
/// same value on both sides and the comparison is about shape *and* content. `state` is a JSON
/// **boolean** — Go's `message.Add` puts the Go value into a `map[string]any` — which is the one
/// thing a stringly-typed port would get wrong without any HTTP-visible symptom.
#[tokio::test]
async fn the_follow_event_is_identical_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let root = &f.roots[12];
    let path = following_path(&f.plain_id, &f.team_id, root);

    let mut go_socket = SocketProbe::connect(GO, &f.plain_token).await;
    let mut rs_socket = SocketProbe::connect(RUST, &f.plain_token).await;

    let go = follow(&client, GO, &f.plain_token, &path).await;
    assert_eq!(go.0, 200);
    let rs = follow(&client, RUST, &f.plain_token, &path).await;
    assert_eq!(rs.0, 200);

    go_socket.collect_for(Duration::from_millis(900)).await;
    rs_socket.collect_for(Duration::from_millis(900)).await;

    let go_events = go_socket.events_named("thread_follow_changed");
    let rs_events = rs_socket.events_named("thread_follow_changed");
    assert_eq!(go_events.len(), 1, "Go published one: {:?}", go_socket.raw);
    assert_eq!(rs_events.len(), 1, "we published one: {:?}", rs_socket.raw);

    assert_eq!(
        go_events[0]["data"], rs_events[0]["data"],
        "the follow event's data differs"
    );
    assert_eq!(
        go_events[0]["broadcast"], rs_events[0]["broadcast"],
        "the follow event's addressing differs"
    );
    assert_eq!(rs_events[0]["data"]["thread_id"], root.as_str());
    assert_eq!(
        rs_events[0]["data"]["state"],
        serde_json::Value::Bool(true),
        "`state` is a JSON boolean, not a string"
    );
    assert_eq!(
        rs_events[0]["data"]["reply_count"], 1,
        "the fixture's roots have exactly one reply"
    );
    assert_eq!(rs_events[0]["broadcast"]["team_id"], f.team_id.as_str());
    assert_eq!(rs_events[0]["broadcast"]["user_id"], f.plain_id.as_str());
    assert_eq!(rs_events[0]["broadcast"]["channel_id"], "");
}

/// `me` in the `{user_id}` segment resolves to the session's own user, on both halves.
///
/// `RequireUserId` substitutes the session id **before** `IsValidId` (web/context.go:301), so a
/// port that validates first answers 400 where Go answers 200. The webapp uses the literal on
/// most of these paths.
#[tokio::test]
async fn the_me_alias_resolves_to_the_session_user() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let root = &f.roots[15];
    plant_membership(&f.plain_id, root, planted(false)).await;

    let path = following_path("me", &f.team_id, root);
    // The same path twice, Go then us: a second follow of a followed thread is accepted by both,
    // so the twin-root discipline the rest of this file keeps is not needed for the alias.
    let go = follow(&client, GO, &f.plain_token, &path).await;
    assert_eq!(go.0, 200, "Go: {}", String::from_utf8_lossy(&go.1));
    let rs = follow(&client, RUST, &f.plain_token, &path).await;
    assert_eq!(rs.0, 200, "us: {}", String::from_utf8_lossy(&rs.1));
    assert_eq!(go.1, rs.1);

    let after = read_membership(&f.plain_id, root)
        .await
        .expect("the row is the session user's");
    assert!(after.following, "`me` reached the plain user's own row");

    // And the sweep half takes the alias too. Its subject is a user with no other rows in this
    // file, so the sweep cannot disturb a neighbour.
    let sweep = read_all_path("me", &f.team_id);
    let go = send(
        &client,
        GO,
        reqwest::Method::PUT,
        &f.go_sweeper.token,
        &sweep,
    )
    .await;
    let rs = send(
        &client,
        RUST,
        reqwest::Method::PUT,
        &f.rs_sweeper.token,
        &sweep,
    )
    .await;
    assert_eq!(go.0, 200, "Go: {}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, go.0);
    assert_eq!(go.1, rs.1);
}

/// `RequireUserId().RequireThreadId().RequireTeamId()` — **thread before team**, which is the
/// opposite of the read route beside it. A request with both segments malformed names
/// `thread_id`, and only the ordering says so.
///
/// **The name is asserted against Go's body, not ours, and that is not laziness.** It reaches a
/// client only through the translated `message`, which this server sends as the raw error id
/// ([D-092]) — so no ordering of the three validators is visible in our output, and a mutation
/// that swaps two of them survives this test. `mm_api::thread_writes::first_invalid_following_param`
/// and its unit test exist because of that; this test pins Go's half of the oracle.
#[tokio::test]
async fn the_validator_order_is_user_thread_team() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let cases = [
        (following_path("short", &f.team_id, &f.roots[0]), "user_id"),
        (following_path(&f.plain_id, "short", &f.roots[0]), "team_id"),
        (
            following_path(&f.plain_id, &f.team_id, "short"),
            "thread_id",
        ),
        // Both wrong: the thread is validated first, so that is the name Go reports.
        (
            following_path(&f.plain_id, "short", "alsoshort"),
            "thread_id",
        ),
    ];

    for (path, expected) in cases {
        for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
            let go = send(&client, GO, method.clone(), &token, &path).await;
            let rs = send(&client, RUST, method.clone(), &token, &path).await;
            assert_eq!(go.0, 400, "{method} {path}");
            assert_eq!(rs.0, go.0, "{method} {path}: statuses must match");
            let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
            assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
            assert!(
                body["detailed_error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(expected)
                    || body["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains(expected),
                "{method} {path} must name {expected}: {body}"
            );
        }
    }

    // And the sweep route, whose order is user then team.
    for (path, _expected) in [
        (read_all_path("short", &f.team_id), "user_id"),
        (read_all_path(&f.plain_id, "short"), "team_id"),
    ] {
        let go = send(&client, GO, reqwest::Method::PUT, &token, &path).await;
        let rs = send(&client, RUST, reqwest::Method::PUT, &token, &path).await;
        assert_eq!(go.0, 400, "{path}");
        assert_eq!(rs.0, go.0, "{path}: statuses must match");
        let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    }
}

/// The two gates, which are different gates on the two halves of this family.
#[tokio::test]
async fn the_gates_refuse_the_way_go_refuses() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    // Somebody else's threads: `SessionHasPermissionToUser`, on both halves.
    for path in [
        following_path(admin_id, &f.team_id, &f.roots[0]),
        read_all_path(admin_id, &f.team_id),
    ] {
        let go = send(&client, GO, reqwest::Method::PUT, &f.plain_token, &path).await;
        let rs = send(&client, RUST, reqwest::Method::PUT, &f.plain_token, &path).await;
        assert_eq!(go.0, 403, "{path}");
        assert_eq!(rs.0, go.0, "{path}: statuses must match");
        let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
        assert_eq!(body["id"], "api.context.permissions.app_error");
    }

    // A thread in a channel the caller cannot read: `SessionHasPermissionToReadPost`, and only
    // the `/following` half has it.
    let path = following_path(&f.plain_id, &f.team_id, &f.unreadable_root);
    for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
        let go = send(&client, GO, method.clone(), &f.plain_token, &path).await;
        let rs = send(&client, RUST, method, &f.plain_token, &path).await;
        assert_eq!(go.0, 403, "{path}");
        assert_eq!(rs.0, go.0, "{path}: statuses must match");
        assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
    }

    // A team the caller is not in: `view_team`, and only the sweep has it. The plain user is in
    // `team_id` and not in `other_team_id`.
    let path = read_all_path(&f.plain_id, &f.other_team_id);
    let go = send(&client, GO, reqwest::Method::PUT, &f.plain_token, &path).await;
    let rs = send(&client, RUST, reqwest::Method::PUT, &f.plain_token, &path).await;
    assert_eq!(go.0, 403, "{path}");
    assert_eq!(rs.0, go.0, "{path}: statuses must match");
    assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
}

/// `GET /threads/read` is **not** this route.
///
/// gorilla skips the PUT-only registration and falls through to `/threads/{thread_id}` with
/// `read` as the id, which `RequireThreadId` then rejects. matchit has no method dimension, so
/// the static path wins here for every method — the fallback is what hands the GET to Go, and
/// this test is the reason it has to.
#[tokio::test]
async fn a_get_on_the_read_path_is_still_gos_404_shaped_400() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = read_all_path(&f.plain_id, &f.team_id);
    let go = send(&client, GO, reqwest::Method::GET, &f.plain_token, &path).await;
    let rs = send(&client, RUST, reqwest::Method::GET, &f.plain_token, &path).await;
    assert_eq!(go.0, 400, "Go: {}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, go.0, "{path}: statuses must match");

    // **Not `assert_error_bodies_match_except_known_gaps`.** That helper pins our `message` to
    // the raw error id, which is true of a body we mint and false of one we proxy — a forwarded
    // answer carries Go's *translated* message. Comparing the two bodies field by field, with
    // only `request_id` allowed to differ, is the stronger claim here anyway: the answer is
    // Go's own, not a reproduction of it.
    let mut go_body: serde_json::Value = serde_json::from_slice(&go.1).expect("Go's body is JSON");
    let mut rs_body: serde_json::Value = serde_json::from_slice(&rs.1).expect("our body is JSON");
    for body in [&mut go_body, &mut rs_body] {
        if let Some(object) = body.as_object_mut() {
            object.remove("request_id");
        }
    }
    assert_eq!(go_body, rs_body, "{path}: the forwarded answer is Go's own");
    assert_eq!(go_body["id"], "api.context.invalid_url_param.app_error");
    assert_eq!(
        go_body["message"], "Invalid or missing thread_id parameter in request URL.",
        "gorilla fell through to getThreadForUser with `read` as the thread id"
    );

    let (_, _, served_by) =
        send_with_header(&client, RUST, reqwest::Method::GET, &f.plain_token, &path).await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "the GET on {path} is forwarded, not answered here"
    );
}

/// An unauthenticated request never reaches any of the three handlers.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let user = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    let team = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
    let thread = "cccccccccccccccccccccccccc";

    let cases = [
        (reqwest::Method::PUT, read_all_path(user, team)),
        (reqwest::Method::PUT, following_path(user, team, thread)),
        (reqwest::Method::DELETE, following_path(user, team, thread)),
    ];

    for (method, path) in cases {
        let go = client
            .request(method.clone(), format!("{GO}{path}"))
            .send()
            .await
            .expect("Go answers");
        let rs = client
            .request(method.clone(), format!("{RUST}{path}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(go.status(), 401, "{method} {path}");
        assert_eq!(rs.status(), go.status(), "{method} {path}");
        let go_body = go.bytes().await.expect("body").to_vec();
        let rs_body = rs.bytes().await.expect("body").to_vec();
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    }
}
