//! Cross-server parity for `POST /api/v4/posts?silent=true` — the `silent_notification` prop
//! and the fan-out it suppresses.
//!
//! ```sh
//! scripts/parity.sh --test parity post_create_silent
//! ```
//!
//! Only an integration author may ask for it — a bot here (`isIntegrationPostAuthor`) — and
//! then `SendNotifications` runs with `IsNotificationSuppressed()` true: no mention pass, so no
//! counters move, no `mentions` on the `posted` frame, and no out-of-channel notice for a
//! mention of a non-member. The prop itself is written by the server (a client's copy is stripped
//! by `SanitizeProps`) and reads back as `true`.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel_typed, create_plain_user,
    create_team, fixture_pool, go_minted_token, purge_api_fixtures, stack_enabled, username_of,
};

/// The stack's own bot (`scripts/stack.sh` seeds it; nothing sweeps it), as the token suite uses.
const BOT: &str = "seedbotdescribed0000000000";

struct Fixture {
    team_id: String,
    channel_id: String,
    /// In the channel.
    reader: common::PlainUser,
    reader_name: String,
    /// In the team, **not** in the channel.
    outsider_name: String,
    /// A personal access token for the bot.
    bot_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn add_to_team(client: &reqwest::Client, token: &str, team_id: &str, user_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": user_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "adding {user_id} to {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "cpsl").await;
            let channel_id = create_channel_typed(client, token, &team_id, "cpsl", "O").await;
            let reader = create_plain_user(client, token, &team_id, "cpsl").await;
            let outsider = create_plain_user(client, token, &team_id, "cpslo").await;
            add_user_to_channel(client, token, &channel_id, &reader.id).await;
            add_to_team(client, token, &team_id, BOT).await;
            add_user_to_channel(client, token, &channel_id, BOT).await;

            let response = client
                .post(format!("{GO}/api/v4/users/{BOT}/tokens"))
                .header("Authorization", format!("Bearer {token}"))
                .json(&serde_json::json!({ "description": "mmrs-parity silent" }))
                .send()
                .await
                .expect("Go answers");
            assert!(
                response.status().is_success(),
                "minting the bot's token failed"
            );
            let minted: serde_json::Value = response.json().await.expect("a token");
            Fixture {
                team_id,
                channel_id,
                reader_name: username_of(client, token, &reader.id).await,
                reader,
                outsider_name: username_of(client, token, &outsider.id).await,
                bot_token: minted["token"].as_str().expect("a token").to_owned(),
            }
        })
        .await
}

async fn create(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    message: &str,
    silent: bool,
) -> (u16, bool, Vec<u8>) {
    let query = if silent { "?silent=true" } else { "" };
    let response = client
        .post(format!("{base}/api/v4/posts{query}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "channel_id": channel_id, "message": message }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

fn normalised(post: &serde_json::Value) -> serde_json::Value {
    let mut post = post.clone();
    let obj = post.as_object_mut().expect("a post object");
    for key in ["id", "create_at", "update_at", "pending_post_id"] {
        obj.insert(key.to_owned(), serde_json::json!(0));
    }
    post
}

/// `(mention_count, mention_count_root)` of one member.
async fn mentions_of(channel_id: &str, user_id: &str) -> (i64, i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as(
        "SELECT mentioncount, mentioncountroot FROM channelmembers WHERE channelid = $1 AND userid = $2",
    )
    .bind(channel_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the member row")
}

async fn wait_for_join_mention(channel_id: &str, user_id: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mentions_of(channel_id, user_id).await.0 < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{user_id}'s join mention never landed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// The bot mentions the reader silently: served on both, the prop is `true`, the counters do
/// not move, and the reader's `posted` frame carries no `mentions`. The same post without
/// `?silent` moves the counter — the control that says the gate is the flag.
#[tokio::test]
async fn a_silent_bot_post_writes_the_prop_and_moves_no_counter() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let channel_id = create_channel_typed(&client, &token, &f.team_id, "cpsls", "O").await;
    add_user_to_channel(&client, &token, &channel_id, &f.reader.id).await;
    add_user_to_channel(&client, &token, &channel_id, BOT).await;
    wait_for_join_mention(&channel_id, &f.reader.id).await;
    let message = format!("cpsl hush @{}", f.reader_name);

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let before = mentions_of(&channel_id, &f.reader.id).await;
        let mut socket = SocketProbe::connect(base, &f.reader.token).await;
        let (status, served, body) =
            create(&client, base, &f.bot_token, &channel_id, &message, true).await;
        let body = json(&body);
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(body["props"]["silent_notification"], true, "{base}: {body}");
        assert_eq!(body["props"]["from_bot"], "true", "{base}");
        let post_id = body["id"].as_str().expect("an id").to_owned();
        bodies.push(normalised(&body));

        let posted_for = |frames: &[serde_json::Value]| {
            frames.iter().any(|f| {
                f["event"] == "posted"
                    && f["data"]["post"]
                        .as_str()
                        .is_some_and(|p| p.contains(&post_id))
            })
        };
        assert!(
            socket
                .collect_until(Duration::from_millis(2500), posted_for)
                .await,
            "{base}: the reader still gets the posted frame: {:?}",
            socket.raw
        );
        let frame = socket
            .events_named("posted")
            .into_iter()
            .find(|f| {
                f["data"]["post"]
                    .as_str()
                    .is_some_and(|p| p.contains(&post_id))
            })
            .expect("the frame");
        assert!(
            frame["data"].get("mentions").is_none(),
            "{base}: no add_mentions hook on a silent post: {frame}"
        );
        // Settled by the time the frame arrived; one more look to be sure nothing moves late.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            mentions_of(&channel_id, &f.reader.id).await,
            before,
            "{base}: a silent mention moves no counter"
        );

        // The control: the same message, not silent, moves the counter by one.
        let (status, served, loud) =
            create(&client, base, &f.bot_token, &channel_id, &message, false).await;
        let loud = json(&loud);
        assert_eq!(status, 201, "{base}: {loud}");
        assert_eq!(served, base == RUST, "{base}");
        assert!(
            loud["props"].get("silent_notification").is_none(),
            "{base}: {loud}"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let after = mentions_of(&channel_id, &f.reader.id).await;
            if after == (before.0 + 1, before.1 + 1) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{base}: the loud control moved {before:?} -> {after:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// A silent mention of a team member who is **not** in the channel is served (no out-of-channel
/// notice is owed under suppression) and leaves no ephemeral behind on either server.
#[tokio::test]
async fn a_silent_out_of_channel_mention_is_served_without_a_notice() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let message = format!("cpsl hush @{}", f.outsider_name);

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        // The bot's own socket is where the ephemeral notice would arrive.
        let mut socket = SocketProbe::connect(base, &f.bot_token).await;
        let (status, served, body) =
            create(&client, base, &f.bot_token, &f.channel_id, &message, true).await;
        let body = json(&body);
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(
            served,
            base == RUST,
            "{base}: served, not forwarded for the notice"
        );
        assert_eq!(body["props"]["silent_notification"], true, "{base}");
        bodies.push(normalised(&body));

        let ephemeral =
            |frames: &[serde_json::Value]| frames.iter().any(|f| f["event"] == "ephemeral_message");
        assert!(
            !socket
                .collect_until(Duration::from_millis(1500), ephemeral)
                .await,
            "{base}: no out-of-channel notice under suppression: {:?}",
            socket.raw
        );
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// A plain user asking for silence is the 403 on both, before anything is written.
#[tokio::test]
async fn a_non_integration_author_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) =
            create(&client, base, &token, &f.channel_id, "cpsl not a bot", true).await;
        assert_eq!(status, 403, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            json(&body)["id"],
            "api.post.create_post.silent_notification.app_error",
            "{base}"
        );
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "silent by a user");
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM posts WHERE channelid = $1 AND message = 'cpsl not a bot'",
    )
    .bind(&f.channel_id)
    .fetch_one(&pool)
    .await
    .expect("counts");
    assert_eq!(rows, 0, "refused before the row");
}

/// The prop reads back as the boolean `true` on both — the shape `SendNotifications` gates on.
#[tokio::test]
async fn the_prop_reads_back_as_a_boolean() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut reads = Vec::new();
    for base in [GO, RUST] {
        let (status, _, body) = create(
            &client,
            base,
            &f.bot_token,
            &f.channel_id,
            "cpsl quiet",
            true,
        )
        .await;
        let body = json(&body);
        assert_eq!(status, 201, "{base}: {body}");
        let post_id = body["id"].as_str().expect("an id");
        for reader in [GO, RUST] {
            let response = client
                .get(format!("{reader}/api/v4/posts/{post_id}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{reader} is unreachable: {e}"));
            assert_eq!(response.status().as_u16(), 200, "{reader}");
            let read: serde_json::Value = response.json().await.expect("a post");
            assert_eq!(
                read["props"]["silent_notification"], true,
                "{reader}: {read}"
            );
            reads.push(normalised(&read));
        }
    }
    assert!(reads.iter().all(|r| r == &reads[0]), "{reads:#?}");
}
