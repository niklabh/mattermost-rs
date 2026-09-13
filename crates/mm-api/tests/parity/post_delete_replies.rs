//! Cross-server parity for `DELETE /api/v4/posts/{post_id}` on a **reply** — the
//! `RemoveNotifications` pass that used to forward it.
//!
//! ```sh
//! scripts/parity.sh --test parity post_delete_replies
//! ```
//!
//! Twin threads again: a root with a mentioning reply on each server's side, the reply deleted
//! on that server, and the mentioned member's `ThreadMemberships` row and `thread_updated`
//! frame compared. The rows are read directly, because the route's body is the deleted post and
//! shows nothing of this.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, add_user_to_channel, client, create_channel_typed,
    create_plain_user, create_team, fixture_pool, go_minted_token, plain_username,
    purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    channel_id: String,
    /// In the channel; the one the replies mention.
    target: common::PlainUser,
    target_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "cpdr").await;
            let channel_id = create_channel_typed(client, token, &team_id, "cpdr", "O").await;
            let target = create_plain_user(client, token, &team_id, "cpdr").await;
            add_user_to_channel(client, token, &channel_id, &target.id).await;
            Fixture {
                team_id,
                channel_id,
                target_name: plain_username("cpdr"),
                target,
            }
        })
        .await
}

async fn create(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    message: &str,
    root_id: &str,
) -> (String, i64) {
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "channel_id": channel_id, "message": message, "root_id": root_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "posting {message:?} failed");
    let body: serde_json::Value = response.json().await.expect("a post");
    (
        body["id"].as_str().expect("an id").to_owned(),
        body["create_at"].as_i64().expect("create_at"),
    )
}

async fn delete_on(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> (u16, bool, String) {
    let response = client
        .delete(format!("{base}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, served, response.text().await.expect("a body"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Membership {
    last_viewed: i64,
    unread_mentions: i64,
}

async fn membership(user_id: &str, post_id: &str) -> Option<Membership> {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT COALESCE(lastviewed, 0), COALESCE(unreadmentions, 0) \
           FROM threadmemberships WHERE userid = $1 AND postid = $2",
    )
    .bind(user_id)
    .bind(post_id)
    .fetch_optional(&pool)
    .await
    .expect("reads");
    row.map(|(last_viewed, unread_mentions)| Membership {
        last_viewed,
        unread_mentions,
    })
}

/// The member's `UnreadMentions` once the pass has settled. Go runs `RemoveNotifications`
/// from a goroutine after answering the delete, so a read straight after the response races
/// it — measured: the licensed Go lost that race every time, the unlicensed one never. Poll
/// for `expected` for up to two seconds, then answer whatever the row holds.
async fn unread_mentions_settled(user_id: &str, post_id: &str, expected: i64) -> Option<i64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let now = membership(user_id, post_id)
            .await
            .map(|m| m.unread_mentions);
        if now == Some(expected) || tokio::time::Instant::now() >= deadline {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn set_unread_mentions(user_id: &str, post_id: &str, unread_mentions: i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query(
        "UPDATE threadmemberships SET unreadmentions = $1 WHERE userid = $2 AND postid = $3",
    )
    .bind(unread_mentions)
    .bind(user_id)
    .bind(post_id)
    .execute(&pool)
    .await
    .expect("updates");
}

async fn set_last_viewed(user_id: &str, post_id: &str, last_viewed: i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query("UPDATE threadmemberships SET lastviewed = $1 WHERE userid = $2 AND postid = $3")
        .bind(last_viewed)
        .bind(user_id)
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("updates");
}

/// A root by the admin with two mentioning replies, so the target holds **two** unread
/// mentions and a decrement is distinguishable from a reset.
async fn thread_with_two_mentions(
    client: &reqwest::Client,
    token: &str,
    f: &Fixture,
) -> (String, (String, i64), (String, i64)) {
    let (root, _) = create(client, token, &f.channel_id, "cpdr root", "").await;
    let at = format!("@{}", f.target_name);
    let first = create(
        client,
        token,
        &f.channel_id,
        &format!("cpdr one {at}"),
        &root,
    )
    .await;
    let second = create(
        client,
        token,
        &f.channel_id,
        &format!("cpdr two {at}"),
        &root,
    )
    .await;
    assert_eq!(
        unread_mentions_settled(&f.target.id, &root, 2).await,
        Some(2),
        "the fixture: two unread mentions before the delete"
    );
    (root, first, second)
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// Deleting a reply that mentioned a member takes one unread mention off their membership, on
/// both, and the route is served here.
#[tokio::test]
async fn deleting_a_mentioning_reply_decrements_the_unread_mention_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for base in [GO, RUST] {
        let (root, first, _second) = thread_with_two_mentions(&client, &token, f).await;
        let (status, served, body) = delete_on(&client, base, &token, &first.0).await;
        assert_eq!(status, 200, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}: served by");
        assert_eq!(
            unread_mentions_settled(&f.target.id, &root, 1).await,
            Some(1),
            "{base}: one of two unread mentions removed"
        );
    }
}

/// The two skips: a member who has viewed the thread **after** the reply keeps their count,
/// and so does one with no unread mention at all. A reply with no mention moves nobody.
#[tokio::test]
async fn a_viewed_thread_and_a_zero_count_are_left_alone_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for base in [GO, RUST] {
        // Viewed after the reply: `LastViewed > CreateAt`.
        let (root, first, _second) = thread_with_two_mentions(&client, &token, f).await;
        set_last_viewed(&f.target.id, &root, first.1 + 1).await;
        let (status, _, body) = delete_on(&client, base, &token, &first.0).await;
        assert_eq!(status, 200, "{base}: {body}");
        assert_eq!(
            unread_mentions_settled(&f.target.id, &root, 2).await,
            Some(2),
            "{base}: viewed after the reply, nothing removed"
        );

        // Viewed at exactly the reply's create_at: `>` is strict, so the mention is removed.
        let (root, first, _second) = thread_with_two_mentions(&client, &token, f).await;
        set_last_viewed(&f.target.id, &root, first.1).await;
        let (status, _, _) = delete_on(&client, base, &token, &first.0).await;
        assert_eq!(status, 200);
        assert_eq!(
            unread_mentions_settled(&f.target.id, &root, 1).await,
            Some(1),
            "{base}: at exactly the reply's time the mention is still unread"
        );

        // Already at zero: `UnreadMentions == 0` skips, and the count is not driven negative.
        let (root, first, _second) = thread_with_two_mentions(&client, &token, f).await;
        set_unread_mentions(&f.target.id, &root, 0).await;
        let (status, _, _) = delete_on(&client, base, &token, &first.0).await;
        assert_eq!(status, 200);
        assert_eq!(
            unread_mentions_settled(&f.target.id, &root, 0).await,
            Some(0),
            "{base}: a zero count stays zero"
        );

        // No mention in the deleted reply: the count stays.
        let (root, _first, _second) = thread_with_two_mentions(&client, &token, f).await;
        let (plain, _) = create(&client, &token, &f.channel_id, "cpdr plain", &root).await;
        let (status, _, _) = delete_on(&client, base, &token, &plain).await;
        assert_eq!(status, 200);
        assert_eq!(
            unread_mentions_settled(&f.target.id, &root, 2).await,
            Some(2),
            "{base}: a reply mentioning nobody removes nothing"
        );
    }
}

/// The decrement publishes `thread_updated` to the member with the thread as they now see it
/// and both `previous_*` counters at zero, on both.
#[tokio::test]
async fn the_decrement_publishes_thread_updated_with_zero_previous_counters() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for base in [GO, RUST] {
        let (root, first, _second) = thread_with_two_mentions(&client, &token, f).await;
        let mut socket = SocketProbe::connect(base, &f.target.token).await;
        let (status, _, body) = delete_on(&client, base, &token, &first.0).await;
        assert_eq!(status, 200, "{base}: {body}");

        let root_id = root.clone();
        assert!(
            socket
                .collect_until(Duration::from_millis(2500), |frames| {
                    frames.iter().any(|f| {
                        f["event"] == "thread_updated"
                            && f["data"]["thread"]
                                .as_str()
                                .is_some_and(|t| t.contains(&root_id))
                    })
                })
                .await,
            "{base}: no thread_updated for the member: {:?}",
            socket.raw
        );
        let frame = socket
            .events_named("thread_updated")
            .into_iter()
            .find(|f| {
                f["data"]["thread"]
                    .as_str()
                    .is_some_and(|t| t.contains(&root))
            })
            .expect("the frame");
        let thread: serde_json::Value =
            serde_json::from_str(frame["data"]["thread"].as_str().unwrap()).expect("JSON");
        assert_eq!(thread["id"], root.as_str(), "{base}");
        assert_eq!(
            thread["unread_mentions"], 1,
            "{base}: as the member now sees it"
        );
        assert_eq!(thread["unread_replies"], 1, "{base}: the surviving reply");
        assert_eq!(thread["reply_count"], 1, "{base}");
        assert_eq!(
            frame["data"]["previous_unread_mentions"], 0,
            "{base}: hardcoded zero"
        );
        assert_eq!(
            frame["data"]["previous_unread_replies"], 0,
            "{base}: hardcoded zero"
        );
        assert_eq!(
            frame["broadcast"]["user_id"],
            f.target.id.as_str(),
            "{base}"
        );
        assert_eq!(frame["broadcast"]["team_id"], f.team_id.as_str(), "{base}");
        assert!(
            thread["participants"][0]["username"]
                .as_str()
                .is_some_and(|u| !u.is_empty()),
            "{base}: extended participants"
        );
    }
}

/// Deleting the **root** runs nothing of this: the target's membership row is untouched by the
/// notification pass (the row itself goes with the thread on both, through the store).
#[tokio::test]
async fn deleting_the_root_is_not_a_notification_pass() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut outcomes = Vec::new();
    for base in [GO, RUST] {
        let (root, _first, _second) = thread_with_two_mentions(&client, &token, f).await;
        let (status, served, body) = delete_on(&client, base, &token, &root).await;
        assert_eq!(status, 200, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        outcomes.push(
            membership(&f.target.id, &root)
                .await
                .map(|m| m.unread_mentions),
        );
    }
    assert_eq!(
        outcomes[0], outcomes[1],
        "what a root delete leaves of the member's row differs between Go and us"
    );
}

/// On the **licensed** pair `allowGroupMentions` is true, so the pre-delete gate runs the
/// mention pass to look for a group mention — and a reply that mentions only a *user* must
/// still be served and decremented there. This is the only pair on which that gate's decision
/// is reachable at all; a mutation forwarding on any mention survived the unlicensed suite.
#[tokio::test]
async fn on_the_licensed_pair_a_user_mention_is_still_served_and_only_a_group_mention_forwards() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for base in [pair.go.as_str(), pair.rust.as_str()] {
        let (root, first, _second) = thread_with_two_mentions(&client, &token, f).await;
        let (status, served, body) = delete_on(&client, base, &token, &first.0).await;
        assert_eq!(status, 200, "{base}: {body}");
        assert_eq!(
            served,
            base == pair.rust,
            "{base}: a user mention is served on the licensed server too"
        );
        assert_eq!(
            unread_mentions_settled(&f.target.id, &root, 1).await,
            Some(1),
            "{base}: decremented on the licensed pair"
        );
    }
}
