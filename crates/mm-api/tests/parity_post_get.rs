//! Cross-server parity for `GET /api/v4/posts/{post_id}`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity_post_get
//! ```
//!
//! # This route can decline, and that is under test too
//!
//! `mm_app::post` reproduces the metadata pipeline only for the shapes whose output it can
//! predict, and forwards the rest (see that module for why). So the suite has to assert **both
//! halves**: that a served response is byte-identical to Go's, *and* that a shape the port
//! declines really was forwarded — `x-mmrs-served-by: go` — rather than answered with a body
//! that merely looks plausible. [`assert_forwarded_and_identical`] is the second half; without
//! it, widening the port's appetite by mistake would look like a passing suite.
//!
//! # Two fixtures cannot be created through the API
//!
//! `PostsPriority` and `PostAcknowledgements` rows are written by a create/ack path that is
//! licence-gated (`license_error.feature_unavailable` on this Team Edition build), while the
//! *read* path is not — `IsPostPriorityEnabled` consults `ServiceSettings.PostPriority` and no
//! licence at all. So the two rows are inserted straight into the shared database, and both
//! servers then read them through their own store. That is the only way to exercise
//! `metadata.priority` and `metadata.acknowledgements` here at all; leaving them untested would
//! mean shipping two store queries with no oracle behind them.

mod common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_plain_user, delete_plain_user, fetch_both_raw, fetch_both_stable,
    go_minted_token, logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
};

/// A 1x1 PNG, small enough to inline and real enough for Go's image decoder — which both the
/// emoji endpoint and the file uploader run before accepting the bytes.
const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0, 3, 1,
    1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// The two custom emoji the rich-post case needs, named **uniquely per run**.
///
/// A fixed name does not work, and the reason is a cache rather than a leak: `LocalCacheEmojiStore`
/// memoises `GetByName` for thirty minutes, and `purge_post_fixtures` deletes the row straight
/// from Postgres — which the Go server never hears about. The next run's `POST /emoji` then finds
/// the stale cache entry and answers `api.emoji.create.duplicate.app_error` against a row that no
/// longer exists. Measured, not theorised.
///
/// Deleting through Go's API instead would invalidate the cache, but only on a run that reaches
/// its teardown; an assertion panics past it, and then every later run is poisoned. A fresh name
/// each run sidesteps both, and the prefix keeps the purge able to collect the rows.
///
/// The second name is soft-deleted before use: `emojiSelectQuery` carries `DeleteAt = 0` in Go's
/// **shared** select builder rather than in `GetMultipleByName`'s own body, so a port that read
/// only that body would resurrect it.
static EMOJI_NAMES: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

fn emoji_names() -> &'static (String, String) {
    EMOJI_NAMES.get_or_init(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        (
            format!("mmrsparitypostlive{stamp}"),
            format!("mmrsparitypostgone{stamp}"),
        )
    })
}

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

/// A one-connection pool for the fixtures that have to write rows the REST API cannot produce.
///
/// `None` when `DATABASE_URL` is unset, so those tests skip rather than fail on a machine with
/// no stack. The timeout is capped because sqlx's default is 30 seconds and six tests once sat
/// on it — see CLAUDE.md on keeping the suite fast.
async fn fixture_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

/// Rows the api-level purge in `common` does not reach: it clears `posts` and `fileinfo` for a
/// `mmrs-parity-%` channel, but nothing keyed on a post id, and nothing in `Emoji`.
///
/// Runs **before** the fixtures are created, for the reason `common::purge_api_fixtures`
/// documents: an assertion panics past any trailing cleanup, so the only teardown that is
/// certain to run is the next run's.
async fn purge_post_fixtures() {
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

    for statement in [
        "DELETE FROM reactions WHERE emojiname LIKE 'mmrsparity%'",
        "DELETE FROM reactions WHERE postid IN (SELECT id FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%'))",
        "DELETE FROM postspriority WHERE postid IN (SELECT id FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%'))",
        "DELETE FROM postacknowledgements WHERE postid IN (SELECT id FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%'))",
        // Go's DELETE on an emoji is a soft delete and the name stays taken, so the row has to go.
        "DELETE FROM emoji WHERE name LIKE 'mmrsparity%'",
    ] {
        let _ = sqlx::query(statement).execute(&pool).await;
    }
}

/// One team and one channel per test **binary**.
///
/// The tests are read-only against posts they each create, so they do not need a channel apiece —
/// and a team apiece would mean six teams, six `town-square`/`off-topic` pairs and six sets of
/// sidebar categories per run, all of which [D-155] says the purge cannot fully unwind.
static FIXTURE: tokio::sync::OnceCell<(String, String)> = tokio::sync::OnceCell::const_new();

async fn team_and_channel(client: &reqwest::Client, token: &str) -> (String, String) {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            purge_post_fixtures().await;
            let team_id = create_team(client, token, "posts").await;
            let channel_id = create_channel(client, token, &team_id, "posts").await;
            (team_id, channel_id)
        })
        .await
        .clone()
}

async fn create_team(client: &reqwest::Client, token: &str, tag: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Post a message carrying file attachments, which `post_message` cannot express.
async fn post_with_files(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    message: &str,
    file_ids: &[String],
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": message,
            "file_ids": file_ids,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "posting with files failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the post decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Go's "simple" upload mode: `channel_id` and `filename` in the query, the bytes as the body.
/// It avoids a multipart encoder for the one endpoint that offers an alternative.
async fn upload_file(client: &reqwest::Client, token: &str, channel_id: &str) -> String {
    let response = client
        .post(format!(
            "{GO}/api/v4/files?channel_id={channel_id}&filename=mmrs-parity.png"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "image/png")
        .body(TINY_PNG)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "uploading the fixture file failed: {}",
        response.text().await.unwrap_or_default()
    );
    let uploaded: serde_json::Value = response.json().await.expect("the upload decodes");
    uploaded["file_infos"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

/// `POST /api/v4/emoji` takes multipart and nothing else, so the body is assembled by hand
/// rather than by pulling reqwest's `multipart` feature — and a Cargo feature change — into the
/// tree for one call.
async fn create_custom_emoji(
    client: &reqwest::Client,
    token: &str,
    creator_id: &str,
    name: &str,
) -> String {
    const BOUNDARY: &str = "mmrsparitypostboundary";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"e.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(TINY_PNG);
    body.extend_from_slice(
        format!(
            "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"emoji\"\r\n\r\n\
             {{\"name\":\"{name}\",\"creator_id\":\"{creator_id}\"}}\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );

    let response = client
        .post(format!("{GO}/api/v4/emoji"))
        .header("Authorization", format!("Bearer {token}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture emoji failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the emoji decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Go's emoji delete is a **soft** delete — it sets `DeleteAt` and leaves the row and its name.
async fn delete_custom_emoji(client: &reqwest::Client, token: &str, emoji_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/emoji/{emoji_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deleting the fixture emoji failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn add_reaction(
    client: &reqwest::Client,
    token: &str,
    post_id: &str,
    user_id: &str,
    emoji: &str,
) {
    let response = client
        .post(format!("{GO}/api/v4/reactions"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "user_id": user_id,
            "post_id": post_id,
            "emoji_name": emoji,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "reacting failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Write the two licence-gated rows straight to the shared database. See the module docs.
/// Returns `false` when `DATABASE_URL` is unset so the caller can skip rather than fail.
async fn plant_priority_and_acknowledgement(
    post_id: &str,
    channel_id: &str,
    user_id: &str,
) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return false;
    };

    // `persistentnotifications` is deliberately **false** while `requestedack` is **true**: two
    // booleans holding the same value cannot catch a port that reads the wrong column.
    sqlx::query(
        "INSERT INTO postspriority (postid, channelid, priority, requestedack, persistentnotifications) \
         VALUES ($1, $2, 'urgent', true, false) ON CONFLICT (postid) DO NOTHING",
    )
    .bind(post_id)
    .bind(channel_id)
    .execute(&pool)
    .await
    .expect("the priority row is planted");

    // A distinctive, non-`CreateAt` timestamp, so reading the wrong column is visible.
    sqlx::query(
        "INSERT INTO postacknowledgements (postid, userid, acknowledgedat, remoteid, channelid) \
         VALUES ($1, $2, 1700000000123, '', $3) ON CONFLICT (postid, userid) DO NOTHING",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(channel_id)
    .execute(&pool)
    .await
    .expect("the acknowledgement row is planted");

    true
}

/// Null the two columns Go's reaction query `COALESCE`s, on one reaction row.
///
/// Nothing reachable through the REST API writes a NULL there — reactions are inserted with both
/// timestamps set and soft-deleted by writing a value — so this is the only way to exercise the
/// coalesces at all. Both are observable: a NULL `UpdateAt` must come back as `CreateAt` rather
/// than `0`, and a NULL `DeleteAt` must still satisfy the "not deleted" predicate. Returns the
/// number of rows touched so the caller can assert the fixture actually landed.
async fn null_out_reaction_timestamps(post_id: &str, emoji_name: &str) -> u64 {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return 0;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return 0;
    };
    sqlx::query(
        "UPDATE reactions SET updateat = NULL, deleteat = NULL WHERE postid = $1 AND emojiname = $2",
    )
    .bind(post_id)
    .bind(emoji_name)
    .execute(&pool)
    .await
    .map(|done| done.rows_affected())
    .unwrap_or(0)
}

/// Overwrite a post's `Props` in the shared database.
///
/// Used for the `attachments` prop, which drives `getEmbedForPost`'s very first branch — the one
/// that returns a `message_attachment` embed before any link logic runs. Written directly rather
/// than through `POST /posts` so the fixture states exactly what is on the row, with no
/// dependence on which props Go's create path chooses to sanitise away.
async fn plant_props(post_id: &str, props: serde_json::Value) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return false;
    };
    sqlx::query("UPDATE posts SET props = $1 WHERE id = $2")
        .bind(props)
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("the props are planted");
    true
}

/// Overwrite a post's `FileIds` column in the shared database.
///
/// **`Post.PreSave` sorts them** — `o.FileIds = RemoveDuplicateStrings(o.FileIds)` (post.go:740),
/// and that helper sorts before deduplicating — so a post created through the API always stores
/// its attachments in *alphabetical id* order, whatever order the client sent. Ids are random,
/// so whether that order differs from the store's `CreateAt DESC` is a coin flip, and on the run
/// that first exposed this it happened to coincide, making `orderFileInfosByID` a no-op the
/// fixture could not see. Planting the column is how the two orders are made to differ on
/// purpose.
async fn plant_file_ids(post_id: &str, file_ids: &[String]) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    let encoded = serde_json::to_string(file_ids).expect("ids serialise");
    sqlx::query("UPDATE posts SET fileids = $1 WHERE id = $2")
        .bind(encoded)
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("the file ids are planted");
    true
}

/// Undo the file-info soft delete that `DeletePost` performs **in a goroutine**
/// (`a.Srv().Go(func() { a.deletePostFiles(...) })`, app/post.go:2013 → `DeleteForPost`).
///
/// A soft-deleted post whose file infos are still live is a state Go itself passes through on
/// every delete — it is just transient, and racing it from a test would be worse than useless.
/// So this waits for the goroutine to finish and then puts the rows back, which is the only way
/// to hold the state still. It is what makes the **second** `preparePostFilesForClient` call
/// observable: the deleted-post short circuit blanks the metadata between the two passes, and
/// only the second one puts `files` back.
///
/// Returns false if the goroutine never ran, so the caller can skip rather than assert on a
/// fixture that did not land.
async fn revive_file_infos_after_post_delete(post_id: &str) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    let mut deleted = false;
    for _ in 0..40 {
        let live: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM fileinfo WHERE postid = $1 AND deleteat = 0")
                .bind(post_id)
                .fetch_one(&pool)
                .await
                .unwrap_or(1);
        if live == 0 {
            deleted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    if !deleted {
        return false;
    }
    sqlx::query("UPDATE fileinfo SET deleteat = 0 WHERE postid = $1")
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("the file infos are revived");
    true
}

async fn delete_post(client: &reqwest::Client, token: &str, post_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deleting the fixture post failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Assert the Rust server **forwarded** this path, and that what came back still matches Go.
///
/// The second half is not redundant: a forward that mangles the body is as much a bug as a
/// handler that does, and the `x-mmrs-served-by` header alone would not notice.
async fn assert_forwarded_and_identical(client: &reqwest::Client, token: &str, path: &str) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let status = response.status().as_u16();
        (
            status,
            served_by,
            response.bytes().await.expect("body").to_vec(),
        )
    };

    let (go_status, _, go_body) = get(GO).await;
    let (rs_status, served_by, rs_body) = get(RUST).await;

    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "{path}: this shape must be forwarded — the metadata pipeline cannot reproduce it"
    );
    assert_eq!(go_status, rs_status, "{path}: status");
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body),
        "{path}: forwarded body"
    );
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The shape every client sees most: a plain root post, no metadata at all.
///
/// Pins the six fields the store never selects (`pending_post_id`, `last_reply_at`,
/// `participants`, and the three that only `PreparePostForClient` can fill), the empty
/// `"metadata":{}` object, and the trailing newline `EncodeJSON` writes.
#[tokio::test]
async fn a_plain_post_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = team_and_channel(&client, &token).await;

    let post_id = post_message(&client, &token, &channel, "plain parity message", None).await;
    let path = format!("/api/v4/posts/{post_id}");
    let (go, rs) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "a plain post must match byte for byte"
    );

    let body: serde_json::Value = serde_json::from_slice(&go).expect("Go's body is JSON");
    assert_eq!(
        body["metadata"],
        serde_json::json!({}),
        "the oracle is empty metadata, not an absent key"
    );
    assert_eq!(body["participants"], serde_json::Value::Null);
    assert_eq!(body["pending_post_id"], "");
    assert!(
        go.ends_with(b"\n"),
        "json.Encoder writes a trailing newline; json.Marshal does not"
    );
}

/// `ReplyCount` is a **thread** count, not a child count — the root reports its replies and a
/// reply reports the same number, itself included.
///
/// Three replies with one deleted, so three separate mistakes give three different answers:
/// counting only a post's own children gives `0` for the reply, counting the whole thread
/// including the root gives `3`, and forgetting the subquery's `DeleteAt = 0` gives `3` as well.
/// The right answer is `2`.
#[tokio::test]
async fn reply_count_matches_go_for_a_root_and_a_reply() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = team_and_channel(&client, &token).await;

    let root = post_message(&client, &token, &channel, "thread root", None).await;
    let reply = post_message(&client, &token, &channel, "reply one", Some(&root)).await;
    post_message(&client, &token, &channel, "reply two", Some(&root)).await;
    let doomed = post_message(&client, &token, &channel, "reply three", Some(&root)).await;
    delete_post(&client, &token, &doomed).await;

    for id in [&root, &reply] {
        let path = format!("/api/v4/posts/{id}");
        let (go, rs) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
        let body: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
        assert_eq!(
            body["reply_count"], 2,
            "{path}: two live replies, three rows — both ends of the thread agree"
        );
    }
}

/// Everything the metadata pipeline can actually fill on this deployment, in one post.
///
/// The fixture is deliberately plural and deliberately awkward, because every singular or tidy
/// version of it hides a mistake:
///
/// - **Two files**, uploaded in one order and attached in the other, so `orderFileInfosByID`
///   has to actually reorder the store's `CreateAt DESC` result.
/// - **Two reactions**, so the `ORDER BY CreateAt` on the reaction query is observable, and one
///   of them has its `UpdateAt`/`DeleteAt` nulled so both `COALESCE`s are exercised.
/// - **Two custom emoji named in the message**, one of them soft-deleted, so the `DeleteAt = 0`
///   that lives in Go's *shared* select builder is observable.
/// - **A system emoji**, whose whole job is to be absent from `metadata.emojis`: the filter that
///   drops it runs before the store query, so a port that skipped it would query — and return —
///   a row Go never has.
#[tokio::test]
async fn a_post_with_every_reachable_metadata_field_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = team_and_channel(&client, &token).await;
    let user_id = logged_in_user_id().to_owned();

    let (live_emoji, deleted_emoji) = emoji_names();
    create_custom_emoji(&client, &token, &user_id, live_emoji).await;
    let gone = create_custom_emoji(&client, &token, &user_id, deleted_emoji).await;
    delete_custom_emoji(&client, &token, &gone).await;

    let first_file = upload_file(&client, &token, &channel).await;
    // Both fixtures below need their two rows to have **distinct** CreateAt values, or the
    // ordering they exist to pin is a tie that Postgres may break either way — which would make
    // the byte comparison itself flaky, not merely the mutation weaker. Both are asserted
    // distinct against Go's own body further down; the pause is what makes that assertion pass.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second_file = upload_file(&client, &token, &channel).await;
    let file_ids = vec![first_file.clone(), second_file.clone()];

    let post_id = post_with_files(
        &client,
        &token,
        &channel,
        &format!("rich :{live_emoji}: :{deleted_emoji}: and :smile:"),
        &file_ids,
    )
    .await;
    // Oldest-first, which is the **reverse** of the store's `CreateAt DESC`, so
    // `orderFileInfosByID` has to actually reorder. Planted rather than sent, because `PreSave`
    // sorts whatever the client supplies — see `plant_file_ids`.
    if !plant_file_ids(&post_id, &file_ids).await {
        return;
    }

    add_reaction(&client, &token, &post_id, &user_id, live_emoji).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    add_reaction(&client, &token, &post_id, &user_id, "+1").await;
    assert_eq!(
        null_out_reaction_timestamps(&post_id, "+1").await,
        1,
        "the NULL-timestamp fixture must land, or both COALESCEs go untested"
    );

    if !plant_priority_and_acknowledgement(&post_id, &channel, &user_id).await {
        return;
    }

    let path = format!("/api/v4/posts/{post_id}");
    let (go, rs) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the rich post must match byte for byte"
    );

    // Then say what the oracle actually contained, so a fixture that silently stopped producing
    // one of these fields cannot leave the comparison passing on nothing.
    let body: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let metadata = &body["metadata"];

    assert_eq!(
        metadata["emojis"].as_array().map(Vec::len),
        Some(1),
        "the soft-deleted emoji and the system one are both absent: {}",
        metadata["emojis"]
    );
    assert_eq!(metadata["emojis"][0]["name"], live_emoji.as_str());

    let files = metadata["files"].as_array().expect("two files");
    assert_eq!(
        files.iter().map(|f| f["id"].as_str()).collect::<Vec<_>>(),
        vec![Some(first_file.as_str()), Some(second_file.as_str())],
        "files follow the post's file_ids, not the store's CreateAt DESC"
    );
    assert_eq!(
        body["file_ids"],
        serde_json::json!([first_file, second_file]),
        "and the planted order is what reached both servers"
    );
    assert!(
        files[0]["mini_preview"].is_string(),
        "the stored mini preview reaches the wire; a null here means Go regenerated it"
    );
    assert_ne!(
        files[0]["create_at"], files[1]["create_at"],
        "tied CreateAt would make the store's ORDER BY a coin flip, and the reordering invisible"
    );

    let reactions = metadata["reactions"].as_array().expect("two reactions");
    assert_eq!(reactions.len(), 2);
    assert_eq!(
        reactions[0]["emoji_name"],
        live_emoji.as_str(),
        "reactions come back in CreateAt order"
    );
    assert_eq!(reactions[1]["emoji_name"], "+1");
    assert_ne!(
        reactions[0]["create_at"], reactions[1]["create_at"],
        "tied CreateAt would make ORDER BY CreateAt a coin flip on both servers"
    );
    assert_eq!(
        reactions[1]["update_at"], reactions[1]["create_at"],
        "a NULL UpdateAt coalesces to CreateAt, not to 0"
    );
    assert_eq!(
        reactions[1]["delete_at"], 0,
        "a NULL DeleteAt coalesces to 0 and still satisfies the not-deleted predicate"
    );

    assert_eq!(metadata["priority"]["priority"], "urgent");
    assert_eq!(metadata["priority"]["requested_ack"], true);
    assert_eq!(metadata["priority"]["persistent_notifications"], false);
    assert_eq!(
        metadata["priority"]["PostId"], post_id,
        "the two capitalised keys are Go's empty json tag falling back to the field name"
    );
    assert_eq!(
        metadata["acknowledgements"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        metadata["acknowledgements"][0]["acknowledged_at"],
        1_700_000_000_123i64
    );
    assert_eq!(body["has_reactions"], true);
}

/// A **reply** never carries priority or acknowledgements, however its own rows read: Go gates
/// the whole block on `post.RootId == ""`. Planting the rows on a reply and expecting them to
/// stay off the wire is the only way to catch a port that dropped that condition.
#[tokio::test]
async fn a_reply_never_reports_priority_even_with_a_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = team_and_channel(&client, &token).await;
    let user_id = logged_in_user_id().to_owned();

    let root = post_message(&client, &token, &channel, "priority thread root", None).await;
    let reply = post_message(&client, &token, &channel, "priority reply", Some(&root)).await;
    if !plant_priority_and_acknowledgement(&reply, &channel, &user_id).await {
        return;
    }

    let path = format!("/api/v4/posts/{reply}");
    let (go, rs) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    let body: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert!(
        body["metadata"].get("priority").is_none(),
        "a reply's priority row is invisible: {}",
        body["metadata"]
    );
    assert!(body["metadata"].get("acknowledgements").is_none());
}

/// The etag is `<version>.<id>.<update_at>`, raw — no quotes, no `W/` — on the 200 **and** on
/// the 304, and `If-None-Match` is compared byte for byte.
#[tokio::test]
async fn the_etag_matches_and_answers_304() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = team_and_channel(&client, &token).await;

    let post_id = post_message(&client, &token, &channel, "etag parity", None).await;
    let path = format!("/api/v4/posts/{post_id}");

    let fetch = async |base: &str, if_none_match: Option<&str>| {
        let mut request = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(value) = if_none_match {
            request = request.header("If-None-Match", value);
        }
        let response = request.send().await.expect("reachable");
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (
            status,
            etag,
            served_by,
            response.bytes().await.expect("body").to_vec(),
        )
    };

    let (go_status, go_etag, _, _) = fetch(GO, None).await;
    let (rs_status, rs_etag, served_by, _) = fetch(RUST, None).await;
    assert_eq!(served_by.as_deref(), Some("rust"), "{path} was forwarded");
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(go_etag, rs_etag, "the etag itself must match");
    let etag = go_etag.expect("Go sets an ETag");
    assert!(
        etag.starts_with(&format!(
            "{}.{post_id}.",
            mm_model::version::CURRENT_VERSION
        )),
        "etag is <version>.<id>.<update_at>, got {etag}"
    );

    let (go_status, go_etag, _, go_body) = fetch(GO, Some(&etag)).await;
    let (rs_status, rs_etag, served_by, rs_body) = fetch(RUST, Some(&etag)).await;
    assert_eq!(served_by.as_deref(), Some("rust"), "the 304 was forwarded");
    assert_eq!((go_status, rs_status), (304, 304));
    assert_eq!(go_etag, rs_etag, "the 304 carries the etag back");
    assert!(
        go_body.is_empty() && rs_body.is_empty(),
        "a 304 has no body"
    );

    // A near-miss must not 304. Quoting it is the mistake an HTTP-shaped implementation makes,
    // and Go's comparison is a plain string equality that rejects it.
    let (go_status, ..) = fetch(GO, Some(&format!("\"{etag}\""))).await;
    let (rs_status, _, served_by, _) = fetch(RUST, Some(&format!("\"{etag}\""))).await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(
        (go_status, rs_status),
        (200, 200),
        "a quoted etag is not a match"
    );
}

/// A soft-deleted post: 404 without the flag, and with it an **empty message** and blank
/// metadata, plus the `deleteBy` prop Go writes on the way out. The message is still in the
/// database — it is `PreparePostForClient`'s short circuit that blanks it, which is why this
/// is a wire test and not a store one.
#[tokio::test]
async fn include_deleted_matches_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (team, channel) = team_and_channel(&client, &token).await;

    // The post carries a **file**, which is what makes the second `preparePostFilesForClient`
    // call observable: the deleted-post short circuit blanks the metadata, and only that second
    // call — the one that looks redundant — puts `files` back.
    let file_id = upload_file(&client, &token, &channel).await;
    let post_id = post_with_files(
        &client,
        &token,
        &channel,
        "soon to be deleted",
        std::slice::from_ref(&file_id),
    )
    .await;
    delete_post(&client, &token, &post_id).await;
    // `DeletePost` soft-deletes the file infos in a goroutine, which would otherwise leave the
    // deleted post's metadata empty and the second files pass untested. See the helper.
    if !revive_file_infos_after_post_delete(&post_id).await {
        return;
    }

    let path = format!("/api/v4/posts/{post_id}");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(
        (go_status, rs_status),
        (404, 404),
        "deleted posts are invisible by default"
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "deleted post, no flag");

    let path = format!("/api/v4/posts/{post_id}?include_deleted=true");
    let (go, rs) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    let body: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(body["message"], "", "the short circuit blanks the message");
    assert!(body["delete_at"].as_i64().unwrap_or_default() > 0);
    assert!(body["props"]["deleteBy"].is_string());
    // Blanked metadata, and then `files` alone put back by the second files pass.
    assert_eq!(
        body["metadata"]
            .as_object()
            .map(|m| m.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["files".to_owned()]),
        "a deleted post keeps only its files: {}",
        body["metadata"]
    );
    assert_eq!(body["metadata"]["files"][0]["id"], file_id);

    // `strconv.ParseBool` with the error discarded: `yes` is not true, so this is the 404 again
    // rather than a 400.
    let path = format!("/api/v4/posts/{post_id}?include_deleted=yes");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(
        (go_status, rs_status),
        (404, 404),
        "an unparseable flag is false"
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "include_deleted=yes");

    // And the gate itself: a non-admin asking for a deleted post is refused for
    // `manage_system` **before** the post is looked up, so the same 403 answers whether or not
    // the post exists.
    let plain = create_plain_user(&client, &token, &team, "postdel").await;
    add_user_to_channel(&client, &token, &channel, &plain.id).await;
    let path = format!("/api/v4/posts/{post_id}?include_deleted=true");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &plain.token, &path).await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go_error = assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rs_body,
        "include_deleted without manage_system",
    );
    assert_eq!(go_error["id"], "api.context.permissions.app_error");
    delete_plain_user(&client, &token, &plain.id).await;
}

/// The three refusals a client can provoke without a fixture user, plus the one that needs one.
#[tokio::test]
async fn refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (team, channel) = team_and_channel(&client, &token).await;

    // Alphanumeric, so gorilla routes it — but not 26 characters, so `RequirePostId` rejects it.
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/posts/tooshort").await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go_error = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "short post id");
    assert_eq!(go_error["id"], "api.context.invalid_url_param.app_error");

    // A well-formed id that names nothing.
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/posts/aaaaaaaaaaaaaaaaaaaaaaaaaa").await;
    assert_eq!((go_status, rs_status), (404, 404));
    let go_error = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing post");
    assert_eq!(go_error["id"], "app.post.get.app_error");

    // A private channel the caller is not in: the refusal names `read_channel_content`, not
    // `read_public_channel`, because the duplicated fallback in `GetPostIfAuthorized` only
    // covers open channels.
    let private = create_private_channel(&client, &token, &team, "postpriv").await;
    let hidden = post_message(&client, &token, &private, "not for you", None).await;
    let plain = create_plain_user(&client, &token, &team, "postref").await;

    let path = format!("/api/v4/posts/{hidden}");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &plain.token, &path).await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go_error =
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "private channel");
    assert_eq!(go_error["id"], "api.context.permissions.app_error");

    // The open-channel arm of the same branch: a non-member *can* read a public channel's post,
    // via `read_public_channel` on the team, and the body is the ordinary 200.
    let public_post = post_message(&client, &token, &channel, "public to the team", None).await;
    let path = format!("/api/v4/posts/{public_post}");
    let (go, rs) = fetch_both_stable(&client, &plain.token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "a non-member read of a public channel"
    );

    delete_plain_user(&client, &token, &plain.id).await;
}

async fn create_private_channel(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    tag: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "P",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the private fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// The other half of the contract: every shape whose metadata this port cannot predict has to be
/// **forwarded**, and each of these really does produce an embed or an image on the Go side.
///
/// If `mm_app::post::message_may_contain_a_link` is ever narrowed, one of these starts being
/// answered locally with an empty `metadata` where Go sends an embed — a divergence no
/// byte-comparison of the *served* shapes would notice, because none of them would be served.
#[tokio::test]
async fn shapes_with_links_are_forwarded_and_still_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = team_and_channel(&client, &token).await;

    for message in [
        "see https://example.com for info",
        "a www.example.com bare host",
        "an image ![alt](https://example.com/a.png)",
        "angle <https://example.com> autolink",
    ] {
        let post_id = post_message(&client, &token, &channel, message, None).await;
        assert_forwarded_and_identical(&client, &token, &format!("/api/v4/posts/{post_id}")).await;
    }

    // The other refusal axis: a prop, not the message. `attachments` is `getEmbedForPost`'s very
    // first branch and returns a `message_attachment` embed **before** any link logic — so this
    // post has no link in it at all and must still be forwarded.
    let post_id = post_message(&client, &token, &channel, "no link here at all", None).await;
    if !plant_props(
        &post_id,
        serde_json::json!({"attachments": [{"text": "an attachment"}]}),
    )
    .await
    {
        return;
    }
    let path = format!("/api/v4/posts/{post_id}");
    assert_forwarded_and_identical(&client, &token, &path).await;

    // And say what Go actually answered, so the forward is known to be hiding a real difference
    // rather than an identical body.
    let go: serde_json::Value = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("JSON");
    assert_eq!(
        go["metadata"]["embeds"][0]["type"], "message_attachment",
        "the attachments prop is what makes this shape unreproducible: {}",
        go["metadata"]
    );
}

/// And the mirror image: messages that *look* linkish to a careless filter but that Go's
/// autolinker leaves alone must still be **served**, or the route forwards everything and is
/// worth nothing. An email address is the important one — Mattermost's autolinker has no email
/// rule even though the webapp renders one.
#[tokio::test]
async fn ordinary_messages_are_still_served_locally() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = team_and_channel(&client, &token).await;

    for message in [
        "contact me at someone@example.com",
        "a [markdown link](example) with no scheme",
        "only system emoji here :smile: :+1:",
        "1 * 2 > 0 and 3 - 1 = 2",
    ] {
        let post_id = post_message(&client, &token, &channel, message, None).await;
        let path = format!("/api/v4/posts/{post_id}");
        let (go, rs) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{message:?}"
        );
        let body: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
        assert_eq!(
            body["metadata"],
            serde_json::json!({}),
            "{message:?}: Go found no link either — if it had, this shape would need forwarding"
        );
    }
}
