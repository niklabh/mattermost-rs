//! The acknowledgement pair on the **licensed** pair — `POST` and
//! `DELETE /api/v4/users/{user_id}/posts/{post_id}/ack` past `MinimumProfessionalLicense`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity post_acks_licensed
//! ```
//!
//! `parity::post_acks` measures the refusal on the stack's unlicensed pair; this measures
//! everything behind it against the Enterprise-licensed oracle (`common::licensed`): the gate
//! order past the licence, the write and its body, the un-acknowledgement and its four
//! refusals, and the two websocket events each write publishes.
//!
//! # Both servers write the same table
//!
//! An acknowledgement is one `(post, user)` row, so a `POST` through Go and then through us is
//! an upsert on the second call — `AcknowledgedAt` moves, nothing else. The tests use that:
//! whichever server acknowledged, the other can un-acknowledge, and both must agree that the
//! second un-acknowledgement finds nothing.
//!
//! # `acknowledged_at` is the one field that cannot be compared
//!
//! Each server stamps its own `GetMillis()`, so the bodies are compared with that key removed and
//! the key itself is asserted to be a recent timestamp on both.

use std::time::Duration;

use crate::common;

use common::{
    SocketProbe, a_team_and_channel_the_user_is_in, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_channel_typed,
    create_plain_user, delete_channel, delete_plain_user, go_minted_token, licensed,
    logged_in_user_id, post_message, request_raw, stack_enabled,
};

const ABSENT_ID: &str = "mmrsnosuchpostmmrsnosuchp1";

fn ack_path(user_id: &str, post_id: &str) -> String {
    format!("/api/v4/users/{user_id}/posts/{post_id}/ack")
}

async fn post_ack(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> (u16, Vec<u8>, Option<String>) {
    request_raw(
        client,
        base,
        reqwest::Method::POST,
        Some(token),
        path,
        Some(b"{}"),
    )
    .await
}

async fn delete_ack(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> (u16, Vec<u8>, Option<String>) {
    request_raw(
        client,
        base,
        reqwest::Method::DELETE,
        Some(token),
        path,
        None,
    )
    .await
}

/// The body with `acknowledged_at` removed, and the removed value.
fn without_timestamp(body: &[u8]) -> (serde_json::Value, i64) {
    let mut value: serde_json::Value = serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("an acknowledgement: {e}: {}", String::from_utf8_lossy(body)));
    let at = value["acknowledged_at"]
        .as_i64()
        .expect("acknowledged_at is a number");
    value.as_object_mut().unwrap().remove("acknowledged_at");
    (value, at)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

struct Fixture {
    reader: common::PlainUser,
    channel: String,
    /// The admin's root post in `channel`, which the reader can read.
    root: String,
    /// A **private** channel the reader is not in, with one post in it.
    private_post: String,
}

async fn fixture(client: &reqwest::Client, admin: &str, tag: &str) -> Fixture {
    let (team, _) = a_team_and_channel_the_user_is_in(client, admin).await;
    let reader = create_plain_user(client, admin, &team, tag).await;
    let channel = create_channel(client, admin, &team, tag).await;
    add_user_to_channel(client, admin, &channel, &reader.id).await;
    let root = post_message(client, admin, &channel, "mmrs licensed ack root", None).await;
    // **Private**, or `SessionHasPermissionToReadPost` grants the read through
    // `read_public_channel` on the team and the 403 never comes — measured: the first version of
    // this fixture used an open channel and Go answered 200.
    let closed = create_channel_typed(client, admin, &team, &format!("{tag}p"), "P").await;
    let private_post =
        post_message(client, admin, &closed, "mmrs licensed ack private", None).await;
    Fixture {
        reader,
        channel,
        root,
        private_post,
    }
}

async fn unwind(client: &reqwest::Client, admin: &str, fixture: Fixture) {
    delete_plain_user(client, admin, &fixture.reader.id).await;
}

/// A `POST` through each licensed server, a `DELETE` through the other, and the bodies agree.
#[tokio::test]
async fn an_acknowledgement_round_trips_through_both_licensed_servers() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let fixture = fixture(&client, &admin, "lack").await;
    let path = ack_path(me, &fixture.root);

    // Go acknowledges, we un-acknowledge — and Go then finds nothing, which is what proves our
    // delete zeroed the row rather than answering 200 over an untouched one (a mutation that did
    // exactly that survived until this read existed).
    let (go_status, go_body, _) = post_ack(&client, &pair.go, &admin, &path).await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    let (rs_status, rs_body, served) = delete_ack(&client, &pair.rust, &admin, &path).await;
    assert_eq!(served.as_deref(), Some("rust"), "the DELETE is ours");
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(rs_body, br#"{"status":"OK"}"#, "ReturnStatusOK, no newline");
    let (go_status, go_after, _) = delete_ack(&client, &pair.go, &admin, &path).await;
    assert_eq!(
        go_status,
        404,
        "Go finds nothing after our delete: {}",
        String::from_utf8_lossy(&go_after)
    );

    // We acknowledge, Go un-acknowledges. The write also stamps `Posts.UpdateAt` — `updatePost`
    // in the store's transaction — which the oracle reads back.
    let before = post_update_at(&client, &pair.go, &admin, &fixture.root).await;
    let (rs_status, rs_body, served) = post_ack(&client, &pair.rust, &admin, &path).await;
    assert!(
        post_update_at(&client, &pair.go, &admin, &fixture.root).await > before,
        "the post's update_at moves with the acknowledgement"
    );
    assert_eq!(served.as_deref(), Some("rust"), "the POST is ours");
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert!(
        !rs_body.ends_with(b"\n"),
        "json.Marshal + w.Write: no newline"
    );
    let (go_status, go_del, _) = delete_ack(&client, &pair.go, &admin, &path).await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_del));
    assert_eq!(go_del, rs_body_status_ok(), "Go's DELETE body");

    // The two POST bodies agree on everything but the millisecond.
    let (go_value, go_at) = without_timestamp(&go_body);
    let (rs_value, rs_at) = without_timestamp(&rs_body);
    assert_eq!(
        rs_value, go_value,
        "the acknowledgement, minus its timestamp"
    );
    assert_eq!(rs_value["user_id"], me);
    assert_eq!(rs_value["post_id"], fixture.root);
    assert_eq!(rs_value["channel_id"], fixture.channel);
    assert!(
        rs_value.get("remote_id").is_none(),
        "remote_id is nil and `omitempty`, so absent: {rs_value}"
    );
    let now = now_ms();
    for (who, at) in [("go", go_at), ("rust", rs_at)] {
        assert!(
            now - at < 60_000 && at <= now + 1_000,
            "{who}: acknowledged_at is a fresh timestamp: {at} vs {now}"
        );
    }

    // Both find nothing to un-acknowledge now, with the same 404.
    let (go_status, go_body, _) = delete_ack(&client, &pair.go, &admin, &path).await;
    let (rs_status, rs_body, served) = delete_ack(&client, &pair.rust, &admin, &path).await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 404, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "second DELETE");
    assert_eq!(parsed["id"], "app.acknowledgement.get.app_error");

    unwind(&client, &admin, fixture).await;
}

/// The post's `update_at`, read through the oracle — which caches posts, so invalidate first.
async fn post_update_at(client: &reqwest::Client, base: &str, token: &str, post_id: &str) -> i64 {
    let (_, _, _) = request_raw(
        client,
        base,
        reqwest::Method::POST,
        Some(token),
        "/api/v4/caches/invalidate",
        None,
    )
    .await;
    let (status, body, _) = request_raw(
        client,
        base,
        reqwest::Method::GET,
        Some(token),
        &format!("/api/v4/posts/{post_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let post: serde_json::Value = serde_json::from_slice(&body).unwrap();
    post["update_at"].as_i64().expect("update_at")
}

fn rs_body_status_ok() -> Vec<u8> {
    br#"{"status":"OK"}"#.to_vec()
}

/// `me` resolves to the session's user in `RequireUserId`, on both.
#[tokio::test]
async fn me_is_the_caller_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let fixture = fixture(&client, &admin, "lackme").await;
    let path = ack_path("me", &fixture.root);

    let (rs_status, rs_body, served) = post_ack(&client, &pair.rust, &admin, &path).await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let (rs_value, _) = without_timestamp(&rs_body);
    assert_eq!(
        rs_value["user_id"],
        logged_in_user_id(),
        "`me` became the caller"
    );
    let (go_status, go_body, _) = delete_ack(&client, &pair.go, &admin, &path).await;
    assert_eq!(
        go_status,
        200,
        "Go un-acknowledges what we wrote under `me`: {}",
        String::from_utf8_lossy(&go_body)
    );

    unwind(&client, &admin, fixture).await;
}

/// The checks past the licence, each answered the same on both licensed servers and each by a
/// **different** refusal: `RequirePostId` (400), `RequireUserId` (400), `edit_other_users`
/// (403), and `read_channel_content` (403) — which is what a post that does not exist gets for a
/// plain user, while an administrator gets past that gate and meets `GetSinglePost`'s 404.
#[tokio::test]
async fn the_gates_past_the_licence_refuse_identically() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let fixture = fixture(&client, &admin, "lackgate").await;

    let cases: Vec<(&str, String, &str, u16, &str)> = vec![
        (
            "RequirePostId",
            ack_path(me, "abc"),
            &admin,
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "RequireUserId",
            ack_path("xyz", &fixture.root),
            &admin,
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "edit_other_users: the reader acting for the admin",
            ack_path(me, &fixture.root),
            &fixture.reader.token,
            403,
            "api.context.permissions.app_error",
        ),
        (
            "read_channel_content: a channel the reader is not in",
            ack_path(&fixture.reader.id, &fixture.private_post),
            &fixture.reader.token,
            403,
            "api.context.permissions.app_error",
        ),
        // `SessionHasPermissionToReadPost` cannot find the post's channel and falls back to the
        // caller's own `read_channel_content` — which a system administrator holds — so the
        // admin passes the gate and it is `GetSinglePost` that refuses, with a **404**. Measured:
        // the first version of this case expected the reader's 403.
        (
            "an absent post, as the admin: GetSinglePost's 404",
            ack_path(me, ABSENT_ID),
            &admin,
            404,
            "app.post.get.app_error",
        ),
        (
            "an absent post, as the reader: the read gate's 403",
            ack_path(&fixture.reader.id, ABSENT_ID),
            &fixture.reader.token,
            403,
            "api.context.permissions.app_error",
        ),
    ];
    for (why, path, token, status, id) in &cases {
        for (method, label) in [
            (reqwest::Method::POST, "POST"),
            (reqwest::Method::DELETE, "DELETE"),
        ] {
            let body = if method == reqwest::Method::POST {
                Some(&b"{}"[..])
            } else {
                None
            };
            let (go_status, go_body, _) =
                request_raw(&client, &pair.go, method.clone(), Some(token), path, body).await;
            let (rs_status, rs_body, served) =
                request_raw(&client, &pair.rust, method, Some(token), path, body).await;
            let context = format!("{label} {why}");
            assert_eq!(served.as_deref(), Some("rust"), "{context}: answered here");
            assert_eq!(
                go_status,
                *status,
                "{context}: {}",
                String::from_utf8_lossy(&go_body)
            );
            assert_eq!(rs_status, go_status, "{context}");
            let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
            assert_eq!(parsed["id"], *id, "{context}");
        }
    }

    unwind(&client, &admin, fixture).await;
}

/// An archived channel is a **403** on both halves, with the save's id on `POST` and the
/// delete's id on `DELETE` — checked before the acknowledgement is looked up, so a `DELETE`
/// with nothing to delete still says "archived", not "not found".
#[tokio::test]
async fn an_archived_channel_is_a_403_with_each_halfs_own_id() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let channel = create_channel(&client, &admin, &team, "lackarch").await;
    let post = post_message(
        &client,
        &admin,
        &channel,
        "mmrs licensed ack archived",
        None,
    )
    .await;
    delete_channel(&client, &admin, &channel).await;
    let path = ack_path(me, &post);

    let (go_status, go_body, _) = post_ack(&client, &pair.go, &admin, &path).await;
    let (rs_status, rs_body, served) = post_ack(&client, &pair.rust, &admin, &path).await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "POST archived");
    assert_eq!(
        parsed["id"],
        "api.acknowledgement.save.archived_channel.app_error"
    );

    let (go_status, go_body, _) = delete_ack(&client, &pair.go, &admin, &path).await;
    let (rs_status, rs_body, served) = delete_ack(&client, &pair.rust, &admin, &path).await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "DELETE archived");
    assert_eq!(
        parsed["id"],
        "api.acknowledgement.delete.archived_channel.app_error"
    );
}

/// Five minutes after the acknowledgement, un-acknowledging is a **403** — decided on the stored
/// `AcknowledgedAt`, so the row is planted six minutes old and both servers refuse it.
#[tokio::test]
async fn an_acknowledgement_older_than_five_minutes_cannot_be_removed() {
    if !stack_enabled() {
        return;
    }
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let fixture = fixture(&client, &admin, "lackold").await;
    let path = ack_path(me, &fixture.root);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    sqlx::query(
        "INSERT INTO postacknowledgements (postid, userid, channelid, acknowledgedat)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (postid, userid) DO UPDATE SET acknowledgedat = $4",
    )
    .bind(&fixture.root)
    .bind(me)
    .bind(&fixture.channel)
    .bind(now_ms() - 6 * 60 * 1000)
    .execute(&pool)
    .await
    .expect("plants an old acknowledgement");

    let (go_status, go_body, _) = delete_ack(&client, &pair.go, &admin, &path).await;
    let (rs_status, rs_body, served) = delete_ack(&client, &pair.rust, &admin, &path).await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "old DELETE");
    assert_eq!(
        parsed["id"],
        "api.acknowledgement.delete.deadline.app_error"
    );

    // A fresh acknowledgement replaces the timestamp, and then it can go.
    let (status, body, _) = post_ack(&client, &pair.rust, &admin, &path).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let (status, body, _) = delete_ack(&client, &pair.go, &admin, &path).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));

    unwind(&client, &admin, fixture).await;
}

/// Each write publishes two events on both servers: `post_acknowledgement_added` (or
/// `_removed`) with the acknowledgement as a JSON **string** in `data`, and a `post_edited`
/// carrying the post. Both are channel-scoped, so the admin's own socket receives them.
#[tokio::test]
async fn both_writes_publish_the_acknowledgement_event_and_a_post_edited() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let fixture = fixture(&client, &admin, "lackws").await;
    let path = ack_path(me, &fixture.root);
    let _stream = common::BROADCAST_STREAM.lock().await;

    for (base, label) in [(pair.go.clone(), "go"), (pair.rust.clone(), "rust")] {
        let mut probe = SocketProbe::connect(&base, &admin).await;

        let (status, body, _) = post_ack(&client, &base, &admin, &path).await;
        assert_eq!(status, 200, "{label}: {}", String::from_utf8_lossy(&body));
        let expected_ack = {
            let (value, _) = without_timestamp(&body);
            value
        };
        let post_id = fixture.root.clone();
        let channel = fixture.channel.clone();
        let found = probe
            .collect_until(Duration::from_secs(5), |frames| {
                has_ack_event(frames, "post_acknowledgement_added", &post_id, &channel)
                    && has_post_edited(frames, &post_id)
            })
            .await;
        assert!(
            found,
            "{label}: post_acknowledgement_added and post_edited: {:?}",
            probe.frames()
        );
        let frames = probe.frames();
        let event = ack_event(&frames, "post_acknowledgement_added", &post_id).unwrap();
        let (mut carried, _) = without_timestamp(
            event["data"]["acknowledgement"]
                .as_str()
                .unwrap()
                .as_bytes(),
        );
        carried.as_object_mut().unwrap().remove("remote_id");
        assert_eq!(
            carried, expected_ack,
            "{label}: the event carries the acknowledgement"
        );
        assert_eq!(
            event["broadcast"]["channel_id"], channel,
            "{label}: channel-scoped"
        );

        let mut probe = SocketProbe::connect(&base, &admin).await;
        let (status, body, _) = delete_ack(&client, &base, &admin, &path).await;
        assert_eq!(status, 200, "{label}: {}", String::from_utf8_lossy(&body));
        let found = probe
            .collect_until(Duration::from_secs(5), |frames| {
                has_ack_event(frames, "post_acknowledgement_removed", &post_id, &channel)
                    && has_post_edited(frames, &post_id)
            })
            .await;
        assert!(
            found,
            "{label}: post_acknowledgement_removed and post_edited: {:?}",
            probe.frames()
        );
    }

    unwind(&client, &admin, fixture).await;
}

fn ack_event<'a>(
    frames: &'a [serde_json::Value],
    event: &str,
    post_id: &str,
) -> Option<&'a serde_json::Value> {
    frames.iter().find(|f| {
        f["event"] == event
            && f["data"]["acknowledgement"]
                .as_str()
                .is_some_and(|s| s.contains(post_id))
    })
}

fn has_ack_event(frames: &[serde_json::Value], event: &str, post_id: &str, channel: &str) -> bool {
    ack_event(frames, event, post_id).is_some_and(|f| f["broadcast"]["channel_id"] == channel)
}

fn has_post_edited(frames: &[serde_json::Value], post_id: &str) -> bool {
    frames.iter().any(|f| {
        f["event"] == "post_edited"
            && f["data"]["post"]
                .as_str()
                .is_some_and(|s| s.contains(post_id))
    })
}
