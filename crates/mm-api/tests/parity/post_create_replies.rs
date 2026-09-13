//! Cross-server parity for the two `POST /api/v4/posts` shapes the notification pass unlocked:
//! a **reply**, and a message that **mentions** someone.
//!
//! ```sh
//! scripts/parity.sh --test parity post_create_replies
//! ```
//!
//! # Twin threads, and the database behind them
//!
//! A create answers a body with a fresh id, so a Go post and a Rust post are never byte-equal.
//! Each test therefore performs the same action on each server — a reply to its own twin root,
//! a mention in the same channel — and compares what the two leave behind: the reply body with
//! its per-request fields blanked, the `Threads` row through the thread read route, the
//! `ThreadMemberships` and `ChannelMembers` rows read directly (Go caches channel members, so a
//! Rust write is invisible to a Go read of that row until the cache expires — [D-087]), and the
//! frames on a recipient's socket.
//!
//! # Collapsed threads are `always_on` on the stack
//!
//! So every user is a CRT user: `thread_updated` goes to every follower, a reply never marks the
//! channel viewed, and the comment-thread mentions (`comments: any|root`) are unreachable —
//! Go skips them for a CRT user. That last arm is ported and unit-tested but not measured here.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, add_user_to_channel, client, create_channel_typed,
    create_plain_user, create_team, fixture_pool, go_minted_token, logged_in_user_id,
    plain_username, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    channel_id: String,
    /// In the channel; follows threads and receives `thread_updated`.
    reader: common::PlainUser,
    /// In the channel; `mention_keys: pineapple`, so a keyword mentions them.
    keyed: common::PlainUser,
    keyed_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "cprp").await;
            let channel_id = create_channel_typed(client, token, &team_id, "cprp", "O").await;
            let reader = create_plain_user(client, token, &team_id, "cprp").await;
            let keyed = create_plain_user(client, token, &team_id, "cprpk").await;
            add_user_to_channel(client, token, &channel_id, &reader.id).await;
            add_user_to_channel(client, token, &channel_id, &keyed.id).await;
            wait_for_join_mentions(&channel_id, &[&reader.id, &keyed.id]).await;

            let response = client
                .put(format!("{GO}/api/v4/users/{}/patch", keyed.id))
                .header("Authorization", format!("Bearer {token}"))
                .json(&serde_json::json!({
                    "notify_props": {
                        "channel": "true", "comments": "never", "desktop": "mention",
                        "desktop_sound": "true", "email": "true", "first_name": "false",
                        "mention_keys": "pineapple", "push": "mention", "push_status": "online",
                    }
                }))
                .send()
                .await
                .expect("Go answers");
            assert!(
                response.status().is_success(),
                "patching the keyed user failed"
            );

            Fixture {
                team_id,
                channel_id,
                reader,
                keyed_name: plain_username("cprpk"),
                keyed,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// requests and rows
// ---------------------------------------------------------------------------------------------

/// `POST /api/v4/posts` against `base`; the status, whether we served it, and the body.
async fn create(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    message: &str,
    root_id: &str,
) -> (u16, bool, serde_json::Value) {
    let response = client
        .post(format!("{base}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": message,
            "root_id": root_id,
        }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    let body = response.json().await.unwrap_or(serde_json::Value::Null);
    (status, served, body)
}

/// A channel of this test's own, with the reader and the keyed user in it — for the tests that
/// read counters other tests in this file would move concurrently in the shared channel.
async fn own_channel(client: &reqwest::Client, token: &str, f: &Fixture, tag: &str) -> String {
    let channel_id = create_channel_typed(client, token, &f.team_id, tag, "O").await;
    add_user_to_channel(client, token, &channel_id, &f.reader.id).await;
    add_user_to_channel(client, token, &channel_id, &f.keyed.id).await;
    wait_for_join_mentions(&channel_id, &[&f.reader.id, &f.keyed.id]).await;
    channel_id
}

/// Go posts the "added to the channel" system message from a goroutine, and that post carries
/// an implicit mention of the added user (`getExplicitMentionsAndKeywords`, the
/// `system_add_to_channel` arm) — so the member's `MentionCount` becomes 1 some milliseconds
/// **after** the add request returned. A test that reads the counter before that lands sees a
/// delta of two on its own post. Measured: two of three runs. Wait for the settled state.
async fn wait_for_join_mentions(channel_id: &str, user_ids: &[&str]) {
    for user_id in user_ids {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if member_counts(channel_id, user_id).await.0 >= 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{user_id}'s join mention never landed in {channel_id}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    // The count is written before the join goroutine returns; one more tick for the rest of it.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// A root by the admin, on Go, as every fixture post is.
async fn root(client: &reqwest::Client, token: &str, channel_id: &str, message: &str) -> String {
    let (status, _, body) = create(client, GO, token, channel_id, message, "").await;
    assert_eq!(status, 201, "{}", body);
    body["id"].as_str().expect("an id").to_owned()
}

/// The per-request fields, plus the ones a twin thread makes different by construction.
fn normalised(post: &serde_json::Value) -> serde_json::Value {
    let mut post = post.clone();
    let obj = post.as_object_mut().expect("a post object");
    for key in ["id", "create_at", "update_at", "root_id", "pending_post_id"] {
        obj.insert(key.to_owned(), serde_json::json!(0));
    }
    post
}

async fn thread_for(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    team_id: &str,
    thread_id: &str,
) -> serde_json::Value {
    let response = client
        .get(format!(
            "{base}/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("answers");
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    assert_eq!(status, 200, "{base} thread read: {body}");
    body
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Membership {
    following: bool,
    unread_mentions: i64,
}

async fn membership(user_id: &str, post_id: &str) -> Option<Membership> {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let row: Option<(bool, i64)> = sqlx::query_as(
        "SELECT COALESCE(following, FALSE), COALESCE(unreadmentions, 0) \
           FROM threadmemberships WHERE userid = $1 AND postid = $2",
    )
    .bind(user_id)
    .bind(post_id)
    .fetch_optional(&pool)
    .await
    .expect("reads");
    row.map(|(following, unread_mentions)| Membership {
        following,
        unread_mentions,
    })
}

/// `(mention_count, mention_count_root, urgent_mention_count, last_viewed_at)`.
async fn member_counts(channel_id: &str, user_id: &str) -> (i64, i64, i64, i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as(
        "SELECT mentioncount, mentioncountroot, urgentmentioncount, lastviewedat \
           FROM channelmembers WHERE channelid = $1 AND userid = $2",
    )
    .bind(channel_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the member row")
}

async fn participants(post_id: &str) -> Vec<String> {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT participants FROM threads WHERE postid = $1")
            .bind(post_id)
            .fetch_optional(&pool)
            .await
            .expect("reads")
            .flatten();
    value
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// A reply is served, answers the same shape as Go's, and leaves the same `Threads` row: one
/// reply, the poster as the only participant, `last_reply_at` at the reply.
#[tokio::test]
async fn a_reply_answers_the_same_shape_and_the_same_thread_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let root_go = root(&client, &token, &f.channel_id, "cprp root go").await;
    let root_rs = root(&client, &token, &f.channel_id, "cprp root rs").await;

    let (go_status, go_served, go_reply) =
        create(&client, GO, &token, &f.channel_id, "cprp reply", &root_go).await;
    let (rs_status, rs_served, rs_reply) =
        create(&client, RUST, &token, &f.channel_id, "cprp reply", &root_rs).await;
    assert_eq!(go_status, 201, "{go_reply}");
    assert_eq!(rs_status, 201, "{rs_reply}");
    assert!(!go_served && rs_served);
    assert_eq!(rs_reply["root_id"], root_rs.as_str());
    assert_eq!(
        normalised(&go_reply),
        normalised(&rs_reply),
        "the reply bodies differ"
    );
    assert_eq!(
        rs_reply["reply_count"], 1,
        "populateReplyCount counts the reply itself"
    );

    // The thread row, read through Go on both — the store is shared.
    let go_thread = thread_for(&client, GO, &token, "me", &f.team_id, &root_go).await;
    let rs_thread = thread_for(&client, GO, &token, "me", &f.team_id, &root_rs).await;
    for (label, thread, reply) in [("go", &go_thread, &go_reply), ("rs", &rs_thread, &rs_reply)] {
        assert_eq!(thread["reply_count"], 1, "{label}");
        assert_eq!(thread["last_reply_at"], reply["create_at"], "{label}");
        assert_eq!(
            thread["unread_replies"], 0,
            "{label}: the poster read their own reply"
        );
        assert_eq!(thread["unread_mentions"], 0, "{label}");
        assert_eq!(
            thread["post"]["update_at"], reply["create_at"],
            "{label}: the root's UpdateAt moved to the reply"
        );
    }
    assert_eq!(
        participants(&root_go).await,
        vec![logged_in_user_id().to_owned()]
    );
    assert_eq!(
        participants(&root_rs).await,
        vec![logged_in_user_id().to_owned()]
    );

    // The poster follows the thread, and their LastViewed was moved by the commenter arm.
    let go_m = membership(logged_in_user_id(), &root_go)
        .await
        .expect("Go followed");
    let rs_m = membership(logged_in_user_id(), &root_rs)
        .await
        .expect("we followed");
    assert_eq!(go_m, rs_m);
    assert!(rs_m.following);
    assert_eq!(rs_m.unread_mentions, 0);
}

/// A second replier is appended to the participants, and a repeat replier moves to the end.
#[tokio::test]
async fn participants_are_ordered_by_latest_reply_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let root_go = root(&client, &token, &f.channel_id, "cprp part go").await;
    let root_rs = root(&client, &token, &f.channel_id, "cprp part rs").await;

    for (base, root_id) in [(GO, &root_go), (RUST, &root_rs)] {
        for (who, message) in [
            (&f.reader.token, "one"),
            (&token, "two"),
            (&f.reader.token, "three"),
        ] {
            let (status, _, body) =
                create(&client, base, who, &f.channel_id, message, root_id).await;
            assert_eq!(status, 201, "{base}: {body}");
        }
    }
    let expected = vec![logged_in_user_id().to_owned(), f.reader.id.clone()];
    assert_eq!(participants(&root_go).await, expected, "Go");
    assert_eq!(participants(&root_rs).await, expected, "us");

    let go_thread = thread_for(&client, GO, &token, "me", &f.team_id, &root_go).await;
    let rs_thread = thread_for(&client, GO, &token, "me", &f.team_id, &root_rs).await;
    assert_eq!(go_thread["reply_count"], 3);
    assert_eq!(rs_thread["reply_count"], 3);
}

/// A mention raises the member's counters by the same amounts on both: `@user` and a keyword
/// are mentions, the poster's own name is not, and a reply raises `mention_count` but not
/// `mention_count_root`.
#[tokio::test]
async fn mention_counts_move_identically() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let channel_id = own_channel(&client, &token, f, "cprpcount").await;
    let at = format!("@{}", f.keyed_name);
    let me = format!(
        "@{}",
        common::username_of(&client, &token, logged_in_user_id()).await
    );

    // (message, root?, expected delta for the keyed user: (count, root, urgent))
    let cases: Vec<(String, bool, (i64, i64, i64))> = vec![
        (format!("cprp hello {at}"), false, (1, 1, 0)),
        ("cprp pineapple season".to_owned(), false, (1, 1, 0)),
        ("cprp nothing for anyone".to_owned(), false, (0, 0, 0)),
        (format!("cprp reply {at}"), true, (1, 0, 0)),
        ("cprp @channel all of you".to_owned(), false, (1, 1, 0)),
        // The poster naming themselves, in a **reply**: `removeMention(post.UserId)` — nobody's
        // counter moves, and the poster's own is asserted at zero below. A reply, because a root
        // post is followed by `MarkChannelsAsViewed`, which zeroes the poster's counter again and
        // hid a dropped removal for a whole batch; a CRT reply skips that reset.
        (format!("cprp note to self {me}"), true, (0, 0, 0)),
    ];
    for (message, is_reply, (d_count, d_root, d_urgent)) in cases {
        for base in [GO, RUST] {
            let root_id = if is_reply {
                root(&client, &token, &channel_id, "cprp mention root").await
            } else {
                String::new()
            };
            let before = member_counts(&channel_id, &f.keyed.id).await;
            let (status, served, body) =
                create(&client, base, &token, &channel_id, &message, &root_id).await;
            assert_eq!(status, 201, "{base} {message:?}: {body}");
            assert_eq!(served, base == RUST, "{base} {message:?}: served by");
            let after = member_counts(&channel_id, &f.keyed.id).await;
            assert_eq!(
                (after.0 - before.0, after.1 - before.1, after.2 - before.2),
                (d_count, d_root, d_urgent),
                "{base} {message:?}: the keyed user's counters"
            );
            // The poster mentions nobody by mentioning themselves.
            let mine = member_counts(&channel_id, logged_in_user_id()).await;
            assert_eq!(mine.0, 0, "{base}: the poster's own count never moves");
        }
    }
}

/// A reply mentioning a member creates that member's thread membership with one unread
/// mention, on both — `MaintainMembership` with `IncrementMentions`.
#[tokio::test]
async fn a_mention_in_a_reply_follows_the_mentioned_user_with_one_unread_mention() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let root_go = root(&client, &token, &f.channel_id, "cprp mfollow go").await;
    let root_rs = root(&client, &token, &f.channel_id, "cprp mfollow rs").await;
    let message = format!("cprp psst @{}", f.keyed_name);

    for (base, root_id) in [(GO, &root_go), (RUST, &root_rs)] {
        assert!(membership(&f.keyed.id, root_id).await.is_none());
        let (status, _, body) =
            create(&client, base, &token, &f.channel_id, &message, root_id).await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(
            membership(&f.keyed.id, root_id).await,
            Some(Membership {
                following: true,
                unread_mentions: 1
            }),
            "{base}: the mentioned member follows with one unread mention"
        );
        // And the thread read for them agrees.
        let thread = thread_for(&client, GO, &f.keyed.token, "me", &f.team_id, root_id).await;
        assert_eq!(thread["unread_mentions"], 1, "{base}");
        assert_eq!(thread["unread_replies"], 1, "{base}");
    }
}

/// The `posted` event on a mentioned member's socket carries the `mentions` hook's field, as a
/// **stringified** array holding only that member; the poster's own socket has no `mentions`.
#[tokio::test]
async fn the_posted_event_carries_mentions_for_the_mentioned_user_only() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let message = format!("cprp ping @{}", f.keyed_name);

    for base in [GO, RUST] {
        let mut keyed_socket = SocketProbe::connect(base, &f.keyed.token).await;
        let mut poster_socket = SocketProbe::connect(base, &token).await;
        let (status, _, body) = create(&client, base, &token, &f.channel_id, &message, "").await;
        assert_eq!(status, 201, "{base}: {body}");
        let post_id = body["id"].as_str().expect("an id").to_owned();
        let posted_for = |frames: &[serde_json::Value]| {
            frames.iter().any(|f| {
                f["event"] == "posted"
                    && f["data"]["post"]
                        .as_str()
                        .is_some_and(|p| p.contains(&post_id))
            })
        };
        assert!(
            keyed_socket
                .collect_until(Duration::from_millis(2500), posted_for)
                .await,
            "{base}: the mentioned user got no posted frame: {:?}",
            keyed_socket.raw
        );
        assert!(
            poster_socket
                .collect_until(Duration::from_millis(2500), posted_for)
                .await,
            "{base}: the poster got no posted frame"
        );
        let keyed_frame = keyed_socket
            .events_named("posted")
            .into_iter()
            .find(|f| {
                f["data"]["post"]
                    .as_str()
                    .is_some_and(|p| p.contains(&post_id))
            })
            .expect("the frame");
        let poster_frame = poster_socket
            .events_named("posted")
            .into_iter()
            .find(|f| {
                f["data"]["post"]
                    .as_str()
                    .is_some_and(|p| p.contains(&post_id))
            })
            .expect("the frame");
        assert_eq!(
            keyed_frame["data"]["mentions"],
            serde_json::Value::String(format!("[\"{}\"]", f.keyed.id)),
            "{base}: the add_mentions hook, stringified"
        );
        assert!(
            poster_frame["data"].get("mentions").is_none(),
            "{base}: the poster is not in their own mentions: {poster_frame}"
        );
        for frame in [&keyed_frame, &poster_frame] {
            assert_eq!(frame["data"]["channel_type"], "O", "{base}");
            assert_eq!(
                frame["data"]["channel_name"]
                    .as_str()
                    .map(|s| s.starts_with("mmrs-parity-")),
                Some(true),
                "{base}"
            );
            assert_eq!(frame["data"]["team_id"], f.team_id.as_str(), "{base}");
            assert_eq!(frame["data"]["set_online"], true, "{base}");
            assert!(
                frame["data"]["sender_name"]
                    .as_str()
                    .is_some_and(|s| s.starts_with('@')),
                "{base}"
            );
        }
    }
}

/// A reply publishes `thread_updated` to each follower with collapsed threads on: the reader
/// who followed the root sees one unread reply; the poster's own frame is zeroed by the
/// commenter arm. `previous_*` are `0` for both — the reader's thread had no unread reply
/// before this one, and the poster's counters were just zeroed.
#[tokio::test]
async fn a_reply_publishes_thread_updated_to_every_follower() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut participant_shapes: Vec<serde_json::Value> = Vec::new();
    for base in [GO, RUST] {
        let root_id = root(&client, &token, &f.channel_id, "cprp tu root").await;
        // The reader follows the root, on Go, so both servers see the same follower set.
        let follow = client
            .put(format!(
                "{GO}/api/v4/users/{}/teams/{}/threads/{root_id}/following",
                f.reader.id, f.team_id
            ))
            .header("Authorization", format!("Bearer {}", f.reader.token))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(follow.status().as_u16(), 200);

        let mut reader_socket = SocketProbe::connect(base, &f.reader.token).await;
        let mut poster_socket = SocketProbe::connect(base, &token).await;
        let (status, _, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cprp tu reply",
            &root_id,
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");

        let want = |frames: &[serde_json::Value]| {
            frames.iter().any(|f| {
                f["event"] == "thread_updated"
                    && f["data"]["thread"]
                        .as_str()
                        .is_some_and(|t| t.contains(&root_id))
            })
        };
        assert!(
            reader_socket
                .collect_until(Duration::from_millis(2500), want)
                .await,
            "{base}: the reader got no thread_updated: {:?}",
            reader_socket.raw
        );
        assert!(
            poster_socket
                .collect_until(Duration::from_millis(2500), want)
                .await,
            "{base}: the poster got no thread_updated: {:?}",
            poster_socket.raw
        );

        let pick = |probe: &SocketProbe| {
            probe
                .events_named("thread_updated")
                .into_iter()
                .find(|f| {
                    f["data"]["thread"]
                        .as_str()
                        .is_some_and(|t| t.contains(&root_id))
                })
                .expect("the frame")
        };
        let reader_frame = pick(&reader_socket);
        let poster_frame = pick(&poster_socket);

        let reader_thread: serde_json::Value =
            serde_json::from_str(reader_frame["data"]["thread"].as_str().unwrap()).expect("JSON");
        let poster_thread: serde_json::Value =
            serde_json::from_str(poster_frame["data"]["thread"].as_str().unwrap()).expect("JSON");

        assert_eq!(reader_thread["id"], root_id.as_str(), "{base}");
        assert_eq!(reader_thread["reply_count"], 1, "{base}");
        assert_eq!(
            reader_thread["unread_replies"], 1,
            "{base}: one unread reply for the reader"
        );
        assert_eq!(reader_thread["unread_mentions"], 0, "{base}");
        assert_eq!(reader_frame["data"]["previous_unread_replies"], 0, "{base}");
        assert_eq!(
            reader_frame["data"]["previous_unread_mentions"], 0,
            "{base}"
        );
        assert_eq!(
            reader_frame["broadcast"]["user_id"],
            f.reader.id.as_str(),
            "{base}"
        );
        assert_eq!(
            reader_frame["broadcast"]["team_id"],
            f.team_id.as_str(),
            "{base}"
        );
        assert_eq!(reader_frame["broadcast"]["channel_id"], "", "{base}");
        // `extended`: the participants are full profiles.
        let participant = &reader_thread["participants"][0];
        assert_eq!(participant["id"], logged_in_user_id(), "{base}");
        assert!(
            participant["username"]
                .as_str()
                .is_some_and(|u| !u.is_empty()),
            "{base}: hydrated"
        );
        // Sanitised with the privacy settings — which show the email on this stack — so the
        // shape is compared against Go's frame below rather than assumed.
        assert!(
            participant.get("password").is_none(),
            "{base}: the sanitiser ran"
        );
        let mut shape = participant.clone();
        shape["update_at"] = serde_json::json!(0);
        participant_shapes.push(shape);

        assert_eq!(
            poster_thread["unread_replies"], 0,
            "{base}: the commenter's own thread is read"
        );
        assert_eq!(poster_thread["unread_mentions"], 0, "{base}");
        assert_eq!(
            poster_frame["broadcast"]["user_id"],
            logged_in_user_id(),
            "{base}"
        );
        // The poster's `LastViewed` moved to now, so their read of the thread shows nothing unread.
        let thread = thread_for(&client, GO, &token, "me", &f.team_id, &root_id).await;
        assert_eq!(thread["unread_replies"], 0, "{base}");
    }
    assert_eq!(
        participant_shapes[0], participant_shapes[1],
        "the hydrated, sanitised participant differs between Go and us"
    );
}

/// A root post marks the channel viewed for the poster; a reply, with collapsed threads on,
/// does not — `isCRTReply` in `CreatePostAsUserWithFlags`.
#[tokio::test]
async fn a_reply_does_not_mark_the_channel_viewed_but_a_root_does() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let channel_id = own_channel(&client, &token, f, "cprpviewed").await;
    let root_id = root(&client, &token, &channel_id, "cprp viewed root").await;

    for base in [GO, RUST] {
        let before = member_counts(&channel_id, logged_in_user_id()).await.3;
        tokio::time::sleep(Duration::from_millis(5)).await;
        let (status, _, _) = create(
            &client,
            base,
            &token,
            &channel_id,
            "cprp viewed reply",
            &root_id,
        )
        .await;
        assert_eq!(status, 201);
        let after_reply = member_counts(&channel_id, logged_in_user_id()).await.3;
        assert_eq!(
            after_reply, before,
            "{base}: a CRT reply leaves LastViewedAt alone"
        );

        tokio::time::sleep(Duration::from_millis(5)).await;
        let (status, _, _) = create(
            &client,
            base,
            &token,
            &channel_id,
            "cprp viewed root two",
            "",
        )
        .await;
        assert_eq!(status, 201);
        let after_root = member_counts(&channel_id, logged_in_user_id()).await.3;
        assert!(
            after_root > after_reply,
            "{base}: a root marks the channel viewed"
        );
    }
}

/// The three shapes the notification pass still hands to Go, each because it ends in text
/// this server cannot mint: an out-of-channel mention, and (on a licence) a group mention.
/// Only the first is reachable on the unlicensed pair; it is forwarded and leaves one row.
#[tokio::test]
async fn an_out_of_channel_mention_is_forwarded_and_leaves_one_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    // A real user who is in the team but not in the channel.
    let outsider = create_plain_user(&client, &token, &f.team_id, "cprpout").await;
    let message = format!("cprp outside @{}", plain_username("cprpout"));
    let (status, served, body) = create(&client, RUST, &token, &f.channel_id, &message, "").await;
    assert_eq!(status, 201, "{body}");
    assert!(
        !served,
        "an out-of-channel mention is forwarded for its ephemeral notice"
    );
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE channelid = $1 AND message = $2")
            .bind(&f.channel_id)
            .bind(&message)
            .fetch_one(&pool)
            .await
            .expect("counts");
    assert_eq!(rows, 1, "exactly one row");
    common::delete_plain_user(&client, &token, &outsider.id).await;
}
