//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/{team_id}/threads` — the Threads
//! view.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity threads_for_user
//! ```
//!
//! # What this port serves, and what it hands upstream
//!
//! The default request plus `?extended`. `since`, `before`, `after`, `unread`, `deleted`,
//! `totalsOnly`, `threadsOnly` and `excludeDirect` each rewrite the store query, and each is
//! **forwarded to Go** rather than guessed at — [`the_unsupported_parameters_are_forwarded`]
//! pins that list, so adding one to the handler without a fixture makes this test fail.
//!
//! # The two things a reader would not predict
//!
//! **Participants are id-only stubs** unless `?extended=true` — `User` structs with every other
//! field at its zero value, which the wire shows as `"username": ""` rather than omitting.
//! **And the embedded post carries no computed fields**: `reply_count` is `0` and
//! `participants` is `null` on it, however many replies the thread has, because this query has
//! no reply-count subquery. The thread's own `reply_count` beside it is the real number.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, fetch_both_raw, fetch_both_stable,
    go_minted_token, logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    /// A second team the plain user is **not** in, for the `view_team` refusal.
    foreign_team_id: String,
    plain_id: String,
    plain_token: String,
    /// Two threads the plain user follows, oldest reply first.
    older_root: String,
    newer_root: String,
    /// A thread in a channel the plain user has been removed from — the `EXISTS` over
    /// `ChannelMembers` is the only thing keeping it out of their list.
    left_channel_root: String,

    /// A second actor, so the assertions above keep their exact counts while these threads
    /// exercise the branches the first fixture could not reach.
    extra_id: String,
    extra_token: String,
    /// Followed, unread, and its root carries `priority = urgent`.
    urgent_root: String,
    /// The same, with `priority = important` — so `is_urgent` must be **false**.
    important_root: String,
    /// A thread whose membership has `following = false`.
    unfollowed_root: String,
    /// A thread whose `LastViewed` is exactly its `LastReplyAt`: read, by one millisecond.
    exactly_read_root: String,
    /// A root post carrying an attachment action integration.
    ///
    /// **Its own actor and channel.** Forwarding is all-or-nothing per page, so a single
    /// attachments thread in a shared page would forward every other assertion's request too —
    /// which it did, until this actor was split out.
    sanitised_root: String,
    attach_id: String,
    attach_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

/// Serialises the tests that assert an **exact** thread count for `plain_id` against the one test
/// that temporarily changes what that count is.
///
/// `a_thread_in_a_channel_the_caller_left_is_excluded` re-inserts the channel membership, checks
/// that the thread reappears, and deletes it again. Inside that window the fixture user follows
/// **three** threads, not two — so a concurrent `total == 2` sees three and fails on a number that
/// is correct for the instant it was read. It is the intra-suite form of the rule the four
/// cross-suite fixes established: an assertion over a whole list cannot survive a test that
/// writes to it. This is the narrowest fix — a lock rather than a third fixture user — because
/// the window is three statements wide.
static PLAIN_THREAD_COUNT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let admin_id = logged_in_user_id();
            let team_id = create_team(client, token, "thrteam").await;
            let foreign_team_id = create_team(client, token, "thrforeign").await;

            let channel = create_channel_typed(client, token, &team_id, "thrmain", "O").await;
            let left = create_channel_typed(client, token, &team_id, "thrleft", "O").await;

            let plain = create_plain_user(client, token, &team_id, "threads").await;
            add_user_to_channel(client, token, &channel, &plain.id).await;
            add_user_to_channel(client, token, &left, &plain.id).await;

            // A thread exists only once a root post has a reply, and a user follows a thread by
            // replying to it — so each fixture thread is a root from the admin plus a reply from
            // the plain user.
            let older_root = post_message(client, token, &channel, "older root", None).await;
            post_message(
                client,
                &plain.token,
                &channel,
                "older reply",
                Some(&older_root),
            )
            .await;

            wait_past_a_millisecond().await;

            let newer_root = post_message(client, token, &channel, "newer root", None).await;
            post_message(
                client,
                &plain.token,
                &channel,
                "newer reply",
                Some(&newer_root),
            )
            .await;

            // The same shape in the channel the plain user is about to leave.
            let left_channel_root = post_message(client, token, &left, "left root", None).await;
            post_message(
                client,
                &plain.token,
                &left,
                "left reply",
                Some(&left_channel_root),
            )
            .await;
            remove_from_channel(client, token, &left, &plain.id).await;
            // Leaving a channel **deletes** the thread memberships in it, so the row is gone and
            // the `EXISTS` over `ChannelMembers` never gets a chance to exclude anything. Planted
            // back directly — the REST API cannot leave a user following a thread in a channel
            // they are not in, and that is exactly the state the predicate exists for.
            plant_thread_membership(&plain.id, &left_channel_root).await;

            // Unread state: the membership rows are freshly viewed after replying, so nothing is
            // unread and the two unread counters would both be zero. Planted directly, which the
            // REST API cannot do without another user posting.
            plant_unread(&plain.id, &newer_root, 2).await;

            // ---- the second actor and the branches the first could not reach ----
            let extra_channel =
                create_channel_typed(client, token, &team_id, "thrextra", "O").await;
            let extra = create_plain_user(client, token, &team_id, "thrx").await;
            add_user_to_channel(client, token, &extra_channel, &extra.id).await;

            let mut roots = Vec::new();
            for tag in ["urgent", "important", "unfollowed", "exact"] {
                let root = post_message(client, token, &extra_channel, tag, None).await;
                post_message(client, &extra.token, &extra_channel, "reply", Some(&root)).await;
                roots.push(root);
            }
            let (urgent_root, important_root, unfollowed_root, exactly_read_root) = (
                roots[0].clone(),
                roots[1].clone(),
                roots[2].clone(),
                roots[3].clone(),
            );

            // The attachments thread lives alone, for the reason on `sanitised_root`.
            let attach_channel =
                create_channel_typed(client, token, &team_id, "thrattach", "O").await;
            let attach = create_plain_user(client, token, &team_id, "thratt").await;
            add_user_to_channel(client, token, &attach_channel, &attach.id).await;
            let sanitised_root = post_with_sanitisable_props(client, token, &attach_channel).await;
            post_message(
                client,
                &attach.token,
                &attach_channel,
                "reply",
                Some(&sanitised_root),
            )
            .await;

            // A priority row is what `PostsPriority` joins on, and nothing in the fixture had
            // one — so both the per-thread `is_urgent` CASE and the urgent-mentions counter were
            // dead code. `important` is the control: same join, different value, `is_urgent`
            // false.
            plant_post_priority(&urgent_root, "urgent").await;
            plant_post_priority(&important_root, "important").await;
            plant_unread(&extra.id, &urgent_root, 3).await;
            plant_unread(&extra.id, &important_root, 5).await;

            // `Following = false` — the REST API only writes it through the follow endpoint, and
            // without a row like this the flag is never the thing excluding a thread.
            plant_following(&extra.id, &unfollowed_root, false).await;

            // `LastViewed` exactly equal to `LastReplyAt`. The unread-threads counter uses a
            // strict `<`, so this thread is read; widening it to `<=` counts it, which nothing
            // else in the fixture could tell apart.
            plant_last_viewed_equal_to_last_reply(&extra.id, &exactly_read_root).await;

            // `SanitizeProps` strips `force_notification`, and `createPost` strips it too — so
            // no sequence of API calls leaves one in the table for the *read* path to remove.
            // Planted directly onto a thread root with no attachments, so the page is still
            // served rather than forwarded.
            plant_props(&important_root, r#"{"force_notification": true}"#).await;

            // A deleted reply, so the unread subquery's own `DeleteAt = 0` has something to
            // exclude.
            let doomed_reply =
                post_message(client, token, &extra_channel, "doomed", Some(&urgent_root)).await;
            delete_post(client, token, &doomed_reply).await;

            let _ = admin_id;
            Fixture {
                team_id,
                foreign_team_id,
                plain_id: plain.id,
                plain_token: plain.token,
                older_root,
                newer_root,
                left_channel_root,
                extra_id: extra.id,
                extra_token: extra.token,
                urgent_root,
                important_root,
                unfollowed_root,
                exactly_read_root,
                sanitised_root,
                attach_id: attach.id,
                attach_token: attach.token,
            }
        })
        .await
}

/// `LastReplyAt` is milliseconds; two threads created inside one tick tie and the `ORDER BY`
/// cannot be told from an unordered scan.
async fn wait_past_a_millisecond() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

async fn remove_from_channel(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    user_id: &str,
) {
    let response = client
        .delete(format!(
            "{GO}/api/v4/channels/{channel_id}/members/{user_id}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "removing {user_id} from {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// A root post whose props both `SanitizeProps` and `StripActionIntegrations` have work to do on.
async fn post_with_sanitisable_props(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": "sanitise me",
            "props": {
                "add_channel_member": {"post_id": "x"},
                "attachments": [{
                    "text": "pick",
                    "actions": [{
                        "id": "act1",
                        "name": "Click",
                        "integration": {"url": "http://example.invalid/hook", "context": {}},
                    }],
                }],
            },
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "posting the sanitisation fixture failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the post decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn delete_post(client: &reqwest::Client, token: &str, post_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "deleting {post_id} failed");
}

/// Plant a `PostsPriority` row. `POST /posts` accepts a priority only on a root post through
/// `metadata.priority`, and the fixture needs both values on demand.
async fn plant_post_priority(root_id: &str, priority: &str) {
    let Some(pool) = test_pool().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO postspriority (postid, channelid, priority, requestedack, persistentnotifications) \
         SELECT p.id, p.channelid, $2, false, false FROM posts p WHERE p.id = $1 \
         ON CONFLICT (postid) DO UPDATE SET priority = $2",
    )
    .bind(root_id)
    .bind(priority)
    .execute(&pool)
    .await
    .expect("the insert runs");
}

/// Write a post's `props` column directly.
async fn plant_props(post_id: &str, props: &str) {
    let Some(pool) = test_pool().await else {
        return;
    };
    sqlx::query("UPDATE posts SET props = $2::jsonb WHERE id = $1")
        .bind(post_id)
        .bind(props)
        .execute(&pool)
        .await
        .expect("the update runs");
}

/// Set a thread membership's `Following` flag.
async fn plant_following(user_id: &str, root_id: &str, following: bool) {
    let Some(pool) = test_pool().await else {
        return;
    };
    sqlx::query("UPDATE threadmemberships SET following = $3 WHERE userid = $1 AND postid = $2")
        .bind(user_id)
        .bind(root_id)
        .bind(following)
        .execute(&pool)
        .await
        .expect("the update runs");
}

/// Make `LastViewed` exactly `LastReplyAt`, which no sequence of API calls reliably produces.
async fn plant_last_viewed_equal_to_last_reply(user_id: &str, root_id: &str) {
    let Some(pool) = test_pool().await else {
        return;
    };
    sqlx::query(
        "UPDATE threadmemberships tm SET lastviewed = t.lastreplyat \
           FROM threads t WHERE t.postid = tm.postid AND tm.userid = $1 AND tm.postid = $2",
    )
    .bind(user_id)
    .bind(root_id)
    .execute(&pool)
    .await
    .expect("the update runs");
}

/// One place for the direct-database escapes this suite needs; silent without a `DATABASE_URL`,
/// and every test that depends on one re-checks the row it planted.
async fn test_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

/// Re-create a `ThreadMemberships` row the API has deleted.
async fn plant_thread_membership(user_id: &str, root_id: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    sqlx::query(
        "INSERT INTO threadmemberships \
             (postid, userid, following, lastviewed, lastupdated, unreadmentions) \
         VALUES ($2, $1, TRUE, 0, 0, 0) \
         ON CONFLICT (postid, userid) DO UPDATE SET following = TRUE",
    )
    .bind(user_id)
    .bind(root_id)
    .execute(&pool)
    .await
    .expect("the insert runs");
}

/// Rewind a thread membership's `LastViewed` and give it mentions, so the unread counters and
/// the per-thread `unread_replies` subquery have something to count.
async fn plant_unread(user_id: &str, root_id: &str, mentions: i64) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    sqlx::query(
        "UPDATE threadmemberships SET lastviewed = 0, unreadmentions = $3 \
         WHERE userid = $1 AND postid = $2",
    )
    .bind(user_id)
    .bind(root_id)
    .bind(mentions)
    .execute(&pool)
    .await
    .expect("the update runs");
}

fn path(user_id: &str, team_id: &str, query: &str) -> String {
    if query.is_empty() {
        format!("/api/v4/users/{user_id}/teams/{team_id}/threads")
    } else {
        format!("/api/v4/users/{user_id}/teams/{team_id}/threads?{query}")
    }
}

fn thread_ids(raw: &[u8]) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(raw).expect("the body is JSON");
    parsed["threads"]
        .as_array()
        .map(|threads| {
            threads
                .iter()
                .map(|t| t["id"].as_str().expect("an id").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route in one assertion: both threads, newest reply first, with their counters.
#[tokio::test]
async fn the_thread_list_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let _count_guard = PLAIN_THREAD_COUNT.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, &f.team_id, "per_page=30");
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

    let ids = thread_ids(&go);
    assert_eq!(
        ids,
        vec![f.newer_root.clone(), f.older_root.clone()],
        "ORDER BY Threads.LastReplyAt DESC"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed["total"], 2,
        "the counters are their own queries, not the page length"
    );
    assert_eq!(parsed["total_unread_threads"], 1, "only the planted one");
    assert_eq!(parsed["total_unread_mentions"], 2);

    let newer = &parsed["threads"][0];
    assert_eq!(
        newer["unread_mentions"], 2,
        "the caller's own membership row"
    );
    assert_eq!(
        newer["unread_replies"], 1,
        "the correlated subquery counts replies created after LastViewed"
    );
    assert_eq!(parsed["threads"][1]["unread_replies"], 0);
}

/// The embedded post carries **no computed fields** — this query has no reply-count subquery.
#[tokio::test]
async fn the_embedded_post_has_no_reply_count_of_its_own() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, &f.team_id, "per_page=30");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let thread = &parsed["threads"][0];
    assert_eq!(thread["reply_count"], 1, "the thread knows its replies");
    assert_eq!(
        thread["post"]["reply_count"], 0,
        "and the post inside it does not"
    );
    assert!(
        thread["post"]["participants"].is_null(),
        "nor its participants: {}",
        thread["post"]
    );
    assert!(
        thread["post"].get("metadata").is_none(),
        "and no metadata — this route never calls PreparePostForClient"
    );
}

/// Participants are id-only stubs by default and full profiles with `?extended=true`.
#[tokio::test]
async fn extended_is_the_only_thing_that_fills_in_participants() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let bare = path(&f.plain_id, &f.team_id, "per_page=30");
    let (go_bare, rs_bare) = fetch_both_stable(&client, &f.plain_token, &bare).await;
    assert_eq!(go_bare, rs_bare, "{bare} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go_bare).expect("JSON");
    let participant = &parsed["threads"][0]["participants"][0];
    assert_eq!(
        participant["id"],
        f.plain_id.as_str(),
        "the replier is the participant"
    );
    assert_eq!(
        participant["username"], "",
        "and every other field is Go's zero value, present rather than omitted: {participant}"
    );

    let extended = path(&f.plain_id, &f.team_id, "per_page=30&extended=true");
    let (go_ext, rs_ext) = fetch_both_stable(&client, &f.plain_token, &extended).await;
    assert_eq!(go_ext, rs_ext, "{extended} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go_ext).expect("JSON");
    let participant = &parsed["threads"][0]["participants"][0];
    assert_eq!(participant["id"], f.plain_id.as_str());
    assert!(
        !participant["username"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "extended resolves the profile: {participant}"
    );
}

/// The `EXISTS` over `ChannelMembers` is the access check: a thread in a channel the caller has
/// left disappears from their list, membership row and all.
#[tokio::test]
async fn a_thread_in_a_channel_the_caller_left_is_excluded() {
    if !stack_enabled() {
        return;
    }
    let _count_guard = PLAIN_THREAD_COUNT.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The thread membership survives leaving the channel, so the exclusion is the `EXISTS` and
    // not a missing row.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let rows: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM threadmemberships WHERE userid = $1 AND postid = $2",
        )
        .bind(&f.plain_id)
        .bind(&f.left_channel_root)
        .fetch_one(&pool)
        .await
        .expect("the count runs");
        assert_eq!(
            rows.0, 1,
            "the thread membership outlives the channel membership"
        );
    }

    let p = path(&f.plain_id, &f.team_id, "per_page=30");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert!(
        !thread_ids(&go).contains(&f.left_channel_root),
        "a thread in a channel the caller left must not be listed: {:?}",
        thread_ids(&go)
    );

    // Put the channel membership back and the same thread reappears for the same caller — so
    // the exclusion is that predicate and nothing else about the fixture.
    //
    // (The admin's own list would have shown it too, but a team-wide page can pick up another
    // suite's attachments post and forward, which says nothing about this predicate.)
    if let Some(pool) = test_pool().await {
        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount, \
                 mentioncount, notifyprops, lastupdateat, schemeuser, schemeadmin, schemeguest, \
                 mentioncountroot, msgcountroot, urgentmentioncount) \
             SELECT p.channelid, $1, 'channel_user', 0, 0, 0, '{}', 0, true, false, false, 0, 0, 0 \
               FROM posts p WHERE p.id = $2 \
             ON CONFLICT (channelid, userid) DO NOTHING",
        )
        .bind(&f.plain_id)
        .bind(&f.left_channel_root)
        .execute(&pool)
        .await
        .expect("the insert runs");

        let (go_back, rs_back) = fetch_both_stable(&client, &f.plain_token, &p).await;
        assert_eq!(go_back, rs_back, "{p} must be byte-identical");
        assert!(
            thread_ids(&go_back).contains(&f.left_channel_root),
            "with the channel membership back, the thread is listed again"
        );

        // Leave it as the fixture built it, so the suite can be re-run.
        sqlx::query(
            "DELETE FROM channelmembers WHERE userid = $1 \
               AND channelid = (SELECT channelid FROM posts WHERE id = $2)",
        )
        .bind(&f.plain_id)
        .bind(&f.left_channel_root)
        .execute(&pool)
        .await
        .expect("the delete runs");
    }
}

/// `per_page` limits the page without touching the counters.
#[tokio::test]
async fn per_page_limits_the_list_and_not_the_totals() {
    if !stack_enabled() {
        return;
    }
    let _count_guard = PLAIN_THREAD_COUNT.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, &f.team_id, "per_page=1");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let page = thread_ids(&go);
    assert_eq!(page.len(), 1, "one thread");
    assert_eq!(parsed["total"], 2, "and the count of all of them");

    // **The page is the head of the unpaged list**, rather than a hard-coded id.
    //
    // Asserting `newer_root` directly failed once in a full-suite run — both servers agreed
    // byte-for-byte, so it was not a divergence, and it passes three times out of three in
    // isolation. Under load something else about the fixture's ordering moves, and pinning the id
    // turns that into a failure about the wrong thing. The property this route actually has is
    // that `per_page=1` returns the first element of the full list, which is what the ordering
    // test above pins; checking them against each other keeps both honest without either
    // depending on a timestamp race.
    let full = path(&f.plain_id, &f.team_id, "per_page=60");
    let (all, _) = fetch_both_stable(&client, &f.plain_token, &full).await;
    let all = thread_ids(&all);
    assert_eq!(
        all.len(),
        2,
        "the fixture's two threads and no more: {all:?}"
    );
    assert_eq!(page, all[..1].to_vec(), "page one is the head of the list");
    assert!(
        all.contains(&f.newer_root) && all.contains(&f.older_root),
        "and both are the fixture's own: {all:?}"
    );
}

/// Every parameter this port does not serve is handed to Go rather than guessed at.
#[tokio::test]
async fn the_unsupported_parameters_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for param in [
        "since=1",
        "before=abc",
        "after=abc",
        "unread=true",
        "deleted=true",
        "totalsOnly=true",
        "threadsOnly=true",
        "excludeDirect=true",
    ] {
        let p = path(&f.plain_id, &f.team_id, param);
        let rs = client
            .get(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {}", f.plain_token))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{p} must be forwarded, not served"
        );
    }

    // And the served shapes really are served, or the list above would pass vacuously.
    let served = path(&f.plain_id, &f.team_id, "per_page=5&extended=true");
    let rs = client
        .get(format!("{RUST}{served}"))
        .header("Authorization", format!("Bearer {}", f.plain_token))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{served} must be served here"
    );
}

/// Two gates, in Go's order: the user first, then the team.
#[tokio::test]
async fn both_permission_gates_answer_403() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    // Somebody else's threads.
    let p = path(admin_id, &f.team_id, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(
        go_status, 403,
        "a plain user cannot read the admin's threads"
    );
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // Their own threads, in a team they are not in.
    let p = path(&f.plain_id, &f.foreign_team_id, "");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "the team gate refuses a non-member");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
}

/// `RequireUserId().RequireTeamId()` — both segments, and the user is checked first.
#[tokio::test]
async fn a_segment_of_the_wrong_length_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for p in [
        path("short", &f.team_id, ""),
        path(&f.plain_id, "short", ""),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// Everything but `GET` on this path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, &f.team_id, "");
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
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
}

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = "/api/v4/users/aaaaaaaaaaaaaaaaaaaaaaaaaa/teams/bbbbbbbbbbbbbbbbbbbbbbbbbb/threads";

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
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, p);
}

/// The `PostsPriority` join and the urgency `CASE` — both dead code until a priority row exists.
///
/// `urgent` and `important` are the same join with different values, so this pins the comparison
/// rather than the join: a mutation flipping `= 'urgent'` to `<> 'urgent'` swaps the two.
#[tokio::test]
async fn only_an_urgent_priority_makes_a_thread_urgent() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Both priority rows must exist, or the test is about a join that never fires.
    if let Some(pool) = test_pool().await {
        let rows: (i64,) =
            sqlx::query_as("SELECT count(*) FROM postspriority WHERE postid = ANY($1)")
                .bind(vec![f.urgent_root.clone(), f.important_root.clone()])
                .fetch_one(&pool)
                .await
                .expect("the count runs");
        assert_eq!(rows.0, 2, "both fixture roots carry a priority row");
    }

    let p = path(&f.extra_id, &f.team_id, "per_page=30");
    let (go, rs) = fetch_both_stable(&client, &f.extra_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let by_id: std::collections::HashMap<&str, &serde_json::Value> = parsed["threads"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|t| (t["id"].as_str().expect("an id"), t))
        .collect();

    assert_eq!(
        by_id[f.urgent_root.as_str()]["is_urgent"],
        true,
        "priority = urgent"
    );
    assert_eq!(
        by_id[f.important_root.as_str()]["is_urgent"],
        false,
        "priority = important is not urgent"
    );
    assert_eq!(
        by_id[f.exactly_read_root.as_str()]["is_urgent"],
        false,
        "and no priority row at all is not urgent"
    );

    // The urgent-mentions counter sums only the urgent thread's mentions, not the important
    // one's — the same comparison in a different query.
    assert_eq!(
        parsed["total_unread_urgent_mentions"],
        3,
        "urgent contributes 3; important's 5 does not: {}",
        serde_json::to_string(&parsed["threads"]).unwrap_or_default()
    );
    assert_eq!(
        parsed["total_unread_mentions"], 8,
        "and the plain mention counter sums both"
    );
}

/// `Following = false` is what keeps an unfollowed thread out of the list — the membership row
/// is still there.
#[tokio::test]
async fn an_unfollowed_thread_is_excluded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    if let Some(pool) = test_pool().await {
        let row: (bool,) = sqlx::query_as(
            "SELECT following FROM threadmemberships WHERE userid = $1 AND postid = $2",
        )
        .bind(&f.extra_id)
        .bind(&f.unfollowed_root)
        .fetch_one(&pool)
        .await
        .expect("the row is there");
        assert!(!row.0, "the membership exists and is not following");
    }

    let p = path(&f.extra_id, &f.team_id, "per_page=30");
    let (go, rs) = fetch_both_stable(&client, &f.extra_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert!(
        !thread_ids(&go).contains(&f.unfollowed_root),
        "an unfollowed thread is not in the list: {:?}",
        thread_ids(&go)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed["total"], 3,
        "and the counters exclude it too — four threads, three followed"
    );
}

/// The unread-threads counter's `LastViewed < LastReplyAt` is **strict**. A thread viewed at
/// exactly its last reply is read, and nothing else in the fixture sits on that boundary.
#[tokio::test]
async fn a_thread_viewed_at_exactly_its_last_reply_is_read() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    if let Some(pool) = test_pool().await {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT tm.lastviewed, t.lastreplyat FROM threadmemberships tm \
               JOIN threads t ON t.postid = tm.postid \
              WHERE tm.userid = $1 AND tm.postid = $2",
        )
        .bind(&f.extra_id)
        .bind(&f.exactly_read_root)
        .fetch_one(&pool)
        .await
        .expect("the row is there");
        assert_eq!(
            row.0, row.1,
            "the boundary is exact, or this proves nothing"
        );
    }

    let p = path(&f.extra_id, &f.team_id, "per_page=30");
    let (go, rs) = fetch_both_stable(&client, &f.extra_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed["total_unread_threads"], 2,
        "the two planted-unread threads, and not the one sitting on the boundary"
    );
}

/// The unread-replies subquery's own `DeleteAt = 0`: a deleted reply is not unread.
#[tokio::test]
async fn a_deleted_reply_is_not_an_unread_reply() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The deleted reply is still a row, so this is an exclusion rather than an absence.
    if let Some(pool) = test_pool().await {
        let rows: (i64,) =
            sqlx::query_as("SELECT count(*) FROM posts WHERE rootid = $1 AND deleteat > 0")
                .bind(&f.urgent_root)
                .fetch_one(&pool)
                .await
                .expect("the count runs");
        assert_eq!(rows.0, 1, "one soft-deleted reply survives in the table");
    }

    let p = path(&f.extra_id, &f.team_id, "per_page=30");
    let (go, rs) = fetch_both_stable(&client, &f.extra_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let urgent = parsed["threads"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|t| t["id"] == f.urgent_root.as_str())
        .expect("the urgent thread is listed");
    assert_eq!(
        urgent["unread_replies"], 1,
        "the live reply counts and the deleted one does not: {urgent}"
    );
}

/// A thread whose root post carries `attachments` is **forwarded**, page and all.
///
/// `StripActionIntegrations` re-marshals that prop from the decoded `SlackAttachment` slice, and
/// Go emits a struct's fields in declaration order where `serde_json::Value` sorts them. The two
/// bodies then differ by key order inside `props.attachments` and by nothing else — which is a
/// wrong body, so this route refuses to be the one that serves it. See `docs/TECH_DEBT.md`.
#[tokio::test]
async fn a_thread_whose_root_has_attachments_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The props really are stored, or this test is about a post that never had them.
    if let Some(pool) = test_pool().await {
        let stored: (String,) = sqlx::query_as("SELECT props::text FROM posts WHERE id = $1")
            .bind(&f.sanitised_root)
            .fetch_one(&pool)
            .await
            .expect("the row is there");
        // `add_channel_member` does not survive `createPost` — Go sanitises on the way in as
        // well as out — so the integration block is the half a fixture can actually plant.
        assert!(
            stored.0.contains("integration"),
            "the stored props carry the integration block: {}",
            stored.0
        );
    }

    let p = path(&f.attach_id, &f.team_id, "per_page=30");
    let rs = client
        .get(format!("{RUST}{p}"))
        .header("Authorization", format!("Bearer {}", f.attach_token))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "the page holds an attachments post, so the whole page is forwarded"
    );
    assert_eq!(rs.status(), 200);

    // Go's own answer still applies both halves of the sanitiser, which is what we are deferring
    // to rather than reproducing.
    let text = rs.text().await.expect("body");
    assert!(
        !text.contains("\"integration\""),
        "Go strips the integration block: {text}"
    );
    assert!(text.contains("\"act1\""), "and keeps the action itself");
}

/// `sanitizeThreadResponse`'s post half — the reachable one.
///
/// `StripActionIntegrations` needs an `attachments` prop, and a page carrying one is forwarded
/// (see the test above), so the only part of this that a served response can exercise is
/// `SanitizeProps` removing `force_notification`. `createPost` strips that prop on the way in as
/// well, so the fixture plants it directly — without which the whole sanitiser is dead code and
/// a mutation deleting both calls survives, as one did.
#[tokio::test]
async fn a_planted_force_notification_prop_is_sanitised_away() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The prop must be in the table, or this asserts nothing about the sanitiser.
    if let Some(pool) = test_pool().await {
        let stored: (String,) = sqlx::query_as("SELECT props::text FROM posts WHERE id = $1")
            .bind(&f.important_root)
            .fetch_one(&pool)
            .await
            .expect("the row is there");
        assert!(
            stored.0.contains("force_notification"),
            "the planted prop is stored: {}",
            stored.0
        );
    }

    let p = path(&f.extra_id, &f.team_id, "per_page=30");
    let (go, rs) = fetch_both_stable(&client, &f.extra_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let text = String::from_utf8_lossy(&go);
    assert!(
        !text.contains("force_notification"),
        "SanitizeProps removes it on the way out: {text}"
    );
}
