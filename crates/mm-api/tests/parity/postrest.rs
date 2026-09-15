//! Cross-server parity for the rest of `api4/post.go` (`setPostReminder`, `restorePostVersion`,
//! `moveThread`, `rewriteMessage`, `revealPost`, `burnPost`), `api4/report.go`'s two writes
//! (`getPostsForReporting`, `startUsersBatchExport`) and `api4/integration_action.go`
//! (`doPostAction` and the four dialog routes).
//!
//! ```sh
//! MMRS_STACK=4 scripts/parity.sh -p mm-api --test parity postrest
//! ```
//!
//! # Burn-on-read rows are planted
//!
//! `FillInPostProps` refuses a `burn_on_read` post without an Enterprise **Advanced** licence
//! (app/post.go:657), which no oracle on this stack carries, so the reveal and burn fixtures are
//! rows: a `Posts` row of that type with `props.expire_at`, and its `TemporaryPosts` content.
//! Twins are planted with identical timestamps so a revealed body compares byte for byte once
//! the id is substituted — one twin is revealed through Go, the other through this server, since
//! a second reveal of the same post is a different (no-event) answer.
//!
//! # A real trigger id
//!
//! `openDialog` verifies the trigger id against `Systems.AsymmetricSigningKey`; the suite reads
//! that row — private half included — and mints ids exactly as `model.GenerateTriggerId` does,
//! so the verified path is exercised and not just the six refusals ahead of it.
//!
//! # Rows all begin `mmrsprest`
//!
//! Planted post ids are the prefix plus seventeen digits, 26 characters; the plain users and
//! channels come from the shared helpers and their `mmrsplain` sweep.

use std::time::Duration;

use crate::common;
use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_channel_typed,
    create_direct_channel, create_plain_user, delete_plain_user, fixture_pool, go_minted_token,
    licensed, logged_in_user_id, post_message, request_raw, stack_enabled, update_post,
    upload_file,
};

const PREFIX: &str = "mmrsprest";
/// A 26-character id that names nothing.
const ABSENT_ID: &str = "mmrsprestnosuchrowmmrspres";

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

type Answer = (u16, Vec<u8>, bool);

/// One `doPostAction` refusal row: context, path, token, body, status, error id.
type ActionCase<'a> = (&'a str, String, &'a str, Option<Vec<u8>>, u16, &'a str);

/// One request to one server: status, body, and whether this server answered it itself.
async fn ask(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    body: Option<&[u8]>,
) -> Answer {
    let (status, body, served_by) = request_raw(client, base, method, token, path, body).await;
    (status, body, served_by.as_deref() == Some("rust"))
}

/// The same request to Go and then to this server.
async fn both(
    client: &reqwest::Client,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    body: Option<&[u8]>,
) -> (Answer, Answer) {
    (
        ask(client, GO, method.clone(), token, path, body).await,
        ask(client, RUST, method, token, path, body).await,
    )
}

fn text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// Assert an error answer matches Go's on status and every field but `message`/`request_id`,
/// and that this server answered it.
fn assert_refusal(go: &Answer, ours: &Answer, status: u16, id: &str, context: &str) {
    assert_eq!(go.0, status, "{context}: Go's status ({})", text(&go.1));
    assert_eq!(ours.0, status, "{context}: our status ({})", text(&ours.1));
    assert!(ours.2, "{context}: answered here, not forwarded");
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &ours.1, context);
    assert_eq!(body["id"], id, "{context}: the error id");
}

/// Assert both servers answered `{"status":"OK"}` and this one served it.
fn assert_status_ok(go: &Answer, ours: &Answer, context: &str) {
    assert_eq!(go.0, 200, "{context}: Go ({})", text(&go.1));
    assert_eq!(ours.0, 200, "{context}: ours ({})", text(&ours.1));
    assert!(ours.2, "{context}: answered here");
    assert_eq!(text(&go.1), r#"{"status":"OK"}"#, "{context}: Go's body");
    assert_eq!(text(&ours.1), text(&go.1), "{context}: byte-identical");
}

async fn pool() -> sqlx::PgPool {
    fixture_pool().await.expect("the suite has a database")
}

fn now_millis() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn planted_id(tag: &str) -> String {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 % 1_000_000_000_000)
        .unwrap_or(0);
    let id = format!("{PREFIX}{tag}{suffix:0>17}");
    id[..26].to_owned()
}

/// A burn-on-read post row: `props.expire_at` when `expire_at` is `Some`, a `TemporaryPosts`
/// row carrying `message` when `temporary_expire_at` is `Some`.
async fn plant_burn_on_read_post(
    tag: &str,
    channel_id: &str,
    author_id: &str,
    create_at: i64,
    expire_at: Option<i64>,
    temporary: Option<(i64, &str)>,
) -> String {
    let pool = pool().await;
    let id = planted_id(tag);
    let props = match expire_at {
        Some(expire_at) => serde_json::json!({
            "expire_at": expire_at,
            "read_duration_seconds": 600000
        }),
        None => serde_json::json!({}),
    };
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, rootid, \
         originalid, message, type, props, hashtags, filenames, fileids, hasreactions, editat, \
         ispinned, remoteid) \
         VALUES ($1, $2, $2, 0, $3, $4, '', '', '', 'burn_on_read', $5, '', '[]', '[]', false, \
         0, false, NULL)",
    )
    .bind(&id)
    .bind(create_at)
    .bind(author_id)
    .bind(channel_id)
    .bind(props)
    .execute(&pool)
    .await
    .expect("the burn-on-read post is planted");
    if let Some((temporary_expire_at, message)) = temporary {
        sqlx::query(
            "INSERT INTO temporaryposts (postid, type, expireat, message, fileids) \
             VALUES ($1, 'burn_on_read', $2, $3, '[]')",
        )
        .bind(&id)
        .bind(temporary_expire_at)
        .bind(message)
        .execute(&pool)
        .await
        .expect("the temporary post is planted");
    }
    id
}

/// Remove every row this suite plants, at the start of a run.
/// Once per test binary, like `common::purge_api_fixtures`.
///
/// It used to run at the start of **every** `fixture` call — ten tests — and it deletes every
/// `mmrsprest%` post, so a later test's fixture removed the posts an earlier test was still using.
/// Rust then read the missing row from the database and refused with 400 where Go, serving the
/// row from its cache, answered its own 403 or 404: `do_post_action_gates_match_go` and
/// `reveal_and_burn_refusals_are_served` failed together in a sharded run while passing alone
/// (2026-09-15). Every planted id is unique (`planted_id`), and nothing in this module counts rows
/// across it, so one sweep for an aborted earlier run is all the purge was ever for. The export
/// test removes its own jobs at its end.
async fn purge() {
    static PURGED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    PURGED.get_or_init(purge_once).await;
}

async fn purge_once() {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    for statement in [
        "DELETE FROM readreceipts WHERE postid LIKE 'mmrsprest%'",
        "DELETE FROM temporaryposts WHERE postid LIKE 'mmrsprest%'",
        "DELETE FROM postreminders WHERE postid LIKE 'mmrsprest%'",
        "DELETE FROM posts WHERE id LIKE 'mmrsprest%'",
        "DELETE FROM jobs WHERE type = 'export_users_to_csv' AND data->>'role' LIKE 'mmrsprest%'",
    ] {
        let _ = sqlx::query(statement).execute(&pool).await;
    }
}

async fn read_receipt(post_id: &str, user_id: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT expireat FROM readreceipts WHERE postid = $1 AND userid = $2")
        .bind(post_id)
        .bind(user_id)
        .fetch_optional(&pool().await)
        .await
        .expect("the receipt reads")
}

async fn temporary_expire_at(post_id: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT expireat FROM temporaryposts WHERE postid = $1")
        .bind(post_id)
        .fetch_optional(&pool().await)
        .await
        .expect("the temporary post reads")
}

async fn reminder_target(post_id: &str, user_id: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT targettime FROM postreminders WHERE postid = $1 AND userid = $2")
        .bind(post_id)
        .bind(user_id)
        .fetch_optional(&pool().await)
        .await
        .expect("the reminder reads")
}

/// The stack's P-256 signing key, from the row Go wrote at first boot.
async fn signing_key() -> p256::ecdsa::SigningKey {
    let row: String =
        sqlx::query_scalar("SELECT value FROM systems WHERE name = 'AsymmetricSigningKey'")
            .fetch_one(&pool().await)
            .await
            .expect("the signing key row exists");
    let parsed: serde_json::Value = serde_json::from_str(&row).expect("the row is JSON");
    // `D` is a `*big.Int` marshalled as a bare decimal integer far outside f64: keep the text.
    let raw = row
        .split("\"d\":")
        .nth(1)
        .expect("the row carries d")
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    assert_eq!(parsed["ecdsa_key"]["curve"], "P-256");
    let bytes = decimal_to_bytes(&raw, 32);
    p256::ecdsa::SigningKey::from_slice(&bytes).expect("a valid scalar")
}

/// A decimal big integer as a big-endian fixed-width byte string.
fn decimal_to_bytes(decimal: &str, width: usize) -> Vec<u8> {
    let mut digits: Vec<u8> = decimal.bytes().map(|b| b - b'0').collect();
    let mut out = Vec::new();
    while !digits.is_empty() && !(digits.len() == 1 && digits[0] == 0) {
        let mut remainder = 0u32;
        let mut next = Vec::with_capacity(digits.len());
        for d in &digits {
            let value = remainder * 10 + u32::from(*d);
            let q = value / 256;
            remainder = value % 256;
            if !(next.is_empty() && q == 0) {
                next.push(q as u8);
            }
        }
        out.push(remainder as u8);
        digits = next;
    }
    out.resize(width, 0);
    out.reverse();
    out
}

/// `model.GenerateTriggerId` with a caller-chosen client id and timestamp.
fn mint_trigger_id(
    key: &p256::ecdsa::SigningKey,
    client_trigger_id: &str,
    user_id: &str,
    millis: i64,
) -> String {
    use base64::Engine as _;
    use p256::ecdsa::signature::Signer as _;
    let data = format!("{client_trigger_id}:{user_id}:{millis}:");
    let signature: p256::ecdsa::Signature = key.sign(data.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(signature.to_der().as_bytes());
    base64::engine::general_purpose::STANDARD.encode(format!("{data}{sig}").as_bytes())
}

/// The frames of `event` a probe collected within the window.
async fn events(probe: &mut SocketProbe, event: &str) -> Vec<serde_json::Value> {
    let name = event.to_owned();
    probe
        .collect_until(Duration::from_secs(4), |frames| {
            frames.iter().any(|f| f["event"] == name)
        })
        .await;
    probe.events_named(event)
}

/// A post JSON with its id (and the twin's) blanked, so two twins compare equal.
fn normalised_post(raw: &str, ids: &[&str]) -> serde_json::Value {
    let mut raw = raw.to_owned();
    for id in ids {
        raw = raw.replace(id, "<twin>");
    }
    serde_json::from_str(&raw).expect("a post decodes")
}

struct Fixture {
    public_channel: String,
    private_channel: String,
    reader: common::PlainUser,
    outsider: common::PlainUser,
}

async fn fixture(client: &reqwest::Client, admin: &str, tag: &str) -> Fixture {
    purge().await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(client, admin).await;
    let reader = create_plain_user(client, admin, &team_id, &format!("{tag}r")).await;
    let outsider = create_plain_user(client, admin, &team_id, &format!("{tag}o")).await;
    let public_channel = create_channel(client, admin, &team_id, &format!("{tag}pub")).await;
    add_user_to_channel(client, admin, &public_channel, &reader.id).await;
    let private_channel =
        create_channel_typed(client, admin, &team_id, &format!("{tag}priv"), "P").await;
    Fixture {
        public_channel,
        private_channel,
        reader,
        outsider,
    }
}

async fn unwind(client: &reqwest::Client, admin: &str, fixture: Fixture) {
    delete_plain_user(client, admin, &fixture.reader.id).await;
    delete_plain_user(client, admin, &fixture.outsider.id).await;
}

// ---------------------------------------------------------------------------------------------
// setPostReminder
// ---------------------------------------------------------------------------------------------

/// A reminder on a team-channel post is served, its row written, and the confirmation on the
/// reader's socket — permalink embed included — is Go's; a reminder on a DM post is forwarded;
/// and the five refusals ahead of the write are served.
#[tokio::test]
async fn reminder_on_a_team_channel_post_is_served_and_a_dm_one_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "rem").await;
    let reader = &fx.reader;

    let post = post_message(
        &client,
        &admin,
        &fx.public_channel,
        "mmrsprest remind me",
        None,
    )
    .await;
    let private_post = post_message(
        &client,
        &admin,
        &fx.private_channel,
        "mmrsprest private",
        None,
    )
    .await;
    let dm = create_direct_channel(&client, &admin, logged_in_user_id(), &reader.id).await;
    let dm_post = post_message(&client, &admin, &dm, "mmrsprest dm", None).await;

    let path = |user: &str, post: &str| format!("/api/v4/users/{user}/posts/{post}/reminder");
    let body = |target: i64| format!(r#"{{"target_time":{target}}}"#);

    // The served write, on the reader's own socket on each server.
    let mut go_probe = SocketProbe::connect(GO, &reader.token).await;
    let mut rs_probe = SocketProbe::connect(RUST, &reader.token).await;
    let go = ask(
        &client,
        GO,
        reqwest::Method::POST,
        Some(&reader.token),
        &path(&reader.id, &post),
        Some(body(4_102_444_800).as_bytes()),
    )
    .await;
    assert_eq!(
        reminder_target(&post, &reader.id).await,
        Some(4_102_444_800)
    );
    let ours = ask(
        &client,
        RUST,
        reqwest::Method::POST,
        Some(&reader.token),
        &path("me", &post),
        Some(body(4_102_448_400).as_bytes()),
    )
    .await;
    assert_status_ok(&go, &ours, "reminder on a team-channel post");
    assert_eq!(
        reminder_target(&post, &reader.id).await,
        Some(4_102_448_400),
        "the upsert moved the target time"
    );

    let go_events = events(&mut go_probe, "ephemeral_message").await;
    let rs_events = events(&mut rs_probe, "ephemeral_message").await;
    assert_eq!(
        go_events.len(),
        1,
        "Go sent one confirmation: {:?}",
        go_probe.raw
    );
    assert_eq!(
        rs_events.len(),
        1,
        "we sent one confirmation: {:?}",
        rs_probe.raw
    );
    let go_post = go_events[0]["data"]["post"]
        .as_str()
        .expect("a post string");
    let rs_post = rs_events[0]["data"]["post"]
        .as_str()
        .expect("a post string");
    let mut go_value: serde_json::Value = serde_json::from_str(go_post).expect("Go's post decodes");
    let mut rs_value: serde_json::Value = serde_json::from_str(rs_post).expect("our post decodes");
    // The two ephemeral posts differ in their fresh id, their `create_at`, and the target time
    // each request carried; everything else — the message, the props, the permalink embed
    // with the reminded post inside it — must agree.
    for value in [&mut go_value, &mut rs_value] {
        value["id"] = serde_json::Value::String(String::new());
        value["create_at"] = serde_json::json!(0);
        value["props"]["target_time"] = serde_json::json!(0);
        let message = value["message"].as_str().unwrap_or_default().to_owned();
        let trimmed = message.split(" at ").next().unwrap_or_default().to_owned();
        value["message"] = serde_json::Value::String(trimmed);
    }
    assert_eq!(
        go_value, rs_value,
        "the confirmation differs\n  go:   {go_post}\n  rust: {rs_post}"
    );
    assert_eq!(
        go_value["metadata"]["embeds"][0]["type"], "permalink",
        "Go's confirmation carries the permalink preview: {go_post}"
    );
    assert_eq!(
        go_events[0]["broadcast"]["user_id"], rs_events[0]["broadcast"]["user_id"],
        "addressed to the reader"
    );

    // A DM post's permalink has no team segment: Go fetches it, and we hand it to Go.
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&reader.token),
        &path(&reader.id, &dm_post),
        Some(body(4_102_444_800).as_bytes()),
    )
    .await;
    assert_eq!((go.0, text(&go.1)), (200, r#"{"status":"OK"}"#.to_owned()));
    assert_eq!(ours.0, 200);
    assert!(!ours.2, "the DM reminder is forwarded");

    // The refusals, in the handler's order.
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&reader.token),
        &path(&reader.id, "short"),
        Some(b"{}"),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "api.context.invalid_url_param.app_error",
        "short post id",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&reader.token),
        &path(logged_in_user_id(), &post),
        Some(body(1).as_bytes()),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.context.permissions.app_error",
        "another user",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&reader.token),
        &path(&reader.id, &private_post),
        Some(body(1).as_bytes()),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.context.permissions.app_error",
        "unreadable post",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&reader.token),
        &path(&reader.id, &post),
        Some(br#"{"target_time":"soon"}"#),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "api.context.invalid_body_param.app_error",
        "bad body",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&reader.token),
        &path(&reader.id, ABSENT_ID),
        Some(body(1).as_bytes()),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.context.permissions.app_error",
        "absent post",
    );

    unwind(&client, &admin, fx).await;
}

// ---------------------------------------------------------------------------------------------
// restorePostVersion
// ---------------------------------------------------------------------------------------------

/// A version restored through each server comes back the same shape, and the refusals — an
/// unknown version, another author's version, a version of a different post — are served.
#[tokio::test]
async fn restore_post_version_is_served_with_its_refusals() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "res").await;

    let edit_history = async |post: &str| -> String {
        let (status, body, _) = request_raw(
            &client,
            GO,
            reqwest::Method::GET,
            Some(&admin),
            &format!("/api/v4/posts/{post}/edit_history"),
            None,
        )
        .await;
        assert_eq!(status, 200, "edit history: {}", text(&body));
        let versions: serde_json::Value = serde_json::from_slice(&body).expect("versions decode");
        versions[0]["id"].as_str().expect("a version id").to_owned()
    };

    // Two twins, each edited once, one restored through each server.
    let twin_a = post_message(
        &client,
        &admin,
        &fx.public_channel,
        "mmrsprest original",
        None,
    )
    .await;
    update_post(&client, &admin, &twin_a, "mmrsprest edited").await;
    let version_a = edit_history(&twin_a).await;
    let twin_b = post_message(
        &client,
        &admin,
        &fx.public_channel,
        "mmrsprest original",
        None,
    )
    .await;
    update_post(&client, &admin, &twin_b, "mmrsprest edited").await;
    let version_b = edit_history(&twin_b).await;

    let path = |post: &str, version: &str| format!("/api/v4/posts/{post}/restore/{version}");
    let go = ask(
        &client,
        GO,
        reqwest::Method::POST,
        Some(&admin),
        &path(&twin_a, &version_a),
        None,
    )
    .await;
    let ours = ask(
        &client,
        RUST,
        reqwest::Method::POST,
        Some(&admin),
        &path(&twin_b, &version_b),
        None,
    )
    .await;
    assert_eq!(go.0, 200, "Go restores: {}", text(&go.1));
    assert_eq!(ours.0, 200, "we restore: {}", text(&ours.1));
    assert!(ours.2, "answered here");
    let mut go_value: serde_json::Value = serde_json::from_slice(&go.1).expect("a post");
    let mut rs_value: serde_json::Value = serde_json::from_slice(&ours.1).expect("a post");
    assert_eq!(go_value["message"], "mmrsprest original");
    for value in [&mut go_value, &mut rs_value] {
        for key in ["id", "create_at", "update_at", "edit_at"] {
            value[key] = serde_json::json!(0);
        }
    }
    assert_eq!(go_value, rs_value, "the restored posts differ in shape");
    assert!(
        go.1.ends_with(b"\n") && ours.1.ends_with(b"\n"),
        "EncodeJSON's newline"
    );

    // The refusals. `version_a` now names a history row of twin A, so it is "not an history
    // item" of twin B.
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&admin),
        &path(&twin_b, &version_a),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.post.restore_post_version.not_an_history_item.app_error",
        "wrong post",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&admin),
        &path(&twin_a, ABSENT_ID),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.context.permissions.app_error",
        "unknown version",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&fx.reader.token),
        &path(&twin_a, &version_a),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.context.permissions.app_error",
        "another author",
    );
    // A live post as the "version": read, owned, but `original_id` is empty.
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&admin),
        &path(&twin_a, &twin_b),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.post.restore_post_version.not_an_history_item.app_error",
        "a live post as version",
    );

    unwind(&client, &admin, fx).await;
}

// ---------------------------------------------------------------------------------------------
// moveThread, rewriteMessage
// ---------------------------------------------------------------------------------------------

/// `moveThread` is the 501 on this stack, after the post id; `rewriteMessage`'s four refusals
/// are served and the bridge call is forwarded.
#[tokio::test]
async fn move_thread_is_the_gate_and_rewrite_gates_then_forwards() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "mov").await;

    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&admin),
        &format!("/api/v4/posts/{ABSENT_ID}/move"),
        Some(br#"{"channel_id":"x"}"#),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        501,
        "api.post.move_thread.disabled.app_error",
        "move",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/posts/short/move",
        Some(b"{}"),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "api.context.invalid_url_param.app_error",
        "move, short id",
    );

    let private_post = post_message(
        &client,
        &admin,
        &fx.private_channel,
        "mmrsprest private",
        None,
    )
    .await;
    let rewrite = |body: &str| body.as_bytes().to_vec();
    let cases: [(&str, Vec<u8>, u16, &str); 5] = [
        (
            "bad body",
            rewrite("{"),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "agent id",
            rewrite(r#"{"agent_id":"nope","message":"hi","action":"shorten"}"#),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "root id",
            rewrite(
                r#"{"agent_id":"abcdefghijklmnopqrstuvwxyz","message":"hi","action":"shorten","root_id":"nope"}"#,
            ),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "unreadable root",
            rewrite(&format!(
                r#"{{"agent_id":"abcdefghijklmnopqrstuvwxyz","message":"hi","action":"shorten","root_id":"{private_post}"}}"#
            )),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "unknown action",
            rewrite(
                r#"{"agent_id":"abcdefghijklmnopqrstuvwxyz","message":"hi","action":"translate"}"#,
            ),
            400,
            "app.post.rewrite.invalid_action",
        ),
    ];
    for (context, body, status, id) in &cases {
        let token = if *context == "unreadable root" {
            &fx.reader.token
        } else {
            &admin
        };
        let (go, ours) = both(
            &client,
            reqwest::Method::POST,
            Some(token),
            "/api/v4/posts/rewrite",
            Some(body),
        )
        .await;
        assert_refusal(&go, &ours, *status, id, context);
    }
    // An empty message accepts any action; the bridge is Go's.
    let (go, ours) = both(
        &client,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/posts/rewrite",
        Some(br#"{"agent_id":"abcdefghijklmnopqrstuvwxyz","message":"","action":"translate","custom_prompt":"x"}"#),
    )
    .await;
    assert_eq!(
        go.0,
        500,
        "Go's bridge fails without the plugin: {}",
        text(&go.1)
    );
    assert_eq!(ours.0, 500);
    assert!(!ours.2, "the bridge call is forwarded");
    let id_of = |body: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["id"].as_str().map(str::to_owned))
    };
    assert_eq!(id_of(&go.1), id_of(&ours.1), "the forwarded bridge failure");
    assert_eq!(
        id_of(&go.1).as_deref(),
        Some("app.post.rewrite.agent_call_failed")
    );

    unwind(&client, &admin, fx).await;
}

// ---------------------------------------------------------------------------------------------
// revealPost, burnPost
// ---------------------------------------------------------------------------------------------

/// Twins revealed through each server come back byte-identical once the id is substituted; the
/// two `post_revealed` events, the all-revealed event, the receipt and the temporary row agree;
/// a burn expires the receipt and publishes `post_burned`; and what follows a burn is the 404.
#[tokio::test]
async fn reveal_and_burn_twins_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "rev").await;
    let reader = &fx.reader;
    let author = logged_in_user_id();

    let created = now_millis() - 60_000;
    let expire_at = now_millis() + 5 * 60_000;
    // The temporary row expires later than the post so that `updateTemporaryPostIfAllRead`
    // has something to move: with the reader the only other member, the first reveal is the
    // last, and the row's expiry becomes the receipt's.
    let twin_a = plant_burn_on_read_post(
        "a",
        &fx.public_channel,
        author,
        created,
        Some(expire_at),
        Some((expire_at + 1000, "mmrsprest secret")),
    )
    .await;
    let twin_b = plant_burn_on_read_post(
        "b",
        &fx.public_channel,
        author,
        created,
        Some(expire_at),
        Some((expire_at + 1000, "mmrsprest secret")),
    )
    .await;

    let reveal = |post: &str| format!("/api/v4/posts/{post}/reveal");
    let burn = |post: &str| format!("/api/v4/posts/{post}/burn");

    let mut go_author = SocketProbe::connect(GO, &admin).await;
    let mut rs_author = SocketProbe::connect(RUST, &admin).await;
    let mut go_reader = SocketProbe::connect(GO, &reader.token).await;
    let mut rs_reader = SocketProbe::connect(RUST, &reader.token).await;

    let go = ask(
        &client,
        GO,
        reqwest::Method::GET,
        Some(&reader.token),
        &reveal(&twin_a),
        None,
    )
    .await;
    let ours = ask(
        &client,
        RUST,
        reqwest::Method::GET,
        Some(&reader.token),
        &reveal(&twin_b),
        None,
    )
    .await;
    assert_eq!(go.0, 200, "Go reveals: {}", text(&go.1));
    assert_eq!(ours.0, 200, "we reveal: {}", text(&ours.1));
    assert!(ours.2, "answered here");
    let ids = [twin_a.as_str(), twin_b.as_str()];
    assert_eq!(
        text(&go.1).replace(&twin_a, "<twin>"),
        text(&ours.1).replace(&twin_b, "<twin>"),
        "the revealed bodies differ"
    );
    let revealed: serde_json::Value = serde_json::from_slice(&go.1).expect("a post");
    assert_eq!(revealed["message"], "mmrsprest secret");
    assert_eq!(
        revealed["metadata"]["expire_at"],
        serde_json::json!(expire_at)
    );
    assert_eq!(
        read_receipt(&twin_a, &reader.id).await,
        Some(expire_at),
        "Go's receipt"
    );
    assert_eq!(
        read_receipt(&twin_b, &reader.id).await,
        Some(expire_at),
        "our receipt"
    );
    assert_eq!(
        temporary_expire_at(&twin_a).await,
        Some(expire_at),
        "Go moved the temporary row"
    );
    assert_eq!(
        temporary_expire_at(&twin_b).await,
        Some(expire_at),
        "we moved the temporary row"
    );

    // The author hears `post_revealed` (with the reader as recipient) and the all-revealed
    // event; the reader hears `post_revealed`.
    let go_to_author = events(&mut go_author, "post_revealed").await;
    let rs_to_author = events(&mut rs_author, "post_revealed").await;
    assert_eq!(
        go_to_author.len(),
        1,
        "Go told the author: {:?}",
        go_author.raw
    );
    assert_eq!(
        rs_to_author.len(),
        1,
        "we told the author: {:?}",
        rs_author.raw
    );
    assert_eq!(
        go_to_author[0]["data"]["recipients"],
        serde_json::json!([reader.id])
    );
    assert_eq!(
        rs_to_author[0]["data"]["recipients"],
        serde_json::json!([reader.id])
    );
    assert_eq!(
        normalised_post(
            go_to_author[0]["data"]["post"].as_str().unwrap_or_default(),
            &ids
        ),
        normalised_post(
            rs_to_author[0]["data"]["post"].as_str().unwrap_or_default(),
            &ids
        ),
        "the author's event differs"
    );
    let go_all = events(&mut go_author, "burn_on_read_all_revealed").await;
    let rs_all = events(&mut rs_author, "burn_on_read_all_revealed").await;
    assert_eq!(go_all.len(), 1, "Go's all-revealed: {:?}", go_author.raw);
    assert_eq!(rs_all.len(), 1, "our all-revealed: {:?}", rs_author.raw);
    assert_eq!(
        go_all[0]["data"]["sender_expire_at"],
        rs_all[0]["data"]["sender_expire_at"]
    );
    assert_eq!(go_all[0]["data"]["post_id"], twin_a);
    assert_eq!(rs_all[0]["data"]["post_id"], twin_b);
    let go_to_reader = events(&mut go_reader, "post_revealed").await;
    let rs_to_reader = events(&mut rs_reader, "post_revealed").await;
    assert_eq!(
        go_to_reader.len(),
        1,
        "Go told the reader: {:?}",
        go_reader.raw
    );
    assert_eq!(
        rs_to_reader.len(),
        1,
        "we told the reader: {:?}",
        rs_reader.raw
    );
    assert_eq!(
        normalised_post(
            go_to_reader[0]["data"]["post"].as_str().unwrap_or_default(),
            &ids
        ),
        normalised_post(
            rs_to_reader[0]["data"]["post"].as_str().unwrap_or_default(),
            &ids
        ),
        "the reader's event differs"
    );

    // A second reveal is the same body and no event.
    go_reader.raw.clear();
    rs_reader.raw.clear();
    let go2 = ask(
        &client,
        GO,
        reqwest::Method::GET,
        Some(&reader.token),
        &reveal(&twin_a),
        None,
    )
    .await;
    let ours2 = ask(
        &client,
        RUST,
        reqwest::Method::GET,
        Some(&reader.token),
        &reveal(&twin_b),
        None,
    )
    .await;
    assert_eq!((go2.0, text(&go2.1)), (200, text(&go.1)));
    assert_eq!((ours2.0, text(&ours2.1)), (200, text(&ours.1)));
    go_reader.collect_for(Duration::from_millis(600)).await;
    rs_reader.collect_for(Duration::from_millis(600)).await;
    assert!(
        go_reader.events_named("post_revealed").is_empty(),
        "Go: {:?}",
        go_reader.raw
    );
    assert!(
        rs_reader.events_named("post_revealed").is_empty(),
        "ours: {:?}",
        rs_reader.raw
    );

    // The burn: the receipt's expiry becomes now, and the reader hears `post_burned`.
    let before = now_millis();
    let go = ask(
        &client,
        GO,
        reqwest::Method::DELETE,
        Some(&reader.token),
        &burn(&twin_a),
        None,
    )
    .await;
    let ours = ask(
        &client,
        RUST,
        reqwest::Method::DELETE,
        Some(&reader.token),
        &burn(&twin_b),
        None,
    )
    .await;
    assert_status_ok(&go, &ours, "burn");
    let after = now_millis();
    for (twin, who) in [(&twin_a, "Go"), (&twin_b, "ours")] {
        let expire = read_receipt(twin, &reader.id)
            .await
            .expect("the receipt stays");
        assert!(
            (before..=after).contains(&expire),
            "{who}: the receipt expires now ({expire})"
        );
    }
    let go_burned = events(&mut go_reader, "post_burned").await;
    let rs_burned = events(&mut rs_reader, "post_burned").await;
    assert_eq!(go_burned.len(), 1, "Go's post_burned: {:?}", go_reader.raw);
    assert_eq!(rs_burned.len(), 1, "our post_burned: {:?}", rs_reader.raw);
    assert_eq!(go_burned[0]["data"]["post_id"], twin_a);
    assert_eq!(rs_burned[0]["data"]["post_id"], twin_b);
    assert_eq!(
        go_burned[0]["broadcast"]["channel_id"],
        rs_burned[0]["broadcast"]["channel_id"]
    );

    // After the burn the receipt is expired, and `GetSinglePost` drops the post: the 404.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (go, ours) = both(
        &client,
        reqwest::Method::GET,
        Some(&reader.token),
        &reveal(&twin_a),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        404,
        "app.post.get.app_error",
        "reveal after burn",
    );
    let (go, ours) = both(
        &client,
        reqwest::Method::DELETE,
        Some(&reader.token),
        &burn(&twin_a),
        None,
    )
    .await;
    assert_refusal(&go, &ours, 404, "app.post.get.app_error", "burn after burn");

    unwind(&client, &admin, fx).await;
}

/// The refusals around a reveal and a burn, each served: not a burn-on-read post, no
/// `expire_at`, expired, the author's own reveal, a non-member, an unrevealed burn, and the
/// author's burn handed to Go.
#[tokio::test]
async fn reveal_and_burn_refusals_are_served() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "rfu").await;
    let reader = &fx.reader;
    let author = logged_in_user_id();
    let created = now_millis() - 60_000;
    let future = now_millis() + 5 * 60_000;

    let plain = post_message(&client, &admin, &fx.public_channel, "mmrsprest plain", None).await;
    let no_expiry = plant_burn_on_read_post(
        "n",
        &fx.public_channel,
        author,
        created,
        None,
        Some((future, "x")),
    )
    .await;
    let expired = plant_burn_on_read_post(
        "e",
        &fx.public_channel,
        author,
        created,
        Some(created),
        Some((future, "x")),
    )
    .await;
    let live = plant_burn_on_read_post(
        "l",
        &fx.public_channel,
        author,
        created,
        Some(future),
        Some((future, "x")),
    )
    .await;
    let for_author_go = plant_burn_on_read_post(
        "f",
        &fx.public_channel,
        author,
        created,
        Some(future),
        Some((future, "x")),
    )
    .await;
    let for_author_rs = plant_burn_on_read_post(
        "g",
        &fx.public_channel,
        author,
        created,
        Some(future),
        Some((future, "x")),
    )
    .await;

    let reveal = |post: &str| format!("/api/v4/posts/{post}/reveal");
    let burn = |post: &str| format!("/api/v4/posts/{post}/burn");
    let get = reqwest::Method::GET;
    let del = reqwest::Method::DELETE;

    let (go, ours) = both(
        &client,
        get.clone(),
        Some(&reader.token),
        &reveal(&plain),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.reveal_post.not_burn_on_read.app_error",
        "reveal a plain post",
    );
    let (go, ours) = both(
        &client,
        get.clone(),
        Some(&reader.token),
        &reveal(&no_expiry),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.reveal_post.missing_expire_at.app_error",
        "no expire_at",
    );
    let (go, ours) = both(
        &client,
        get.clone(),
        Some(&reader.token),
        &reveal(&expired),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.reveal_post.post_expired.app_error",
        "expired",
    );
    let (go, ours) = both(&client, get.clone(), Some(&admin), &reveal(&live), None).await;
    assert_refusal(
        &go,
        &ours,
        400,
        "api.post.reveal_post.cannot_reveal_own_post.app_error",
        "own post",
    );
    let (go, ours) = both(
        &client,
        get.clone(),
        Some(&fx.outsider.token),
        &reveal(&live),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.post.reveal_post.user_not_in_channel.app_error",
        "non-member reveal",
    );
    let (go, ours) = both(
        &client,
        get.clone(),
        Some(&reader.token),
        &reveal(ABSENT_ID),
        None,
    )
    .await;
    assert_refusal(&go, &ours, 404, "app.post.get.app_error", "absent");
    let (go, ours) = both(
        &client,
        get.clone(),
        Some(&reader.token),
        &reveal("short"),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "api.context.invalid_url_param.app_error",
        "short id",
    );

    let (go, ours) = both(
        &client,
        del.clone(),
        Some(&reader.token),
        &burn(&plain),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.burn_post.not_burn_on_read.app_error",
        "burn a plain post",
    );
    let (go, ours) = both(
        &client,
        del.clone(),
        Some(&reader.token),
        &burn(&live),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.burn_post.not_revealed.app_error",
        "unrevealed burn",
    );
    let (go, ours) = both(
        &client,
        del.clone(),
        Some(&fx.outsider.token),
        &burn(&live),
        None,
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.post.burn_post.user_not_in_channel.app_error",
        "non-member burn",
    );
    // The author's own burn is `PermanentDeletePostDataRetainStub`: Go's, on both servers.
    let go = ask(
        &client,
        GO,
        del.clone(),
        Some(&admin),
        &burn(&for_author_go),
        None,
    )
    .await;
    let ours = ask(
        &client,
        RUST,
        del.clone(),
        Some(&admin),
        &burn(&for_author_rs),
        None,
    )
    .await;
    assert_eq!(go.0, 200, "Go burns the author's post: {}", text(&go.1));
    assert!(!ours.2, "the author's burn is forwarded");
    assert_eq!((ours.0, text(&ours.1)), (200, text(&go.1)));

    unwind(&client, &admin, fx).await;
}

// ---------------------------------------------------------------------------------------------
// getPostsForReporting
// ---------------------------------------------------------------------------------------------

/// Unlicensed, the route is the admin gate and the licence 400; licensed, every page — both
/// directions, both time fields, deleted and system posts in and out, the cursor round trip
/// and its refusals — is byte-identical.
#[tokio::test]
async fn posts_for_reporting_pages_match_go_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "rep").await;
    let path = "/api/v4/reports/posts";
    let post = reqwest::Method::POST;

    let (go, ours) = both(
        &client,
        post.clone(),
        Some(&fx.reader.token),
        path,
        Some(br#"{"channel_id":"x"}"#),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.context.permissions.app_error",
        "not an admin",
    );
    let (go, ours) = both(
        &client,
        post.clone(),
        Some(&admin),
        path,
        Some(format!(r#"{{"channel_id":"{}"}}"#, fx.public_channel).as_bytes()),
    )
    .await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.post.get_posts_for_reporting.license_error",
        "unlicensed",
    );

    // Five posts, one deleted, plus the system join posts the fixture left; reply counts too.
    let channel = &fx.public_channel;
    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(
            post_message(
                &client,
                &admin,
                channel,
                &format!("mmrsprest report {n} & <b>"),
                None,
            )
            .await,
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    post_message(&client, &admin, channel, "mmrsprest reply", Some(&ids[0])).await;
    common::delete_post(&client, &admin, &ids[2]).await;

    let pair = licensed().await;
    let licensed_both = async |body: &str| -> (Answer, Answer) {
        (
            ask(
                &client,
                &pair.go,
                post.clone(),
                Some(&admin),
                path,
                Some(body.as_bytes()),
            )
            .await,
            ask(
                &client,
                &pair.rust,
                post.clone(),
                Some(&admin),
                path,
                Some(body.as_bytes()),
            )
            .await,
        )
    };
    let assert_page = |go: &Answer, ours: &Answer, context: &str| -> serde_json::Value {
        assert_eq!(go.0, 200, "{context}: Go ({})", text(&go.1));
        assert_eq!(ours.0, 200, "{context}: ours ({})", text(&ours.1));
        assert!(ours.2, "{context}: answered here");
        assert_eq!(text(&go.1), text(&ours.1), "{context}: byte-identical");
        serde_json::from_slice(&go.1).expect("a page")
    };

    // Ascending by create_at, two per page, cursor round trip until the last page.
    let mut body = format!(r#"{{"channel_id":"{channel}","per_page":2}}"#);
    let mut seen = 0;
    for page in 0..8 {
        let (go, ours) = licensed_both(&body).await;
        let value = assert_page(&go, &ours, &format!("asc page {page}"));
        seen += value["posts"].as_array().map_or(0, Vec::len);
        match value["next_cursor"]["cursor"].as_str() {
            Some(cursor) => {
                body = format!(
                    r#"{{"channel_id":"{channel}","per_page":2,"cursor":"{cursor}","sort_direction":"desc"}}"#
                );
            }
            None => break,
        }
    }
    assert!(
        seen >= 6,
        "the pages covered the channel's live posts: {seen}"
    );

    // Descending, by update_at, deleted posts included, system posts excluded, with metadata.
    for (context, extra) in [
        ("desc", r#""sort_direction":"desc""#),
        ("update_at", r#""time_field":"update_at","per_page":3"#),
        ("include_deleted", r#""include_deleted":true"#),
        ("exclude_system_posts", r#""exclude_system_posts":true"#),
        ("include_metadata", r#""include_metadata":true"#),
        ("start_time", r#""start_time":1"#),
        ("per_page capped", r#""per_page":5000"#),
    ] {
        let (go, ours) = licensed_both(&format!(r#"{{"channel_id":"{channel}",{extra}}}"#)).await;
        let value = assert_page(&go, &ours, context);
        assert!(
            !value["posts"].as_array().is_none_or(Vec::is_empty),
            "{context}: a page"
        );
    }

    // The refusals on the licensed pair, and the empty page's channel read.
    let refusals: [(&str, String, u16, &str); 6] = [
        (
            "bad body",
            "{".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "bad channel",
            r#"{"channel_id":"short"}"#.to_owned(),
            400,
            "model.post.query_params.invalid_channel_id",
        ),
        (
            "bad cursor",
            r#"{"cursor":"***"}"#.to_owned(),
            400,
            "model.post.decode_cursor.invalid_base64",
        ),
        (
            "cursor parts",
            format!(r#"{{"cursor":"{}"}}"#, {
                use base64::Engine as _;
                base64::engine::general_purpose::URL_SAFE.encode("1:a:b")
            }),
            400,
            "model.post.decode_cursor.invalid_format",
        ),
        (
            "cursor sort",
            format!(r#"{{"cursor":"{}"}}"#, {
                use base64::Engine as _;
                base64::engine::general_purpose::URL_SAFE
                    .encode(format!("1:{channel}:create_at:false:false:sideways:0:"))
            }),
            400,
            "model.post.query_params.invalid_sort_direction",
        ),
        (
            "empty page, absent channel",
            format!(r#"{{"channel_id":"{ABSENT_ID}"}}"#),
            404,
            "app.channel.get.existing.app_error",
        ),
    ];
    for (context, body, status, id) in &refusals {
        let (go, ours) = licensed_both(body).await;
        assert_refusal(&go, &ours, *status, id, context);
    }
    // A valid cursor for an empty tail with no body channel: the body's empty channel is read.
    let (go, ours) = licensed_both(&format!(r#"{{"cursor":"{}"}}"#, {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE.encode(format!(
            "1:{channel}:create_at:false:false:asc:{}:",
            i64::MAX - 1
        ))
    }))
    .await;
    assert_refusal(
        &go,
        &ours,
        404,
        "app.channel.get.existing.app_error",
        "cursor tail without a body channel",
    );

    unwind(&client, &admin, fx).await;
}

// ---------------------------------------------------------------------------------------------
// startUsersBatchExport
// ---------------------------------------------------------------------------------------------

/// Unlicensed, the admin gate and the licence 400; licensed, a job created through each server,
/// the duplicate refused by both against a planted pending job, the two query refusals, and the
/// system bot's DM on each server.
#[tokio::test]
async fn users_batch_export_creates_the_job_and_the_dm_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "exp").await;
    let path = "/api/v4/reports/users/export";
    let post = reqwest::Method::POST;

    let (go, ours) = both(&client, post.clone(), Some(&fx.reader.token), path, None).await;
    assert_refusal(
        &go,
        &ours,
        403,
        "api.context.permissions.app_error",
        "not an admin",
    );
    let (go, ours) = both(&client, post.clone(), Some(&admin), path, None).await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.report.start_users_batch_export.license_error",
        "unlicensed",
    );

    let pair = licensed().await;
    let licensed_both = async |query: &str| -> (Answer, Answer) {
        (
            ask(
                &client,
                &pair.go,
                post.clone(),
                Some(&admin),
                &format!("{path}?{query}"),
                None,
            )
            .await,
            ask(
                &client,
                &pair.rust,
                post.clone(),
                Some(&admin),
                &format!("{path}?{query}"),
                None,
            )
            .await,
        )
    };
    let (go, ours) = licensed_both("team_filter=short").await;
    assert_refusal(
        &go,
        &ours,
        400,
        "api.getUsersForReporting.invalid_team_filter",
        "team filter",
    );
    let (go, ours) = licensed_both("hide_active=true&hide_inactive=true").await;
    assert_refusal(
        &go,
        &ours,
        400,
        "api.getUsersForReporting.invalid_active_filter",
        "both hidden",
    );

    // The system bot's DM with the admin, watched on each server before the export starts.
    let (status, body, _) = request_raw(
        &client,
        GO,
        reqwest::Method::GET,
        Some(&admin),
        "/api/v4/users/username/system-bot",
        None,
    )
    .await;
    let bot_id = if status == 200 {
        serde_json::from_slice::<serde_json::Value>(&body).expect("the bot")["id"]
            .as_str()
            .expect("an id")
            .to_owned()
    } else {
        String::new()
    };
    let mut go_probe = SocketProbe::connect(&pair.go, &admin).await;
    let mut rs_probe = SocketProbe::connect(&pair.rust, &admin).await;

    let role_go = format!("{PREFIX}go{}", now_millis());
    let role_rs = format!("{PREFIX}rs{}", now_millis());
    let go = ask(
        &client,
        &pair.go,
        post.clone(),
        Some(&admin),
        &format!("{path}?role_filter={role_go}&date_range=last_30_days"),
        None,
    )
    .await;
    let ours = ask(
        &client,
        &pair.rust,
        post.clone(),
        Some(&admin),
        &format!("{path}?role_filter={role_rs}"),
        None,
    )
    .await;
    assert_status_ok(&go, &ours, "export started");

    let jobs = pool().await;
    let job_data = async |role: &str| -> serde_json::Value {
        let raw: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT data FROM jobs WHERE type = 'export_users_to_csv' AND data->>'role' = $1",
        )
        .bind(role)
        .fetch_optional(&jobs)
        .await
        .expect("the job reads");
        raw.expect("the job row exists")
    };
    let go_job = job_data(&role_go).await;
    let rs_job = job_data(&role_rs).await;
    assert_eq!(go_job["date_range"], "last_30_days");
    assert_eq!(rs_job["date_range"], "");
    for key in [
        "requesting_user_id",
        "hide_active",
        "hide_inactive",
        "team",
        "guest_filter",
        "end_at",
    ] {
        assert_eq!(go_job[key], rs_job[key], "job data {key}");
    }
    assert_eq!(rs_job["start_at"], "0", "all_time starts at zero");
    assert_ne!(
        go_job["start_at"], "0",
        "last_30_days starts thirty days back"
    );
    assert_eq!(
        go_job.as_object().map(|o| o.keys().collect::<Vec<_>>()),
        rs_job.as_object().map(|o| o.keys().collect::<Vec<_>>()),
        "the same nine keys"
    );

    // A planted pending job with these options is a duplicate for both servers.
    let role_dup = format!("{PREFIX}dup{}", now_millis());
    sqlx::query(
        "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status, progress, data) \
         VALUES ($1, 'export_users_to_csv', 0, $2, 0, 0, 'pending', 0, $3)",
    )
    .bind(planted_id("j"))
    .bind(now_millis())
    .bind(serde_json::json!({
        "requesting_user_id": logged_in_user_id(), "date_range": "", "role": role_dup,
        "team": "", "hide_active": "false", "hide_inactive": "false",
        "start_at": "0", "end_at": "0", "guest_filter": ""
    }))
    .execute(&jobs)
    .await
    .expect("the pending job is planted");
    let (go, ours) = licensed_both(&format!("role_filter={role_dup}")).await;
    assert_refusal(
        &go,
        &ours,
        400,
        "app.report.start_users_batch_export.job_exists",
        "duplicate",
    );

    // The DM from the system bot, off the request on both servers.
    if !bot_id.is_empty() {
        let started = |frames: &[serde_json::Value]| {
            frames.iter().any(|f| {
                f["event"] == "posted"
                    && f["data"]["post"]
                        .as_str()
                        .is_some_and(|p| p.contains("You've started an export"))
            })
        };
        assert!(
            go_probe
                .collect_until(Duration::from_secs(6), started)
                .await,
            "Go's DM: {:?}",
            go_probe.raw
        );
        assert!(
            rs_probe
                .collect_until(Duration::from_secs(6), started)
                .await,
            "our DM: {:?}",
            rs_probe.raw
        );
        let message_of = |probe: &SocketProbe| -> String {
            probe
                .events_named("posted")
                .iter()
                .filter_map(|f| f["data"]["post"].as_str().map(str::to_owned))
                .filter_map(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
                .filter(|p| p["user_id"] == bot_id)
                .filter_map(|p| p["message"].as_str().map(str::to_owned))
                .find(|m| m.contains("You've started an export"))
                .unwrap_or_default()
        };
        assert_eq!(
            message_of(&go_probe),
            "You've started an export of user data for the last 30 days. When the export is complete, a CSV file will be delivered to you in this direct message."
        );
        assert_eq!(
            message_of(&rs_probe),
            "You've started an export of user data for all time. When the export is complete, a CSV file will be delivered to you in this direct message."
        );
    }

    let _ = sqlx::query(
        "DELETE FROM jobs WHERE type = 'export_users_to_csv' AND data->>'role' LIKE 'mmrsprest%'",
    )
    .execute(&jobs)
    .await;
    unwind(&client, &admin, fx).await;
}

// ---------------------------------------------------------------------------------------------
// openDialog
// ---------------------------------------------------------------------------------------------

/// A trigger id minted with the stack's own key opens the dialog on both servers — the
/// `open_dialog` frame byte-identical — and each refusal ahead of the signature is served.
#[tokio::test]
async fn open_dialog_verifies_the_trigger_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "dlg").await;
    let reader = &fx.reader;
    let key = signing_key().await;
    let path = "/api/v4/actions/dialogs/open";
    let post = reqwest::Method::POST;
    let client_trigger = "mmrsprestclienttriggerid00";

    let dialog_body = |trigger: &str| {
        format!(
            r#"{{"trigger_id":"{trigger}","url":"http://example.invalid/dialog","dialog":{{"callback_id":"cb","title":"Report & <review>","introduction_text":"","icon_url":"","elements":[{{"display_name":"Why","name":"why","type":"text","subtype":"","default":"","placeholder":"","help_text":"","optional":false,"min_length":0,"max_length":0,"data_source":"","options":null,"multiselect":false}}],"submit_label":"Go","notify_on_cancel":true,"state":"s"}}}}"#
        )
    };

    let mut go_probe = SocketProbe::connect(GO, &reader.token).await;
    let mut rs_probe = SocketProbe::connect(RUST, &reader.token).await;
    // No session at all: `APIHandler`.
    let trigger = mint_trigger_id(&key, client_trigger, &reader.id, now_millis());
    let (go, ours) = both(
        &client,
        post.clone(),
        None,
        path,
        Some(dialog_body(&trigger).as_bytes()),
    )
    .await;
    assert_status_ok(&go, &ours, "open dialog");
    let go_events = events(&mut go_probe, "open_dialog").await;
    let rs_events = events(&mut rs_probe, "open_dialog").await;
    assert_eq!(
        go_events.len(),
        1,
        "Go opened one dialog: {:?}",
        go_probe.raw
    );
    assert_eq!(
        rs_events.len(),
        1,
        "we opened one dialog: {:?}",
        rs_probe.raw
    );
    assert_eq!(
        go_events[0]["data"]["dialog"], rs_events[0]["data"]["dialog"],
        "the dialog string"
    );
    assert!(
        go_events[0]["data"]["dialog"]
            .as_str()
            .is_some_and(|d| d.contains(client_trigger) && d.contains("\\u0026")),
        "Go's dialog carries the client trigger id, HTML-escaped: {:?}",
        go_events[0]["data"]["dialog"]
    );

    // The refusals, in `DecodeAndVerifyTriggerId`'s order.
    use base64::Engine as _;
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    let now = now_millis();
    let forged = {
        let other = p256::ecdsa::SigningKey::from_slice(&[9u8; 32]).unwrap();
        mint_trigger_id(&other, client_trigger, &reader.id, now)
    };
    let cases: [(&str, String, &str); 8] = [
        (
            "bad body",
            "{".to_owned(),
            "api.context.invalid_body_param.app_error",
        ),
        (
            "no url",
            r#"{"trigger_id":"x","url":""}"#.to_owned(),
            "api.context.invalid_body_param.app_error",
        ),
        (
            "base64",
            dialog_body("***"),
            "interactive_message.decode_trigger_id.base64_decode_failed",
        ),
        (
            "parts",
            dialog_body(&b64("a:b:c")),
            "interactive_message.decode_trigger_id.missing_data",
        ),
        (
            "expired",
            dialog_body(&mint_trigger_id(
                &key,
                client_trigger,
                &reader.id,
                now - 120_000,
            )),
            "interactive_message.decode_trigger_id.expired",
        ),
        (
            "signature base64",
            dialog_body(&b64(&format!("a:b:{now}:***"))),
            "interactive_message.decode_trigger_id.base64_decode_failed_signature",
        ),
        (
            "signature asn1",
            dialog_body(&b64(&format!("a:b:{now}:{}", b64("nope")))),
            "interactive_message.decode_trigger_id.signature_decode_failed",
        ),
        (
            "forged",
            dialog_body(&forged),
            "interactive_message.decode_trigger_id.verify_signature_failed",
        ),
    ];
    for (context, body, id) in &cases {
        let (go, ours) = both(
            &client,
            post.clone(),
            Some(&admin),
            path,
            Some(body.as_bytes()),
        )
        .await;
        assert_refusal(&go, &ours, 400, id, context);
    }

    unwind(&client, &admin, fx).await;
}

// ---------------------------------------------------------------------------------------------
// submitDialog, lookupDialog, executeDialogAction, doPostAction
// ---------------------------------------------------------------------------------------------

/// The three dialog submits refuse where Go refuses — the body, the URL, the channel, the
/// membership, the files, the query — and the outbound guard's refusal is the same 400 on both;
/// a `/plugins/` URL is Go's.
#[tokio::test]
async fn dialog_submits_refuse_where_go_refuses() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "sub").await;
    let reader = &fx.reader;
    let channel = &fx.public_channel;
    let post = reqwest::Method::POST;
    let submit = "/api/v4/actions/dialogs/submit";
    let lookup = "/api/v4/actions/dialogs/lookup";
    let execute = "/api/v4/actions/dialogs/execute";
    let refused = "http://127.0.0.1:9/integration";

    let admins_file = upload_file(
        &client,
        &admin,
        channel,
        "mmrsprest.txt",
        "text/plain",
        b"mmrsprest",
    )
    .await;
    let readers_file = upload_file(
        &client,
        &reader.token,
        channel,
        "mmrsprest2.txt",
        "text/plain",
        b"mmrsprest",
    )
    .await;
    let eleven: Vec<String> = (0..11).map(|n| format!("{PREFIX}file{n:0>13}")).collect();
    let eleven = serde_json::to_string(&eleven).unwrap();

    let cases: Vec<(&str, &str, &str, String, u16, &str)> = vec![
        (
            "submit bad body",
            submit,
            &reader.token,
            "{".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "submit no url",
            submit,
            &reader.token,
            r#"{"url":""}"#.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "submit bad channel",
            submit,
            &reader.token,
            format!(r#"{{"url":"{refused}","channel_id":"{ABSENT_ID}"}}"#),
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            "submit private channel",
            submit,
            &reader.token,
            format!(
                r#"{{"url":"{refused}","channel_id":"{}"}}"#,
                fx.private_channel
            ),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "submit refused by the guard",
            submit,
            &reader.token,
            format!(r#"{{"url":"{refused}","channel_id":"{channel}","submission":{{"a":"b"}}}}"#),
            400,
            "api.post.do_action.action_integration.app_error",
        ),
        (
            "submit not owned",
            submit,
            &reader.token,
            format!(
                r#"{{"url":"{refused}","channel_id":"{channel}","file_ids":["{admins_file}"]}}"#
            ),
            403,
            "app.submit_interactive_dialog.file_not_owned",
        ),
        (
            "submit unknown file",
            submit,
            &reader.token,
            format!(r#"{{"url":"{refused}","channel_id":"{channel}","file_ids":["{ABSENT_ID}"]}}"#),
            400,
            "app.submit_interactive_dialog.invalid_file_id",
        ),
        (
            "submit too many files",
            submit,
            &reader.token,
            format!(r#"{{"url":"{refused}","channel_id":"{channel}","file_ids":{eleven}}}"#),
            400,
            "app.submit_interactive_dialog.too_many_file_ids",
        ),
        (
            "submit smuggled file",
            submit,
            &reader.token,
            format!(
                r#"{{"url":"{refused}","channel_id":"{channel}","submission":{{"f":"x, {admins_file}"}}}}"#
            ),
            403,
            "app.submit_interactive_dialog.file_not_owned",
        ),
        (
            "submit own file, guard",
            submit,
            &reader.token,
            format!(
                r#"{{"url":"{refused}","channel_id":"{channel}","file_ids":["{readers_file}","{readers_file}"],"submission":{{"f":["{readers_file}"]}}}}"#
            ),
            400,
            "api.post.do_action.action_integration.app_error",
        ),
        (
            "lookup bad url",
            lookup,
            &reader.token,
            format!(r#"{{"url":"ftp://x","channel_id":"{channel}"}}"#),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "lookup traversal",
            lookup,
            &reader.token,
            format!(r#"{{"url":"/plugins/../x","channel_id":"{channel}"}}"#),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "lookup refused by the guard",
            lookup,
            &reader.token,
            format!(r#"{{"url":"{refused}","channel_id":"{channel}"}}"#),
            400,
            "api.post.do_action.action_integration.app_error",
        ),
        (
            "execute bad body",
            execute,
            &reader.token,
            "{".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "execute bad url",
            execute,
            &reader.token,
            format!(r#"{{"url":"nope","channel_id":"{channel}"}}"#),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "execute private channel",
            execute,
            &reader.token,
            format!(
                r#"{{"url":"{refused}","channel_id":"{}"}}"#,
                fx.private_channel
            ),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "execute refused by the guard",
            execute,
            &reader.token,
            format!(r#"{{"url":"{refused}","channel_id":"{channel}","context":{{"k":"v"}}}}"#),
            400,
            "api.post.do_action.action_integration.app_error",
        ),
        (
            "execute long context value",
            execute,
            &reader.token,
            format!(
                r#"{{"url":"{refused}","channel_id":"{channel}","context":{{"k":"{}"}}}}"#,
                "v".repeat(2049)
            ),
            400,
            "api.post.do_action.action_integration.app_error",
        ),
    ];
    for (context, path, token, body, status, id) in &cases {
        let (go, ours) = both(
            &client,
            post.clone(),
            Some(token),
            path,
            Some(body.as_bytes()),
        )
        .await;
        assert_refusal(&go, &ours, *status, id, context);
    }

    // A plugin path is the plugin host's, on both servers.
    let (go, ours) = both(
        &client,
        post.clone(),
        Some(&reader.token),
        lookup,
        Some(
            format!(r#"{{"url":"/plugins/mmrsprest/lookup","channel_id":"{channel}"}}"#).as_bytes(),
        ),
    )
    .await;
    assert!(!ours.2, "the plugin call is forwarded");
    assert_eq!(
        ours.0,
        go.0,
        "forwarded status: {} vs {}",
        text(&go.1),
        text(&ours.1)
    );

    unwind(&client, &admin, fx).await;
}

/// `doPostAction`'s gates: the body, the read permission, the query, the post, the action from
/// either format, the guard, an `openURL` block action's `goto_location`, and the cookie hand-off.
#[tokio::test]
async fn do_post_action_gates_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "act").await;
    let reader = &fx.reader;
    let post = reqwest::Method::POST;
    let author = logged_in_user_id();
    let created = now_millis() - 60_000;

    let plain = post_message(
        &client,
        &admin,
        &fx.public_channel,
        "mmrsprest actions",
        None,
    )
    .await;
    let private_post = post_message(
        &client,
        &admin,
        &fx.private_channel,
        "mmrsprest private",
        None,
    )
    .await;
    // A post with an attachment action pointing at a refused address, and one with an
    // `openURL` block action.
    let pool = pool().await;
    let plant = async |tag: &str, props: serde_json::Value| -> String {
        let id = planted_id(tag);
        sqlx::query(
            "INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, rootid, \
             originalid, message, type, props, hashtags, filenames, fileids, hasreactions, editat, \
             ispinned, remoteid) \
             VALUES ($1, $2, $2, 0, $3, $4, '', '', 'mmrsprest interactive', '', $5, '', '[]', '[]', \
             false, 0, false, NULL)",
        )
        .bind(&id)
        .bind(created)
        .bind(author)
        .bind(&fx.public_channel)
        .bind(props)
        .execute(&pool)
        .await
        .expect("the interactive post is planted");
        id
    };
    let with_attachment = plant("t", serde_json::json!({
        "attachments": [{"text": "pick", "actions": [{"id": "act-1", "name": "Go", "type": "button",
            "integration": {"url": "http://127.0.0.1:9/action", "context": {"k": "v"}}}]}]
    })).await;
    let with_blocks = plant("k", serde_json::json!({
        "mm_blocks_actions": {"open_1": {"type": "openURL", "url": "https://example.com/docs", "query": {"z": "1"}},
                              "ext_1": {"type": "external", "url": "http://127.0.0.1:9/ext"}}
    })).await;

    let path = |p: &str, a: &str| format!("/api/v4/posts/{p}/actions/{a}");
    let cases: Vec<ActionCase> = vec![
        (
            "bad body",
            path(&plain, "a1"),
            &reader.token,
            Some(b"{".to_vec()),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "trailing value",
            path(&plain, "a1"),
            &reader.token,
            Some(br#"{"query":{}}{"cookie":"x"}"#.to_vec()),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "unreadable post",
            path(&private_post, "a1"),
            &reader.token,
            None,
            403,
            "api.context.permissions.app_error",
        ),
        (
            "absent post",
            path(ABSENT_ID, "a1"),
            &reader.token,
            None,
            403,
            "api.context.permissions.app_error",
        ),
        (
            "query too long",
            path(&plain, "a1"),
            &reader.token,
            Some(format!(r#"{{"query":{{"k":"{}"}}}}"#, "v".repeat(2049)).into_bytes()),
            400,
            "api.post.do_action.query.app_error",
        ),
        (
            "no such action",
            path(&plain, "a1"),
            &reader.token,
            None,
            404,
            "api.post.do_action.action_id.app_error",
        ),
        (
            "no such block action",
            path(&plain, "a1"),
            &reader.token,
            Some(br#"{"integration_format":"mm_block"}"#.to_vec()),
            404,
            "api.post.do_action.action_id.app_error",
        ),
        (
            "attachment action, guard",
            path(&with_attachment, "act-1"),
            &reader.token,
            Some(b"{}".to_vec()),
            400,
            "api.post.do_action.action_integration.app_error",
        ),
        (
            "external block action, guard",
            path(&with_blocks, "ext_1"),
            &reader.token,
            Some(br#"{"integration_format":"block"}"#.to_vec()),
            400,
            "api.post.do_action.action_integration.app_error",
        ),
        (
            "block action as attachment",
            path(&with_blocks, "open_1"),
            &reader.token,
            Some(b"  \n".to_vec()),
            404,
            "api.post.do_action.action_id.app_error",
        ),
    ];
    for (context, path, token, body, status, id) in &cases {
        let (go, ours) = both(&client, post.clone(), Some(token), path, body.as_deref()).await;
        assert_refusal(&go, &ours, *status, id, context);
    }

    // The `openURL` block action answers without an integration call.
    let (go, ours) = both(
        &client,
        post.clone(),
        Some(&reader.token),
        &path(&with_blocks, "open_1"),
        Some(br#"{"integration_format":"card","query":{"q":"x"}}"#),
    )
    .await;
    assert_eq!(go.0, 200, "Go: {}", text(&go.1));
    assert_eq!(ours.0, 200, "ours: {}", text(&ours.1));
    assert!(ours.2, "answered here");
    assert_eq!(text(&go.1), text(&ours.1), "the goto answer");
    assert_eq!(
        text(&go.1),
        "{\"status\":\"OK\",\"trigger_id\":\"\",\"goto_location\":\"https://example.com/docs?z=1\"}\n"
    );

    // A cookie is Go's to decrypt.
    let (go, ours) = both(
        &client,
        post.clone(),
        Some(&reader.token),
        &path(&plain, "a1"),
        Some(br#"{"cookie":"bm90IGEgY29va2ll"}"#),
    )
    .await;
    assert!(!ours.2, "the cookie is forwarded");
    assert_eq!(ours.0, go.0);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&ours.1)
            .ok()
            .map(|v| v["id"].clone()),
        serde_json::from_slice::<serde_json::Value>(&go.1)
            .ok()
            .map(|v| v["id"].clone()),
        "the forwarded cookie failure"
    );

    // The action id class admits `_` and `-`; a `.` is a mux 404, Go's own.
    let (go, ours) = both(
        &client,
        post.clone(),
        Some(&reader.token),
        &path(&plain, "a.1"),
        None,
    )
    .await;
    assert_eq!(go.0, 404);
    assert!(!ours.2, "a segment outside Go's class is forwarded");
    assert_eq!(ours.0, 404);

    unwind(&client, &admin, fx).await;
}
