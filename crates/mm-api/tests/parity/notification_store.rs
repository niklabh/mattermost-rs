//! Go as the oracle for two store **writes** `App.SendNotifications` makes:
//! `SqlChannelStore::increment_mention_count` and the `update_participants` branch of
//! `SqlThreadStore::maintain_membership`. Neither is a route yet, so each is called directly
//! against the shared database and the row it left is read back through a route both servers
//! serve:
//!
//! ```text
//! GET /api/v4/channels/{channel_id}/members/{user_id}                        the three counters
//! GET /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}            Threads.Participants
//! ```
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity notification_store
//! ```
//!
//! Byte-identity between the two servers' reads is only half of it — both read the same row.
//! The other half is the *values* in Go's body: a counter one higher than before the write, a
//! participant list one longer with the follower last. That is Go's own decoding of what the
//! port wrote, which no store-level assertion can substitute for.

use crate::common;

use common::{
    add_user_to_channel, client, create_channel_typed, create_plain_user, create_team,
    fetch_both_stable, fixture_pool, go_minted_token, logged_in_user_id, post_message,
    purge_api_fixtures, stack_enabled,
};
use mm_store::channel_store::ChannelStore;
use mm_store::thread_store::{ThreadMembershipOpts, ThreadStore};
use mm_store::{SqlChannelStore, SqlThreadStore};

fn parse(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("JSON: {e}: {}", String::from_utf8_lossy(body)))
}

/// A `Threads` row is written in the reply's own transaction on both servers, but the reply is
/// posted through Go and the store read here is a separate connection; wait for it rather than
/// assume ordering.
async fn await_thread_row(pool: &sqlx::PgPool, root_id: &str) {
    for attempt in 1..=40_u64 {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM threads WHERE postid = $1")
            .bind(root_id)
            .fetch_one(pool)
            .await
            .expect("the count runs");
        if count == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25 * attempt)).await;
    }
    panic!("the Threads row for {root_id} never appeared");
}

/// Each `(is_root, is_urgent)` pair moves exactly the counters Go names, as Go reads them back.
#[tokio::test]
async fn increment_mention_count_is_what_go_reads_back() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;
    let Some(pool) = fixture_pool().await else {
        eprintln!("skipping: DATABASE_URL is not set");
        return;
    };

    let team_id = create_team(&client, &token, "notifinc").await;
    let channel_id = create_channel_typed(&client, &token, &team_id, "notifinc", "O").await;
    let plain = create_plain_user(&client, &token, &team_id, "notifinc").await;
    add_user_to_channel(&client, &token, &channel_id, &plain.id).await;

    let path = format!("/api/v4/channels/{channel_id}/members/{}", plain.id);
    let (go_before, rs_before) = fetch_both_stable(&client, &plain.token, &path).await;
    assert_eq!(
        go_before, rs_before,
        "{path} must be byte-identical before the write"
    );
    let before = parse(&go_before);
    let counters = |body: &serde_json::Value| {
        (
            body["mention_count"].as_i64().expect("mention_count"),
            body["mention_count_root"]
                .as_i64()
                .expect("mention_count_root"),
            body["urgent_mention_count"]
                .as_i64()
                .expect("urgent_mention_count"),
        )
    };
    let (m, r, u) = counters(&before);
    let mut last_update_at = before["last_update_at"].as_i64().expect("last_update_at");

    let store = SqlChannelStore::new(pool);
    let listed = [plain.id.clone()];
    for (is_root, is_urgent, expected) in [
        (true, true, (m + 1, r + 1, u + 1)),
        (false, false, (m + 2, r + 1, u + 1)),
        (true, false, (m + 3, r + 2, u + 1)),
        (false, true, (m + 4, r + 2, u + 2)),
    ] {
        // Both servers' `GetMillis` are millisecond clocks; a write in the same millisecond as
        // the previous one would leave `last_update_at` equal, which the `>` below would call a
        // missed write. One tick is enough.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        store
            .increment_mention_count(&channel_id, &listed, is_root, is_urgent)
            .await
            .expect("increments");

        let (go, rs) = fetch_both_stable(&client, &plain.token, &path).await;
        assert_eq!(
            go, rs,
            "{path} must be byte-identical after is_root={is_root} is_urgent={is_urgent}"
        );
        let after = parse(&go);
        assert_eq!(
            counters(&after),
            expected,
            "Go's reading after is_root={is_root} is_urgent={is_urgent}: {after}"
        );
        let moved = after["last_update_at"].as_i64().expect("last_update_at");
        assert!(
            moved > last_update_at,
            "last_update_at moves on every write: {moved} after {last_update_at}"
        );
        last_update_at = moved;
    }
}

/// A follower inserted with `update_participants` is the **last** entry Go renders, after the
/// replier Go put there itself; the bare and `extended=true` shapes both agree across servers.
#[tokio::test]
async fn a_follow_with_participants_is_what_go_renders() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;
    let Some(pool) = fixture_pool().await else {
        eprintln!("skipping: DATABASE_URL is not set");
        return;
    };

    let team_id = create_team(&client, &token, "notifpart").await;
    let channel_id = create_channel_typed(&client, &token, &team_id, "notifpart", "O").await;
    let plain = create_plain_user(&client, &token, &team_id, "notifpart").await;
    add_user_to_channel(&client, &token, &channel_id, &plain.id).await;

    // The admin roots and replies, so Go's own post path writes `Participants = [admin]`.
    let root = post_message(&client, &token, &channel_id, "root", None).await;
    post_message(&client, &token, &channel_id, "reply", Some(&root)).await;
    await_thread_row(&pool, &root).await;

    SqlThreadStore::new(pool)
        .maintain_membership(
            &plain.id,
            &root,
            ThreadMembershipOpts {
                following: true,
                increment_mentions: false,
                update_following: true,
                update_viewed_timestamp: true,
                update_participants: true,
            },
        )
        .await
        .expect("inserts the membership and appends the participant");

    let admin_id = logged_in_user_id();
    for query in ["", "?extended=true"] {
        let path = format!(
            "/api/v4/users/{}/teams/{team_id}/threads/{root}{query}",
            plain.id
        );
        let (go, rs) = fetch_both_stable(&client, &plain.token, &path).await;
        assert_eq!(go, rs, "{path} must be byte-identical");
        let body = parse(&go);
        let ids: Vec<&str> = body["participants"]
            .as_array()
            .unwrap_or_else(|| panic!("participants is an array: {body}"))
            .iter()
            .map(|p| p["id"].as_str().expect("participant id"))
            .collect();
        assert_eq!(
            ids,
            [admin_id, plain.id.as_str()],
            "Go renders the replier first and the appended follower last ({path}): {body}"
        );
    }
}
