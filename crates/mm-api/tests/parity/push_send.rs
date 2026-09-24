//! Cross-server parity for push notifications: the same action through Go and through mm-api, the
//! two requests captured by the stack's push proxy (`common::push_proxy`), and compared once the
//! per-send `ack_id` and signature are masked (the signature is verified to be an ES256 JWT over
//! that ack id and device first).
//!
//! ```sh
//! scripts/parity.sh --test parity push_send
//! ```
//!
//! Each test owns a device id no other test uses: attaching a device revokes every other session
//! of the user holding the same id, and the proxy tells requests apart by it.

use std::time::Duration;

use crate::common;

use common::push_proxy::{PushRequest, normalize_push, push_proxy};
use common::{GO, RUST, client, go_minted_token, stack_enabled};

const WAIT: Duration = Duration::from_secs(10);

fn proxy() -> &'static common::push_proxy::PushProxy {
    push_proxy().expect("the push proxy could not bind this stack's port — who holds it?")
}

fn for_device<'a>(device: &'a str, kind: &'a str) -> impl Fn(&PushRequest) -> bool + 'a {
    move |r: &PushRequest| {
        let body = r.json();
        body["device_id"]
            .as_str()
            .is_some_and(|d| d.ends_with(device))
            && body["type"] == kind
    }
}

async fn attach_device(base: &str, token: &str, device_id: &str) {
    let response = client()
        .put(format!("{base}/api/v4/users/sessions/device"))
        .header("Authorization", format!("Bearer {token}"))
        .header("X-Requested-With", "XMLHttpRequest")
        .json(&serde_json::json!({ "device_id": device_id }))
        .send()
        .await
        .expect("the server answers");
    assert_eq!(response.status(), 200, "attaching {device_id}");
}

async fn post(base: &str, token: &str, path: &str, body: serde_json::Value) -> (u16, Vec<u8>) {
    let response = client()
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("X-Requested-With", "XMLHttpRequest")
        .json(&body)
        .send()
        .await
        .expect("the server answers");
    let status = response.status().as_u16();
    (
        status,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

/// Read the fixture channel through Go (zeroing its mentions), then lose focus on **both**
/// servers. `ActiveChannel` lives only in each process's status cache — never in the `Status`
/// row — and a user active in the channel is not pushed, so both caches must say "nowhere".
async fn read_and_lose_focus(viewer_token: &str, channel_id: &str, device: &str) {
    let (status, _) = post(
        GO,
        viewer_token,
        "/api/v4/channels/members/me/view",
        serde_json::json!({ "channel_id": channel_id }),
    )
    .await;
    assert_eq!(status, 200, "the read");
    for base in [GO, RUST] {
        let (status, _) = post(
            base,
            viewer_token,
            "/api/v4/channels/members/me/view",
            serde_json::json!({ "channel_id": "" }),
        )
        .await;
        assert_eq!(status, 200, "{base}: the focus loss");
    }
    let _ = proxy()
        .take(Duration::from_secs(2), for_device(device, "clear"))
        .await;
}

/// A reader with a "phone" session carrying `device` and a second session to read with, in a
/// fresh channel the admin posts into.
struct Fixture {
    admin: String,
    reader: common::PlainUser,
    viewer_token: String,
    channel_id: String,
    username: String,
}

async fn fixture(tag: &str, device: &str) -> Fixture {
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let reader = common::create_plain_user(&http, &admin, &team_id, tag).await;
    attach_device(GO, &reader.token, device).await;
    let viewer_token = common::login_plain_user(&http, tag).await;
    let channel_id = common::create_channel(&http, &admin, &team_id, tag).await;
    common::add_user_to_channel(&http, &admin, &channel_id, &reader.id).await;
    // Being added is itself a mention — the `system_add_to_channel` post names the added user —
    // so read the channel once, or the first post's badge counts it and the second's does not.
    // The add-to-channel post's notification pass runs after the add returns, so wait for its
    // increment to land before reading, or the read races it and the mention survives.
    if let Some(pool) = common::fixture_pool().await {
        for _ in 0..100 {
            let count: Option<i64> = sqlx::query_scalar(
                "SELECT mentioncount FROM channelmembers WHERE channelid = $1 AND userid = $2",
            )
            .bind(&channel_id)
            .bind(&reader.id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
            if count.unwrap_or(0) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
    let device_suffix = device.split(':').nth(1).unwrap_or(device).to_owned();
    read_and_lose_focus(&viewer_token, &channel_id, &device_suffix).await;
    Fixture {
        admin,
        reader,
        viewer_token,
        channel_id,
        username: common::plain_username(tag),
    }
}

/// Viewing a channel with an unread mention clears its notification on the user's **other**
/// devices: one `clear` per such channel, carrying the badge left afterwards.
#[tokio::test]
async fn viewing_a_mentioned_channel_clears_it_on_the_other_device() {
    if !stack_enabled() {
        return;
    }
    let proxy = proxy();
    let device = "android_rn:mmrs-push-clear";
    let f = fixture("pushclear", device).await;

    let mut clears = Vec::new();
    for base in [GO, RUST] {
        common::post_message(
            &client(),
            &f.admin,
            &f.channel_id,
            &format!("@{} look", f.username),
            None,
        )
        .await;
        let (status, body) = post(
            base,
            &f.viewer_token,
            "/api/v4/channels/members/me/view",
            serde_json::json!({ "channel_id": f.channel_id }),
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        let clear = proxy
            .take(WAIT, for_device("mmrs-push-clear", "clear"))
            .await
            .unwrap_or_else(|| panic!("{base} sent no clear"));
        clears.push(clear);
        proxy.discard(for_device("mmrs-push-clear", "message"));
    }
    assert_eq!(clears[0].path, "/api/v1/send_push");
    assert_eq!(clears[0].path, clears[1].path);
    assert_eq!(clears[0].method, clears[1].method);
    assert_eq!(
        normalize_push(clears[0].json()),
        normalize_push(clears[1].json()),
        "the two clears differ"
    );
    assert_eq!(clears[0].json()["channel_id"], f.channel_id.as_str());

    common::delete_channel(&client(), &f.admin, &f.channel_id).await;
    common::delete_plain_user(&client(), &f.admin, &f.reader.id).await;
}

/// `GET /system/ping?device_id=` sends a `test` push and reports what the proxy said.
#[tokio::test]
async fn the_pings_device_test_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let proxy = proxy();
    let mut pushes = Vec::new();
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let response = client()
            .get(format!(
                "{base}/api/v4/system/ping?device_id=apple_rn:mmrs-push-ping"
            ))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status(), 200, "{base}");
        let body: serde_json::Value = response.json().await.expect("JSON");
        bodies.push(body["CanReceiveNotifications"].clone());
        pushes.push(
            proxy
                .take(WAIT, for_device("mmrs-push-ping", "test"))
                .await
                .unwrap_or_else(|| panic!("{base} sent no test push")),
        );
    }
    assert_eq!(bodies[0], "true");
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(pushes[0].json(), pushes[1].json(), "the test pushes differ");

    // A device the proxy reports removed is `"false"`, and one it fails is `"unknown"`.
    for (device, expected) in [
        ("apple_rn:mmrs-remove", "false"),
        ("apple_rn:mmrs-fail", "unknown"),
    ] {
        let mut answers = Vec::new();
        for base in [GO, RUST] {
            let body: serde_json::Value = client()
                .get(format!("{base}/api/v4/system/ping?device_id={device}"))
                .send()
                .await
                .expect("answers")
                .json()
                .await
                .expect("JSON");
            answers.push(body["CanReceiveNotifications"].clone());
        }
        assert_eq!(answers[0], expected, "{device}");
        assert_eq!(answers[0], answers[1], "{device}");
        proxy.discard(for_device(device.trim_start_matches("apple_rn:"), "test"));
    }
}

/// `POST /notifications/ack` re-encodes the ack to the proxy's `/api/v1/ack` and answers OK.
#[tokio::test]
async fn an_ack_reaches_the_proxy_as_go_sends_it() {
    if !stack_enabled() {
        return;
    }
    let proxy = proxy();
    let http = client();
    let token = go_minted_token(&http).await;
    let ack = serde_json::json!({
        "id": "mmrsackmmrsackmmrsackmmrsa",
        "received_at": 1_700_000_000_123_i64,
        "platform": "android",
        "type": "clear",
        "extra": "dropped",
    });
    let mut forwarded = Vec::new();
    let mut answers = Vec::new();
    for base in [GO, RUST] {
        answers.push(post(base, &token, "/api/v4/notifications/ack", ack.clone()).await);
        forwarded.push(
            proxy
                .take(WAIT, |r| {
                    r.path == "/api/v1/ack" && r.json()["id"] == "mmrsackmmrsackmmrsackmmrsa"
                })
                .await
                .unwrap_or_else(|| panic!("{base} forwarded no ack")),
        );
    }
    assert_eq!(answers[0], answers[1], "the responses differ");
    assert_eq!(answers[0].0, 200);
    assert_eq!(
        forwarded[0].body, forwarded[1].body,
        "the acks differ byte for byte"
    );
}

/// Marking a post unread sends an `update_badge` to every device of the user.
#[tokio::test]
async fn marking_a_post_unread_updates_the_badge() {
    if !stack_enabled() {
        return;
    }
    let proxy = proxy();
    let device = "android_rn:mmrs-push-badge";
    let f = fixture("pushbadge", device).await;
    let post_id = common::post_message(
        &client(),
        &f.admin,
        &f.channel_id,
        &format!("@{} badge", f.username),
        None,
    )
    .await;
    proxy.discard(for_device("mmrs-push-badge", "message"));

    let mut badges = Vec::new();
    for base in [GO, RUST] {
        let (status, body) = post(
            base,
            &f.viewer_token,
            &format!("/api/v4/users/{}/posts/{post_id}/set_unread", f.reader.id),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        badges.push(
            proxy
                .take(WAIT, for_device("mmrs-push-badge", "update_badge"))
                .await
                .unwrap_or_else(|| panic!("{base} sent no badge update")),
        );
    }
    assert_eq!(
        normalize_push(badges[0].json()),
        normalize_push(badges[1].json()),
        "the badge updates differ"
    );

    common::delete_channel(&client(), &f.admin, &f.channel_id).await;
    common::delete_plain_user(&client(), &f.admin, &f.reader.id).await;
}

/// Post `message` through `base` as the admin, then take the `message` push for `device`, with the
/// channel read again through Go afterwards so the next post starts from the same badge.
async fn post_and_take_push(f: &Fixture, base: &str, message: &str, device: &str) -> PushRequest {
    let proxy = proxy();
    let (status, body) = post(
        base,
        &f.admin,
        "/api/v4/posts",
        serde_json::json!({ "channel_id": f.channel_id, "message": message }),
    )
    .await;
    assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&body));
    let push = proxy
        .take(WAIT, for_device(device, "message"))
        .await
        .unwrap_or_else(|| panic!("{base} sent no message push"));
    read_and_lose_focus(&f.viewer_token, &f.channel_id, device).await;
    push
}

/// The post's own id differs between the two posts; everything else must not.
fn without_post_id(request: &PushRequest) -> serde_json::Value {
    let mut body = normalize_push(request.json());
    body["post_id"] = serde_json::Value::from("<post>");
    body
}

/// A post that @-mentions a member with a device pushes them the full-contents message.
#[tokio::test]
async fn a_mention_pushes_the_same_message() {
    if !stack_enabled() {
        return;
    }
    let device = "android_rn:mmrs-push-mention";
    let f = fixture("pushmention", device).await;
    let message = format!("@{} are you there", f.username);
    let go = post_and_take_push(&f, GO, &message, "mmrs-push-mention").await;
    let rs = post_and_take_push(&f, RUST, &message, "mmrs-push-mention").await;
    assert_eq!(
        without_post_id(&go),
        without_post_id(&rs),
        "the mention pushes differ"
    );
    assert!(
        go.json()["message"]
            .as_str()
            .unwrap_or_default()
            .contains("are you there"),
        "the full contents: {}",
        go.json()
    );

    common::delete_channel(&client(), &f.admin, &f.channel_id).await;
    common::delete_plain_user(&client(), &f.admin, &f.reader.id).await;
}

/// A member whose `push` notify prop is `all` is pushed for a post that does not mention them.
#[tokio::test]
async fn an_all_activity_member_is_pushed_without_a_mention() {
    if !stack_enabled() {
        return;
    }
    let device = "android_rn:mmrs-push-all";
    let f = fixture("pushall", device).await;
    let response = client()
        .put(format!("{GO}/api/v4/users/{}/patch", f.reader.id))
        .header("Authorization", format!("Bearer {}", f.admin))
        .json(&serde_json::json!({ "notify_props": {
            "push": "all", "push_status": "online", "email": "false", "desktop": "mention",
            "channel": "true", "comments": "never", "mention_keys": "", "first_name": "false",
        }}))
        .send()
        .await
        .expect("answers");
    assert_eq!(response.status(), 200, "patching notify props");

    let go = post_and_take_push(&f, GO, "nothing personal", "mmrs-push-all").await;
    let rs = post_and_take_push(&f, RUST, "nothing personal", "mmrs-push-all").await;
    assert_eq!(
        without_post_id(&go),
        without_post_id(&rs),
        "the all-activity pushes differ"
    );

    common::delete_channel(&client(), &f.admin, &f.channel_id).await;
    common::delete_plain_user(&client(), &f.admin, &f.reader.id).await;
}

/// Reading **on the phone** clears nothing on it — the session that did the reading is skipped —
/// and an unread channel with no mention sends no clear at all.
#[tokio::test]
async fn no_clear_goes_to_the_reading_session_or_for_a_channel_without_a_mention() {
    if !stack_enabled() {
        return;
    }
    let proxy = proxy();
    let device = "android_rn:mmrs-push-noclear";
    let f = fixture("pushnoclear", device).await;

    for base in [GO, RUST] {
        // A mention, read on the phone itself: the clear would go only to the phone, which is
        // the reading session.
        common::post_message(
            &client(),
            &f.admin,
            &f.channel_id,
            &format!("@{} hi", f.username),
            None,
        )
        .await;
        let _ = proxy
            .take(WAIT, for_device("mmrs-push-noclear", "message"))
            .await;
        let (status, _) = post(
            base,
            &f.reader.token,
            "/api/v4/channels/members/me/view",
            serde_json::json!({ "channel_id": f.channel_id }),
        )
        .await;
        assert_eq!(status, 200, "{base}");
        assert!(
            proxy
                .take(
                    Duration::from_secs(2),
                    for_device("mmrs-push-noclear", "clear")
                )
                .await
                .is_none(),
            "{base} cleared the session that did the reading"
        );
        read_and_lose_focus(&f.viewer_token, &f.channel_id, "mmrs-push-noclear").await;

        // Unread, not mentioned, read on the other session: nothing to clear.
        common::post_message(&client(), &f.admin, &f.channel_id, "no names here", None).await;
        let (status, _) = post(
            base,
            &f.viewer_token,
            "/api/v4/channels/members/me/view",
            serde_json::json!({ "channel_id": f.channel_id }),
        )
        .await;
        assert_eq!(status, 200, "{base}");
        assert!(
            proxy
                .take(
                    Duration::from_secs(2),
                    for_device("mmrs-push-noclear", "clear")
                )
                .await
                .is_none(),
            "{base} cleared a channel that held no mention"
        );
        read_and_lose_focus(&f.viewer_token, &f.channel_id, "mmrs-push-noclear").await;
    }

    common::delete_channel(&client(), &f.admin, &f.channel_id).await;
    common::delete_plain_user(&client(), &f.admin, &f.reader.id).await;
}
