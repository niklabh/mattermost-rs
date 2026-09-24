//! Cross-server parity for `POST /api/v4/users/{user_id}/posts/{post_id}/set_unread` in an **open
//! channel** and on the **CRT-unsupported reply** arm — the two shapes [D-421] forwarded until the
//! mention engine existed.
//!
//! One fixture, both servers in turn: Go marks the channel unread, the reader's rows are read
//! back, the thread membership is put back as it was, then this server does the same and its rows
//! must be Go's. `UpdateLastViewedAtPost` computes every column from post rows, never from the
//! member row it overwrites, so the channel member needs no reset; the thread membership does,
//! because the reply arm creates or rewrites it.
//!
//! # What the channel holds, and why each post is there
//!
//! The reader has mention key `zebra`, `comments = any`, and channel mentions on. After the marked
//! post `A` come, in order:
//!
//! | post | why it is (or is not) a mention |
//! |---|---|
//! | the reader's `system_add_to_channel` | `addedUserId` is the reader |
//! | `@reader` root, **urgent** (planted `PostsPriority` row) | explicit mention; the urgent count |
//! | `zebra` root | the mention key |
//! | `@channel` root | channel mentions allowed |
//! | reader's root `R`, then an admin reply to it | a comment mention: the reader started it |
//! | admin root `T`, reader reply, admin `zebra` reply `T2`, admin reply `T3` | `T3` is a comment mention under `any` (an earlier reply); `T2` is the thread's one explicit mention |
//! | admin root `V`, admin reply `V1` | nothing — and a thread the reader has no membership in |
//! | the reader's own `zebra` root | nothing — a user never mentions themselves |
//! | admin root `plain` | nothing |
//!
//! So `mention_count` and `mention_count_root` differ, the urgent count is non-zero and below the
//! root count, and a port that dropped any one rule answers a different number.

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, add_user_to_channel, client,
    create_channel, create_plain_user, delete_channel, delete_plain_user, fixture_pool,
    go_minted_token, post_message, stack_enabled,
};
use std::time::Duration;

/// A `ThreadMemberships` row: `(following, lastviewed, unreadmentions, lastupdated)`.
type ThreadRow = (Option<bool>, Option<i64>, Option<i64>, Option<i64>);

/// Six `ChannelMembers` counters.
type MemberRow = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

struct Fixture {
    channel: String,
    reader: common::PlainUser,
    /// `A`, the root every root-arm mark uses.
    mark_root: String,
    /// The urgent `@reader` root, marked once so the subject post's own priority is read.
    urgent_root: String,
    /// `T2`: a reply in a thread the reader already follows.
    followed_reply: String,
    /// `V1`: a reply in a thread the reader has no membership in.
    unfollowed_reply: String,
}

async fn fixture(client: &reqwest::Client, admin: &str, pool: &sqlx::PgPool, tag: &str) -> Fixture {
    let (team, _) = a_team_and_channel_the_user_is_in(client, admin).await;
    let reader = create_plain_user(client, admin, &team, tag).await;
    let response = client
        .put(format!("{GO}/api/v4/users/{}/patch", reader.id))
        .header("Authorization", format!("Bearer {}", reader.token))
        .json(&serde_json::json!({
            "notify_props": {
                "mention_keys": "zebra", "comments": "any", "channel": "true",
                "first_name": "false", "desktop": "mention", "email": "true",
                "push": "mention", "desktop_sound": "true", "push_status": "away",
            }
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "the reader's notify props are set");
    let username = common::plain_username(tag);

    let channel = create_channel(client, admin, &team, tag).await;
    let mark_root = post_message(client, admin, &channel, "sum mark here", None).await;
    add_user_to_channel(client, admin, &channel, &reader.id).await;
    let urgent_root =
        post_message(client, admin, &channel, &format!("hello @{username}"), None).await;
    sqlx::query(
        "INSERT INTO postspriority (postid, channelid, priority, requestedack, \
         persistentnotifications) VALUES ($1, $2, 'urgent', false, false)",
    )
    .bind(&urgent_root)
    .bind(&channel)
    .execute(pool)
    .await
    .expect("the urgent priority is planted");
    post_message(client, admin, &channel, "zebra crossing", None).await;
    post_message(client, admin, &channel, "@channel heads up", None).await;
    // The reader's own post, naming their own keyword: never a mention.
    post_message(client, &reader.token, &channel, "zebra is mine", None).await;
    let started = post_message(client, &reader.token, &channel, "my own thread", None).await;
    post_message(client, admin, &channel, "a reply to you", Some(&started)).await;
    let joined = post_message(client, admin, &channel, "a thread to join", None).await;
    post_message(client, &reader.token, &channel, "me too", Some(&joined)).await;
    let followed_reply = post_message(
        client,
        admin,
        &channel,
        "a later zebra reply",
        Some(&joined),
    )
    .await;
    post_message(client, admin, &channel, "and another", Some(&joined)).await;
    let untouched = post_message(client, admin, &channel, "nobody's thread", None).await;
    let unfollowed_reply =
        post_message(client, admin, &channel, "still nobody", Some(&untouched)).await;
    post_message(client, admin, &channel, "plain", None).await;

    Fixture {
        channel,
        reader,
        mark_root,
        urgent_root,
        followed_reply,
        unfollowed_reply,
    }
}

/// The reader's `ChannelMembers` row, `LastUpdateAt` left out (it is the clock).
async fn member_row(pool: &sqlx::PgPool, channel: &str, user: &str) -> Vec<Option<i64>> {
    let row: MemberRow = sqlx::query_as(
        "SELECT lastviewedat, msgcount, msgcountroot, mentioncount, mentioncountroot, \
             urgentmentioncount FROM channelmembers WHERE channelid = $1 AND userid = $2",
    )
    .bind(channel)
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("the member row");
    vec![row.0, row.1, row.2, row.3, row.4, row.5]
}

/// The reader's `ThreadMemberships` row for a thread as `(following, lastviewed, unreadmentions)`
/// plus the raw `lastupdated`, or `None`.
async fn thread_row(pool: &sqlx::PgPool, thread: &str, user: &str) -> Option<ThreadRow> {
    sqlx::query_as(
        "SELECT following, lastviewed, unreadmentions, lastupdated FROM threadmemberships \
         WHERE postid = $1 AND userid = $2",
    )
    .bind(thread)
    .bind(user)
    .fetch_optional(pool)
    .await
    .expect("the membership query")
}

/// Put the reader's membership of `thread` back to `before` (deleting it when there was none).
async fn restore_thread_row(
    pool: &sqlx::PgPool,
    thread: &str,
    user: &str,
    before: Option<ThreadRow>,
) {
    sqlx::query("DELETE FROM threadmemberships WHERE postid = $1 AND userid = $2")
        .bind(thread)
        .bind(user)
        .execute(pool)
        .await
        .expect("the membership is removed");
    if let Some((following, viewed, mentions, updated)) = before {
        sqlx::query(
            "INSERT INTO threadmemberships (postid, userid, following, lastviewed, lastupdated, \
             unreadmentions) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(thread)
        .bind(user)
        .bind(following)
        .bind(viewed)
        .bind(updated)
        .bind(mentions)
        .execute(pool)
        .await
        .expect("the membership is restored");
    }
}

/// One mark on one server: `(status, body, post_unread events, thread_updated events)`.
async fn mark(
    client: &reqwest::Client,
    base: &str,
    fixture: &Fixture,
    post: &str,
    body: &str,
) -> (u16, String, Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let mut probe = SocketProbe::connect(base, &fixture.reader.token).await;
    let path = format!(
        "{base}/api/v4/users/{}/posts/{post}/set_unread",
        fixture.reader.id
    );
    let response = client
        .post(&path)
        .header("Authorization", format!("Bearer {}", fixture.reader.token))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .expect("answers");
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), &path);
    }
    let text = response.text().await.expect("a body");
    let wants_thread =
        body.contains("false") && post != fixture.mark_root && post != fixture.urgent_root;
    probe
        .collect_until(Duration::from_secs(5), |frames| {
            let unread = frames.iter().any(|f| f["event"] == "post_unread");
            let thread = frames.iter().any(|f| f["event"] == "thread_updated");
            unread && (thread || !wants_thread)
        })
        .await;
    probe.collect_for(Duration::from_millis(300)).await;
    let thread_events = probe.events_named("thread_updated");
    (
        status,
        text,
        probe.events_named("post_unread"),
        thread_events,
    )
}

/// Go, then (with the thread membership put back) this server; the answers, the rows and the
/// events must agree. Returns Go's parsed body.
async fn mark_on_both(
    client: &reqwest::Client,
    pool: &sqlx::PgPool,
    fixture: &Fixture,
    post: &str,
    thread: &str,
    body: &str,
) -> serde_json::Value {
    let before = thread_row(pool, thread, &fixture.reader.id).await;

    let go = mark(client, GO, fixture, post, body).await;
    let go_member = member_row(pool, &fixture.channel, &fixture.reader.id).await;
    let go_thread = thread_row(pool, thread, &fixture.reader.id).await;

    restore_thread_row(pool, thread, &fixture.reader.id, before).await;

    let rs = mark(client, RUST, fixture, post, body).await;
    let rs_member = member_row(pool, &fixture.channel, &fixture.reader.id).await;
    let rs_thread = thread_row(pool, thread, &fixture.reader.id).await;

    let context = format!("{post} {body}");
    assert_eq!(go.0, 200, "{context}: {}", go.1);
    assert_eq!((rs.0, &rs.1), (go.0, &go.1), "{context}: the response");
    assert_eq!(rs_member, go_member, "{context}: the channel member row");
    let strip = |row: Option<ThreadRow>| {
        row.map(|(following, viewed, mentions, _)| (following, viewed, mentions))
    };
    assert_eq!(
        strip(rs_thread),
        strip(go_thread),
        "{context}: the thread membership"
    );
    if thread != post {
        assert_eq!(go.3.len(), 1, "{context}: Go published the followed thread");
    }
    let without_seq = |events: Vec<serde_json::Value>| -> Vec<serde_json::Value> {
        events
            .into_iter()
            .map(|mut e| {
                e.as_object_mut().map(|o| o.remove("seq"));
                e
            })
            .collect()
    };
    assert_eq!(
        without_seq(rs.2),
        without_seq(go.2),
        "{context}: post_unread"
    );
    assert_eq!(
        without_seq(rs.3),
        without_seq(go.3),
        "{context}: thread_updated"
    );
    serde_json::from_str(&go.1).expect("a ChannelUnreadAt")
}

#[tokio::test]
async fn set_unread_in_an_open_channel_counts_mentions_as_go_does() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let fixture = fixture(&client, &admin, &pool, "sumopen").await;

    for body in [
        r#"{"collapsed_threads_supported":true}"#,
        r#"{"collapsed_threads_supported":false}"#,
    ] {
        let got = mark_on_both(
            &client,
            &pool,
            &fixture,
            &fixture.mark_root,
            &fixture.mark_root,
            body,
        )
        .await;
        // Six posts after `A` mention the reader (the add, `@reader`, `zebra`, `@channel` and two
        // comment mentions), four of them roots, one of those urgent. Asserted as an ordering
        // rather than as numbers: the numbers are Go's, and the comparison above is against Go.
        let mentions = got["mention_count"].as_i64().expect("a count");
        let roots = got["mention_count_root"].as_i64().expect("a count");
        let urgent = got["urgent_mention_count"].as_i64().expect("a count");
        assert!(
            mentions > roots && roots > urgent && urgent > 0,
            "the fixture must discriminate: {got}"
        );
    }

    // The marked post is itself an urgent root mention: `GetPriorityForPost` on the subject.
    let got = mark_on_both(
        &client,
        &pool,
        &fixture,
        &fixture.urgent_root,
        &fixture.urgent_root,
        r#"{"collapsed_threads_supported":true}"#,
    )
    .await;
    assert_eq!(got["urgent_mention_count"], 1, "{got}");

    delete_channel(&client, &admin, &fixture.channel).await;
    delete_plain_user(&client, &admin, &fixture.reader.id).await;
}

#[tokio::test]
async fn a_reply_without_crt_support_follows_the_thread_as_go_does() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let fixture = fixture(&client, &admin, &pool, "sumreply").await;
    let body = r#"{"collapsed_threads_supported":false}"#;

    // A thread the reader replied in: the membership exists and is rewritten.
    let joined: String = sqlx::query_scalar("SELECT rootid FROM posts WHERE id = $1")
        .bind(&fixture.followed_reply)
        .fetch_one(&pool)
        .await
        .expect("the root");
    assert!(
        thread_row(&pool, &joined, &fixture.reader.id)
            .await
            .is_some(),
        "the reader's reply made them a member"
    );
    let got = mark_on_both(
        &client,
        &pool,
        &fixture,
        &fixture.followed_reply,
        &joined,
        body,
    )
    .await;
    assert_eq!(
        got["mention_count_root"], 0,
        "the reply arm zeroes the root count"
    );
    assert_eq!(
        thread_row(&pool, &joined, &fixture.reader.id)
            .await
            .and_then(|row| row.2),
        Some(1),
        "countThreadMentions counts the zebra reply and not the plain one after it"
    );

    // A thread the reader never touched: the membership is created.
    let untouched: String = sqlx::query_scalar("SELECT rootid FROM posts WHERE id = $1")
        .bind(&fixture.unfollowed_reply)
        .fetch_one(&pool)
        .await
        .expect("the root");
    assert!(
        thread_row(&pool, &untouched, &fixture.reader.id)
            .await
            .is_none()
    );
    mark_on_both(
        &client,
        &pool,
        &fixture,
        &fixture.unfollowed_reply,
        &untouched,
        body,
    )
    .await;
    assert_eq!(
        thread_row(&pool, &untouched, &fixture.reader.id)
            .await
            .map(|row| row.0),
        Some(Some(true)),
        "the unread mark follows the thread"
    );

    delete_channel(&client, &admin, &fixture.channel).await;
    delete_plain_user(&client, &admin, &fixture.reader.id).await;
}
