//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}` —
//! one thread, which the webapp asks for when a thread is opened.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity thread_for_user
//! ```
//!
//! # The team id is validated and then ignored
//!
//! `RequireTeamId` runs, and nothing after it reads the value — not the permission checks, not
//! the store. A thread answers the same under any team's path, including a team it has nothing
//! to do with. [`the_team_id_is_validated_and_then_never_used`].
//!
//! # A miss is a 404, but not always the same one
//!
//! A thread the caller never replied to has no membership row and answers
//! `app.user.get_thread_membership_for_user.not_found`. One the caller **unfollowed** keeps its
//! row, so the refusal comes one layer deeper, from the store:
//! `app.user.get_threads_for_user.not_found`.
//! [`an_unfollowed_thread_is_the_same_404_as_a_missing_one`].

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, delete_post, fetch_both_raw,
    fetch_both_stable, go_minted_token, logged_in_user_id, post_message, purge_api_fixtures,
    stack_enabled,
};

struct Fixture {
    team_id: String,
    /// A second team, so "any team's path" can be shown rather than asserted.
    other_team_id: String,
    plain_id: String,
    plain_token: String,
    /// Followed by the plain user, with unread state planted.
    followed_root: String,
    /// Followed, then unfollowed — the membership row survives with `Following = false`.
    unfollowed_root: String,
    /// A thread in a private channel the plain user cannot read.
    unreadable_root: String,
    /// In the channel, but has never posted or replied — so no `ThreadMemberships` row.
    bystander_token: String,
    bystander_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "onethr").await;
            let other_team_id = create_team(client, token, "onethrb").await;
            let channel = create_channel_typed(client, token, &team_id, "onethr", "O").await;
            let closed = create_channel_typed(client, token, &team_id, "onethrshut", "P").await;

            let plain = create_plain_user(client, token, &team_id, "onethr").await;
            add_user_to_channel(client, token, &channel, &plain.id).await;

            // Reads the channel, follows nothing. The only actor here with no membership row.
            let bystander = create_plain_user(client, token, &team_id, "onethrby").await;
            add_user_to_channel(client, token, &channel, &bystander.id).await;

            // A thread exists once a root has a reply, and replying is how a user comes to
            // follow it.
            let followed_root = post_message(client, token, &channel, "followed", None).await;
            let seen = post_message(
                client,
                &plain.token,
                &channel,
                "reply",
                Some(&followed_root),
            )
            .await;

            // Three replies, and the read mark sits between the first and the rest, so
            // `unread_replies` is neither zero nor the whole reply count. Every value the
            // subquery's three predicates disagree about now differs:
            //
            // * `> LastViewed` — 1 unread against 2 live replies, so removing the cutoff moves it
            // * `DeleteAt = 0` — one of the newer replies is deleted, so removing the filter moves it
            // * `RootId = t.PostId` — anything else in the database is newer than the mark
            //
            // `LastViewed` is also a large number where `UnreadMentions` is 0, so the two columns
            // the store binds from the membership can no longer be swapped undetectably.
            let unseen =
                post_message(client, token, &channel, "second", Some(&followed_root)).await;
            // Deleted, and newer than the mark: present for the `DeleteAt` predicate to exclude.
            let doomed =
                post_message(client, token, &channel, "doomed", Some(&followed_root)).await;
            delete_post(client, token, &doomed).await;

            // Nothing in this fixture carried a `PostsPriority` row, so the `is_urgent` CASE and
            // the config flag behind it were dead code.
            plant_post_priority(&followed_root, "urgent").await;
            // `sanitizeThreadResponse` strips three props from the embedded post, and none of
            // them can be set through `POST /posts`: Go sanitises them out of client input at
            // creation, so the branch is dead unless the row is written directly. The key is
            // `silent_notification`, not `silent` — a first attempt at this fixture used the
            // shorter name, and *both* servers passed it straight through. `zz_keep` is
            // the control — one key survives, so the response's `props` is a single-entry object
            // and cannot drift on key order ([D-166]).
            plant_post_props(
                &followed_root,
                r#"{"zz_keep": "kept", "add_channel_member": {"post_id": "x"}, "force_notification": true, "silent_notification": true}"#,
            )
            .await;

            let unfollowed_root = post_message(client, token, &channel, "unfollowed", None).await;
            post_message(
                client,
                &plain.token,
                &channel,
                "reply",
                Some(&unfollowed_root),
            )
            .await;
            // Go writes the membership from a goroutine, so the unfollow can race it.
            await_membership(&plain.id, &unfollowed_root).await;
            unfollow(client, &plain.token, &plain.id, &team_id, &unfollowed_root).await;

            // In a private channel the plain user is not in, so the read gate refuses.
            let unreadable_root = post_message(client, token, &closed, "not yours", None).await;
            post_message(client, token, &closed, "reply", Some(&unreadable_root)).await;

            // **Last, and that ordering is the whole point.** Posting into a channel marks its
            // threads viewed for the poster: with collapsed threads on, Go moves `LastViewed` to
            // *now* for every thread membership in that channel. A mark set before the plain
            // user's reply to `unfollowed_root` was overwritten by it, silently putting
            // `unread_replies` back to zero and taking two of the subquery's three predicates
            // out of reach. Nothing writes to this channel as this user after this point.
            await_membership(&plain.id, &followed_root).await;
            let mark = created_at(&seen).await;
            mark_thread_read(
                client,
                &plain.token,
                &plain.id,
                &team_id,
                &followed_root,
                mark,
            )
            .await;
            assert_read_mark_held(&plain.id, &followed_root, mark, &unseen).await;

            Fixture {
                team_id,
                other_team_id,
                plain_id: plain.id,
                plain_token: plain.token,
                followed_root,
                unfollowed_root,
                unreadable_root,
                bystander_token: bystander.token,
                bystander_id: bystander.id,
            }
        })
        .await
}

/// `PUT /users/{id}/teams/{id}/threads/{id}/read/{timestamp}` — the API's own way to move
/// `LastViewed` off zero.
///
/// The timestamp is explicit rather than `now()`: the mark has to land *between* two replies,
/// and a wall-clock mark lands after every one of them, which quietly reduces the unread-replies
/// subquery to zero and takes two of its three predicates out of the test's reach.
async fn mark_thread_read(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    team_id: &str,
    thread_id: &str,
    timestamp: i64,
) {
    let response = client
        .put(format!(
            "{GO}/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "marking {thread_id} read failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// One short-lived connection, capped so a missing database fails fast rather than sitting on
/// sqlx's 30-second default.
async fn test_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

/// A post's `CreateAt`, which `POST /posts` returns but `post_message` throws away.
async fn created_at(post_id: &str) -> i64 {
    let pool = test_pool()
        .await
        .expect("DATABASE_URL is set for this suite");
    let row: (i64,) = sqlx::query_as("SELECT createat FROM posts WHERE id = $1")
        .bind(post_id)
        .fetch_one(&pool)
        .await
        .expect("the post is there");
    row.0
}

/// Fail loudly if the read mark did not survive, rather than silently disabling the mutations
/// that depend on it.
///
/// A wall-clock mark set here was found later holding a *different*, larger value — so the row
/// is read back and the ordering the fixture claims is asserted outright.
async fn assert_read_mark_held(user_id: &str, root_id: &str, mark: i64, unseen: &str) {
    let pool = test_pool()
        .await
        .expect("DATABASE_URL is set for this suite");
    let row: (i64, i64) = sqlx::query_as(
        "SELECT lastviewed, (SELECT createat FROM posts WHERE id = $3) \
           FROM threadmemberships WHERE userid = $1 AND postid = $2",
    )
    .bind(user_id)
    .bind(root_id)
    .bind(unseen)
    .fetch_one(&pool)
    .await
    .expect("the membership is there");
    assert_eq!(
        row.0, mark,
        "something moved LastViewed after the read mark"
    );
    assert!(
        row.1 > mark,
        "the second reply must be newer than the mark, or nothing is unread"
    );
}

/// Write a post's `Props` directly.
///
/// `POST /posts` runs `SanitizeInput`, which drops exactly the props `sanitizeThreadResponse`
/// exists to strip — so the only way to put one in front of this route is to write the row.
/// Confirmed to be visible through Go afterwards: the post cache does not hide it.
async fn plant_post_props(post_id: &str, props: &str) {
    let Some(pool) = test_pool().await else {
        return;
    };
    let _ = sqlx::query("UPDATE posts SET props = $2::jsonb WHERE id = $1")
        .bind(post_id)
        .bind(props)
        .execute(&pool)
        .await;
}

/// Plant a `PostsPriority` row — `POST /posts` accepts a priority only at creation time, and the
/// root here is created before the fixture knows it wants one.
async fn plant_post_priority(root_id: &str, priority: &str) {
    let Some(pool) = test_pool().await else {
        return;
    };
    let _ = sqlx::query(
        "INSERT INTO postspriority (postid, channelid, priority, requestedack, persistentnotifications)          SELECT $1, channelid, $2, FALSE, FALSE FROM posts WHERE id = $1          ON CONFLICT (postid) DO UPDATE SET priority = $2",
    )
    .bind(root_id)
    .bind(priority)
    .execute(&pool)
    .await;
}

/// `DELETE /users/{id}/teams/{id}/threads/{id}/following` — the API's own way to set
/// `Following = false`, which keeps the membership row.
async fn unfollow(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    team_id: &str,
    thread_id: &str,
) {
    let response = client
        .delete(format!(
            "{GO}/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "unfollowing {thread_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Wait for the membership Go writes from a goroutine after a reply is accepted.
///
/// **No unread state is planted in this suite.** An `UPDATE` to `ThreadMemberships` does stick
/// in the table — read back and confirmed — and Go still answers `unread_mentions: 0` from it,
/// so the server reads that row through a cache this suite cannot reach. `threads_for_user`'s
/// planted unread works because its values arrive through a different query path. Here they come
/// back through the membership object itself, so they are covered by the byte-for-byte
/// comparison instead: both servers read the same row through the same cache.
async fn await_membership(user_id: &str, root_id: &str) {
    let Some(pool) = test_pool().await else {
        return;
    };
    for attempt in 1..=40_u64 {
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM threadmemberships WHERE userid = $1 AND postid = $2",
        )
        .bind(user_id)
        .bind(root_id)
        .fetch_one(&pool)
        .await
        .expect("the count runs");
        if row.0 == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25 * attempt)).await;
    }
    panic!("the thread membership for {root_id} never appeared");
}

fn path(user_id: &str, team_id: &str, thread_id: &str, query: &str) -> String {
    if query.is_empty() {
        format!("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}")
    } else {
        format!("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}?{query}")
    }
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The thread, byte for byte, with the counters the membership contributes.
#[tokio::test]
async fn one_thread_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, &f.team_id, &f.followed_root, "");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline json.Marshal does not"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["id"], f.followed_root.as_str());
    // The fixture pins all four: two live replies and a deleted one, with the read mark on the
    // first, so `reply_count` counts live replies only and `unread_replies` is neither zero nor
    // the whole count. Asserted against the fixture rather than left to the byte comparison,
    // which agrees whenever both servers are wrong the same way.
    assert_eq!(parsed["reply_count"], 2, "the deleted reply is not counted");
    assert_eq!(
        parsed["unread_replies"], 1,
        "one reply is newer than the mark and not deleted: {parsed}"
    );
    assert_eq!(parsed["unread_mentions"], 0);
    // `sanitizeThreadResponse` removed all three of `add_channel_member`, `force_notification`
    // and `silent_notification`; `zz_keep` proves the sanitiser is selective rather than
    // blanking the map.
    assert_eq!(
        parsed["post"]["props"],
        serde_json::json!({"zz_keep": "kept"}),
        "the post's props are sanitised: {parsed}"
    );
    assert!(
        parsed["last_viewed_at"].as_i64().unwrap_or(0) > 0,
        "the read mark is on the wire as `last_viewed_at`: {parsed}"
    );
}

/// Participants are id-only stubs unless `?extended=true` — the same rule as the list route.
#[tokio::test]
async fn extended_fills_in_the_participants() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let bare = path(&f.plain_id, &f.team_id, &f.followed_root, "");
    let (go_bare, rs_bare) = fetch_both_stable(&client, &f.plain_token, &bare).await;
    assert_eq!(go_bare, rs_bare, "{bare} must be byte-identical");
    let parsed: serde_json::Value = serde_json::from_slice(&go_bare).expect("JSON");
    assert_eq!(parsed["participants"][0]["username"], "");

    let extended = path(&f.plain_id, &f.team_id, &f.followed_root, "extended=true");
    let (go_ext, rs_ext) = fetch_both_stable(&client, &f.plain_token, &extended).await;
    assert_eq!(go_ext, rs_ext, "{extended} must be byte-identical");
    let parsed: serde_json::Value = serde_json::from_slice(&go_ext).expect("JSON");
    assert!(
        !parsed["participants"][0]["username"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "extended resolves the profile: {}",
        parsed["participants"][0]
    );
}

/// `RequireTeamId` validates the segment and nothing afterwards reads it.
#[tokio::test]
async fn the_team_id_is_validated_and_then_never_used() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let right = path(&f.plain_id, &f.team_id, &f.followed_root, "");
    let (go_right, _rs) = fetch_both_stable(&client, &f.plain_token, &right).await;

    // A team the thread has nothing to do with, and one that does not exist at all.
    for team in [f.other_team_id.as_str(), "aaaaaaaaaaaaaaaaaaaaaaaaaa"] {
        let p = path(&f.plain_id, team, &f.followed_root, "");
        let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
        assert_eq!(go, rs, "{p} must be byte-identical");
        assert_eq!(
            go, go_right,
            "{p} answers exactly what the thread's own team's path answers"
        );
    }

    // But a malformed team id is still a 400, so the validator is doing something.
    let p = path(&f.plain_id, "short", &f.followed_root, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}

/// Two 404s with **different error ids**, and which one you get turns on whether a row exists.
///
/// Unfollowing keeps the `ThreadMemberships` row with `Following = false`, so the lookup
/// succeeds and the store refuses: `app.user.get_threads_for_user.not_found`. A user who never
/// replied has no row, so the lookup refuses first:
/// `app.user.get_thread_membership_for_user.not_found`. The handler's doc comment claimed the
/// first of those was unreachable through this route; it is not, and this test is why.
#[tokio::test]
async fn an_unfollowed_thread_is_the_same_404_as_a_missing_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The row is still there and not following, or this is a test about a deleted row.
    if let Some(pool) = test_pool().await {
        let row: (bool,) = sqlx::query_as(
            "SELECT following FROM threadmemberships WHERE userid = $1 AND postid = $2",
        )
        .bind(&f.plain_id)
        .bind(&f.unfollowed_root)
        .fetch_one(&pool)
        .await
        .expect("the membership row survives the unfollow");
        assert!(!row.0, "and it is not following");
    }

    let unfollowed = path(&f.plain_id, &f.team_id, &f.unfollowed_root, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &unfollowed).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status, "{unfollowed}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &unfollowed);
    assert_eq!(
        go["id"], "app.user.get_threads_for_user.not_found",
        "the row is there and not following, so the store refuses, not the lookup"
    );

    // The bystander has no row at all, so the *lookup* refuses one layer higher — a different
    // error id for the same 404, on a thread the unfollower gets the other id for.
    let never = path(&f.bystander_id, &f.team_id, &f.unfollowed_root, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.bystander_token, &never).await;
    assert_eq!(go_status, 404, "never followed, never replied");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &never);
    assert_eq!(
        go["id"],
        "app.user.get_thread_membership_for_user.not_found"
    );
}

/// The read gate, and the asymmetry an unknown id creates.
#[tokio::test]
async fn the_read_gate_refuses_and_an_unknown_id_splits_by_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // A thread in a channel the plain user cannot read.
    let p = path(&f.plain_id, &f.team_id, &f.unreadable_root, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "SessionHasPermissionToReadPost refuses");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // An id that names nothing: the same check cannot resolve a channel and falls back to a
    // system-level one, so a plain user is refused and an admin reaches the 404.
    let unknown = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    let p = path(&f.plain_id, &f.team_id, unknown, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "the fallback refuses an ordinary user");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);

    let admin_id = logged_in_user_id();
    let p = path(admin_id, &f.team_id, unknown, "");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(
        go_status, 404,
        "and admits an admin as far as the membership"
    );
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(
        go["id"],
        "app.user.get_thread_membership_for_user.not_found"
    );
}

/// Somebody else's thread is the user gate, not the read gate.
#[tokio::test]
async fn another_users_thread_is_a_403() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    let p = path(admin_id, &f.team_id, &f.followed_root, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// `RequireThreadId`, and the user segment beside it.
#[tokio::test]
async fn a_segment_of_the_wrong_length_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for p in [
        path("short", &f.team_id, &f.followed_root, ""),
        path(&f.plain_id, &f.team_id, "short", ""),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// Everything but `GET` on this exact path stays Go's — `/following` and `/read` are one segment
/// deeper and are nobody's here.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // **A thread id nothing else in this suite asserts on.** Only the `x-mmrs-served-by` header
    // is read here, so the id need not exist — and it must not be a real one, because the
    // `/following` request below is a *write*: Go's follow route sets `LastViewed` to now, which
    // silently moved the read mark this suite's fixture depends on and cost two mutations their
    // verdict. Forwarding tests share their fixture with everyone; they should touch nothing.
    let p = path(&f.plain_id, &f.team_id, "zzzzzzzzzzzzzzzzzzzzzzzzzz", "");
    for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }

    // The sibling that hangs off this path is **no longer** Go's: `PUT`/`DELETE …/following` are
    // served here (`mm_api::thread_writes`), and `tests/parity/thread_writes.rs` is their suite.
    // Asserted rather than deleted, because this test is what would otherwise notice a
    // regression in the router's static-versus-parameter preference on this path.
    //
    // The **team** segment is deliberately malformed, so the handler answers 400 before it
    // writes anything: this test shares its fixture with the rest of the suite, and a follow
    // would move the read mark every other test here depends on.
    let following = format!(
        "/api/v4/users/{}/teams/short/threads/zzzzzzzzzzzzzzzzzzzzzzzzzz/following",
        f.plain_id
    );
    let rs = client
        .put(format!("{RUST}{following}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{following} is served here now"
    );
}

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = path(
        "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccc",
        "",
    );

    let go = client
        .get(format!("{GO}{p}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{p}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{p}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
}
