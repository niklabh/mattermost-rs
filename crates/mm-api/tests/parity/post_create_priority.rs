//! Cross-server parity for `POST /api/v4/posts` carrying `metadata.priority`.
//!
//! ```sh
//! scripts/parity.sh --test parity post_create_priority
//! ```
//!
//! The priority is written to `PostsPriority` inside the post's transaction and **echoed** back
//! from the request rather than read from the row — so the create body has no `PostId` key and a
//! `null` for every boolean the client left out, while the read a moment later has both. An
//! urgent mention moves `UrgentMentionCount`. A persistent notification is still forwarded, and
//! without a licence is refused before either server would write anything.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, fixture_pool, go_minted_token,
    purge_api_fixtures, stack_enabled, username_of,
};

struct Fixture {
    team_id: String,
    channel_id: String,
    reader: common::PlainUser,
    reader_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "cppr").await;
            let channel_id = create_channel_typed(client, token, &team_id, "cppr", "O").await;
            let reader = create_plain_user(client, token, &team_id, "cppr").await;
            add_user_to_channel(client, token, &channel_id, &reader.id).await;
            let reader_name = username_of(client, token, &reader.id).await;
            Fixture {
                team_id,
                channel_id,
                reader,
                reader_name,
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
    root_id: &str,
    priority: serde_json::Value,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": message,
            "root_id": root_id,
            "metadata": { "priority": priority },
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
    let body = response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    (status, served, body)
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

async fn get_post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> (u16, serde_json::Value) {
    let response = client
        .get(format!("{base}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    (
        response.status().as_u16(),
        response.json().await.unwrap_or(serde_json::Value::Null),
    )
}

/// A post with its ids and clocks replaced, `PostId` inside the priority included.
fn normalised(post: &serde_json::Value) -> serde_json::Value {
    let mut post = post.clone();
    let obj = post.as_object_mut().expect("a post object");
    for key in ["id", "create_at", "update_at", "pending_post_id"] {
        obj.insert(key.to_owned(), serde_json::json!(0));
    }
    if let Some(priority) = obj
        .get_mut("metadata")
        .and_then(|m| m.get_mut("priority"))
        .and_then(|p| p.as_object_mut())
    {
        if priority.contains_key("PostId") {
            priority.insert("PostId".to_owned(), serde_json::json!(0));
        }
    }
    post
}

/// `(priority, requestedack, persistentnotifications)` of the row.
async fn priority_row(post_id: &str) -> Option<(String, Option<bool>, Option<bool>)> {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as(
        "SELECT priority, requestedack, persistentnotifications FROM postspriority WHERE postid = $1",
    )
    .bind(post_id)
    .fetch_optional(&pool)
    .await
    .expect("the priority query")
}

async fn rows_with_message(channel_id: &str, message: &str) -> i64 {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE channelid = $1 AND message = $2")
        .bind(channel_id)
        .bind(message)
        .fetch_one(&pool)
        .await
        .expect("counts")
}

/// `(mention_count, urgent_mention_count)` of one member.
async fn counts_of(channel_id: &str, user_id: &str) -> (i64, i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as(
        "SELECT mentioncount, urgentmentioncount FROM channelmembers WHERE channelid = $1 AND userid = $2",
    )
    .bind(channel_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the member row")
}

/// `{"priority":"important"}` and nothing else: the row holds NULL for both booleans, the
/// create body echoes the request (`null`, no `PostId`), and the read has the row's shape.
#[tokio::test]
async fn an_important_priority_is_written_and_echoed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    let mut reads = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cppr important",
            "",
            serde_json::json!({ "priority": "important" }),
        )
        .await;
        let body = json(&body);
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(
            body["metadata"]["priority"],
            serde_json::json!({
                "priority": "important",
                "requested_ack": null,
                "persistent_notifications": null
            }),
            "{base}: the request echoed, not the row"
        );
        assert_eq!(
            priority_row(post_id).await,
            Some(("important".to_owned(), None, None)),
            "{base}"
        );
        bodies.push(normalised(&body));

        for reader in [GO, RUST] {
            let (status, read) = get_post(&client, reader, &token, post_id).await;
            assert_eq!(status, 200, "{reader} reading {base}'s post: {read}");
            assert_eq!(read["metadata"]["priority"]["PostId"], post_id, "{reader}");
            assert_eq!(
                read["metadata"]["priority"]["requested_ack"],
                serde_json::Value::Null,
                "{reader}: NULL reads as null"
            );
            reads.push(normalised(&read));
        }
    }
    assert_eq!(bodies[0], bodies[1]);
    assert!(
        reads.iter().all(|read| read == &reads[0]),
        "every read agrees: {reads:#?}"
    );
}

/// An urgent post that mentions the reader moves both their counters, and the booleans land
/// as sent. `requested_ack: true` is a licensed feature (the handler's 501 without one), so the
/// unlicensed pair sends `false` and the licensed pair sends `true` — without the mention,
/// because an `@` on a licensed server still forwards (`FillInPostProps`'s LDAP-groups arm).
#[tokio::test]
async fn an_urgent_mention_moves_the_urgent_counter() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    // A channel of this test's own, so no other test's mention moves the counter.
    let channel_id = create_channel_typed(&client, &token, &f.team_id, "cppru", "O").await;
    add_user_to_channel(&client, &token, &channel_id, &f.reader.id).await;
    // Go posts the "added to the channel" system message from a goroutine, and it mentions the
    // added user — so the counter moves once more some milliseconds after the add returned.
    // Wait for that before reading the baseline.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while counts_of(&channel_id, &f.reader.id).await.0 < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reader's join mention never landed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let message = format!("cppr urgent @{}", f.reader_name);

    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    let cases = [
        (GO, false, RUST, true),
        (RUST, false, RUST, true),
        (pair.go.as_str(), true, pair.rust.as_str(), false),
        (pair.rust.as_str(), true, pair.rust.as_str(), false),
    ];
    for (base, requested_ack, rust_base, mention) in cases {
        let before = counts_of(&channel_id, &f.reader.id).await;
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &channel_id,
            if mention {
                &message
            } else {
                "cppr urgent quiet"
            },
            "",
            serde_json::json!({
                "priority": "urgent",
                "requested_ack": requested_ack,
                "persistent_notifications": false
            }),
        )
        .await;
        let body = json(&body);
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == rust_base, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(
            priority_row(post_id).await,
            Some(("urgent".to_owned(), Some(requested_ack), Some(false))),
            "{base}"
        );
        // The join mention lands from a goroutine; poll for the settled delta.
        let delta = i64::from(mention);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let after = counts_of(&channel_id, &f.reader.id).await;
            if after == (before.0 + delta, before.1 + delta) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{base}: counters {before:?} -> {after:?}, expected +{delta}/+{delta}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// The level is stored as sent — nothing validates it.
#[tokio::test]
async fn an_unknown_level_is_stored_as_sent() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cppr whatever",
            "",
            serde_json::json!({ "priority": "whatever", "requested_ack": false }),
        )
        .await;
        let body = json(&body);
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(
            priority_row(post_id).await,
            Some(("whatever".to_owned(), Some(false), None)),
            "{base}"
        );
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// A priority document with no level: the handler lets it through (`requested_ack: false`
/// needs no licence), and the NOT NULL column fails the save — transaction and all, so no post
/// row is left behind — with the generic 500.
#[tokio::test]
async fn a_priority_without_a_level_fails_the_save_and_leaves_no_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let message = "cppr no level";

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            message,
            "",
            serde_json::json!({ "requested_ack": false }),
        )
        .await;
        assert_eq!(status, 500, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(json(&body)["id"], "app.post.save.app_error", "{base}");
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "a priority with no level");
    assert_eq!(
        rows_with_message(&f.channel_id, message).await,
        0,
        "the transaction rolled the post back on both"
    );
}

/// A priority on a reply is the handler's 400, before anything is written.
#[tokio::test]
async fn a_priority_on_a_reply_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (status, _, root) = create(
        &client,
        GO,
        &token,
        &f.channel_id,
        "cppr root",
        "",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&root));
    let root_id = json(&root)["id"].as_str().expect("an id").to_owned();

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cppr reply",
            &root_id,
            serde_json::json!({ "priority": "important" }),
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "a priority on a reply");
}

/// Without a licence a persistent notification is the handler's 501 on both; on the licensed
/// pair it is forwarded whole, because the notification row and its job have no port.
#[tokio::test]
async fn a_persistent_notification_is_refused_unlicensed_and_forwarded_licensed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let message = format!("cppr persistent @{}", f.reader_name);
    let priority = serde_json::json!({
        "priority": "urgent",
        "requested_ack": false,
        "persistent_notifications": true
    });

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            &message,
            "",
            priority.clone(),
        )
        .await;
        assert_eq!(status, 501, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "persistent, unlicensed");

    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    let (status, served, body) = create(
        &client,
        &pair.rust,
        &token,
        &f.channel_id,
        &message,
        "",
        priority,
    )
    .await;
    let body = json(&body);
    assert_eq!(status, 201, "licensed: {body}");
    assert!(!served, "licensed: forwarded to Go: {body}");
    let post_id = body["id"].as_str().expect("an id");
    assert_eq!(
        priority_row(post_id).await,
        Some(("urgent".to_owned(), Some(false), Some(true))),
        "Go wrote the row"
    );
}

/// Metadata beyond a priority — an `expire_at` here — is forwarded: Go echoes it and this port
/// has not measured that.
#[tokio::test]
async fn metadata_beyond_a_priority_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let response = client
        .post(format!("{RUST}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": f.channel_id,
            "message": "cppr expire",
            "metadata": { "priority": { "priority": "important" }, "expire_at": 5 },
        }))
        .send()
        .await
        .expect("Rust answers");
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go")
    );
}
