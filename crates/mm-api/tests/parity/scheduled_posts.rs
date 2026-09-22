//! The four scheduled-post routes on the **licensed** pair — everything behind
//! `requireScheduledPostsEnabled` (api4/scheduled_post.go).
//!
//! ```sh
//! scripts/parity.sh --test parity scheduled_posts
//! ```
//!
//! `parity::gated_families` measures the two refusals in front of the gate on the unlicensed
//! pair; this measures what sits behind it against the Enterprise-licensed oracle
//! (`common::licensed`): the writes and their bodies, the list with and without its direct
//! channels, the refusals in Go's order, the `repeat_type` presence rule on update, and the three
//! websocket events.
//!
//! # Each test acts as a user of its own
//!
//! A scheduled post belongs to one user and the list is keyed by team, so every test makes a
//! plain user in a team of its own. Nothing a sibling test writes can then reach a list or an
//! event this one compares.
//!
//! # What is tokenised
//!
//! Each server mints its own id and stamps its own milliseconds, so a write's answer is compared
//! with `id`, `create_at` and `update_at` replaced by tokens that still say empty-or-not. Reads of
//! rows both servers can see are compared byte for byte.

use std::time::Duration;

use serde_json::{Value as Json, json};

use crate::common;

use common::{
    LicensedPair, PlainUser, SocketProbe, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, create_team, delete_plain_user,
    go_minted_token, licensed, logged_in_user_id, request_raw, stack_enabled,
};

/// Well-formed, and nobody's.
const ABSENT_ID: &str = "mmrsnoscheduledpostmmrsnos";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn m(s: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(s.as_bytes()).expect("a method")
}

/// One request to each licensed server; the Rust one must be served here.
async fn both(
    client: &reqwest::Client,
    pair: &LicensedPair,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (gs, gb, _) = request_raw(client, &pair.go, m(method), Some(token), path, body).await;
    let (rs, rb, served) =
        request_raw(client, &pair.rust, m(method), Some(token), path, body).await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "{method} {path} was forwarded"
    );
    ((gs, gb), (rs, rb))
}

/// One request to one server of the pair, asserting who served it.
async fn one(
    client: &reqwest::Client,
    base: &str,
    rust: bool,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> (u16, Vec<u8>) {
    let (status, bytes, served) =
        request_raw(client, base, m(method), Some(token), path, body).await;
    if rust {
        assert_eq!(
            served.as_deref(),
            Some("rust"),
            "{method} {path} was forwarded"
        );
    }
    (status, bytes)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parse(bytes: &[u8]) -> Json {
    serde_json::from_slice(bytes).unwrap_or_else(|e| panic!("not JSON ({e}): {}", text(bytes)))
}

/// `keys` replaced by a token that keeps empty-or-not (strings) and zero-or-not (numbers).
fn tokenised(bytes: &[u8], keys: &[&str]) -> Json {
    let mut value = parse(bytes);
    if let Some(object) = value.as_object_mut() {
        for key in keys {
            if let Some(entry) = object.get_mut(*key) {
                let token = match &*entry {
                    Json::String(s) if s.is_empty() => json!(""),
                    Json::String(_) => json!("<id>"),
                    Json::Number(n) if n.as_i64() == Some(0) => json!(0),
                    Json::Number(_) => json!("<set>"),
                    other => other.clone(),
                };
                *entry = token;
            }
        }
    }
    value
}

/// The keys each server fills in for itself on a write.
const MINTED: [&str; 3] = ["id", "create_at", "update_at"];

struct Fixture {
    team: String,
    channel: String,
    user: PlainUser,
}

async fn fixture(client: &reqwest::Client, admin: &str, tag: &str) -> Fixture {
    let team = create_team(client, admin, tag).await;
    let user = create_plain_user(client, admin, &team, tag).await;
    let channel = create_channel(client, admin, &team, tag).await;
    common::add_user_to_channel(client, admin, &channel, &user.id).await;
    Fixture {
        team,
        channel,
        user,
    }
}

/// Every scheduled post the fixture's user owns goes, whatever the test left behind — including
/// the rows a refusal must not have written, and any a failed assertion stranded.
async fn unwind(client: &reqwest::Client, admin: &str, fixture: Fixture) {
    if let Some(pool) = common::fixture_pool().await {
        let _ = sqlx::query("DELETE FROM scheduledposts WHERE userid = $1")
            .bind(&fixture.user.id)
            .execute(&pool)
            .await;
    }
    delete_plain_user(client, admin, &fixture.user.id).await;
}

fn body(value: Json) -> Vec<u8> {
    serde_json::to_vec(&value).expect("a body")
}

/// Create, list, update and delete, each crossing the pair: what one server wrote the other
/// reads, updates and deletes, and every answer agrees.
#[tokio::test]
async fn a_scheduled_post_round_trips_across_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "schedrt").await;
    let token = fx.user.token.clone();
    let list = format!("/api/v4/posts/scheduled/team/{}", fx.team);

    // Nothing yet: an empty list is `[]` under the team's key, never `null`.
    let ((gs, gb), (rs, rb)) = both(&client, &pair, &token, "GET", &list, None).await;
    assert_eq!((gs, rs), (200, 200), "{}", text(&gb));
    assert_eq!(text(&rb), text(&gb), "the empty list");
    assert_eq!(text(&gb), format!("{{\"{}\":[]}}\n", fx.team));

    // Create one on each server. The message and props carry HTML, which `json.Marshal` escapes.
    let at = now_ms() + 3_600_000;
    let create = |scheduled_at: i64| {
        body(json!({
            "channel_id": fx.channel,
            "message": "mmrs <b>scheduled</b> & more",
            "props": {"mmrs": "<x>"},
            "priority": {"priority": "important"},
            "scheduled_at": scheduled_at,
            "user_id": "somebodyelsesidsomebodyels",
            "create_at": 1234,
            "delete_at": 99,
        }))
    };
    let (gs, go_created) = one(
        &client,
        &pair.go,
        false,
        &token,
        "POST",
        "/api/v4/posts/schedule",
        Some(&create(at + 60_000)),
    )
    .await;
    let (rs, rs_created) = one(
        &client,
        &pair.rust,
        true,
        &token,
        "POST",
        "/api/v4/posts/schedule",
        Some(&create(at)),
    )
    .await;
    assert_eq!(
        (gs, rs),
        (201, 201),
        "Go {} / Rust {}",
        text(&go_created),
        text(&rs_created)
    );
    assert!(
        rs_created.ends_with(b"\n") && go_created.ends_with(b"\n"),
        "Encode's newline"
    );
    assert!(
        text(&rs_created).contains("\\u003cb\\u003e"),
        "HTML-escaped: {}",
        text(&rs_created)
    );
    let mut keys = MINTED.to_vec();
    keys.push("scheduled_at");
    assert_eq!(
        tokenised(&rs_created, &keys),
        tokenised(&go_created, &keys),
        "the created post"
    );
    let go_post = parse(&go_created);
    let rs_post = parse(&rs_created);
    assert_eq!(
        rs_post["user_id"],
        fx.user.id.as_str(),
        "the session's user, not the body's"
    );
    assert_eq!(rs_post["delete_at"], 0);
    assert_ne!(
        rs_post["create_at"], 1234,
        "SanitizeInput zeroed it, PreSave stamped it"
    );
    assert_eq!(rs_post["scheduled_at"], at);

    // Both rows, read through both servers: byte for byte.
    let ((gs, gb), (rs, rb)) = both(&client, &pair, &token, "GET", &list, None).await;
    assert_eq!((gs, rs), (200, 200));
    assert_eq!(text(&rb), text(&gb), "the list of two");
    let listed = parse(&gb);
    let listed = listed[&fx.team].as_array().expect("a list");
    assert_eq!(listed.len(), 2);
    // Rust's was created second and is due first: `ScheduledAt` decides, not `CreateAt`.
    assert_eq!(listed[0]["id"], rs_post["id"], "ScheduledAt ascending");
    assert_eq!(
        listed[0]["metadata"],
        json!({}),
        "prepareDraftWithFileInfos"
    );

    // Update each server's post through the other. The body is the post as created, message
    // changed — an update must carry `create_at` and `user_id` itself.
    let update = |post: &Json, message: &str| {
        let mut post = post.clone();
        post["message"] = json!(message);
        post.as_object_mut().unwrap().remove("repeat_type");
        body(post)
    };
    let go_id = go_post["id"].as_str().unwrap().to_owned();
    let rs_id = rs_post["id"].as_str().unwrap().to_owned();
    let (rs, rs_updated) = one(
        &client,
        &pair.rust,
        true,
        &token,
        "PUT",
        &format!("/api/v4/posts/schedule/{go_id}"),
        Some(&update(&go_post, "edited <by> rust")),
    )
    .await;
    let (gs, go_updated) = one(
        &client,
        &pair.go,
        false,
        &token,
        "PUT",
        &format!("/api/v4/posts/schedule/{rs_id}"),
        Some(&update(&rs_post, "edited <by> rust")),
    )
    .await;
    assert_eq!(
        (gs, rs),
        (201, 201),
        "Go {} / Rust {}",
        text(&go_updated),
        text(&rs_updated)
    );
    assert_eq!(
        tokenised(&rs_updated, &keys),
        tokenised(&go_updated, &keys),
        "the updated post"
    );
    assert!(rs_updated.ends_with(b"\n"));

    let ((_, gb), (_, rb)) = both(&client, &pair, &token, "GET", &list, None).await;
    assert_eq!(text(&rb), text(&gb), "the list after the updates");
    assert!(text(&gb).contains("edited \\u003cby\\u003e rust"));

    // Delete each through the other: 201, with the stored row as the body.
    let (rs, rs_deleted) = one(
        &client,
        &pair.rust,
        true,
        &token,
        "DELETE",
        &format!("/api/v4/posts/schedule/{go_id}"),
        None,
    )
    .await;
    let (gs, go_deleted) = one(
        &client,
        &pair.go,
        false,
        &token,
        "DELETE",
        &format!("/api/v4/posts/schedule/{rs_id}"),
        None,
    )
    .await;
    assert_eq!(
        (gs, rs),
        (201, 201),
        "Go {} / Rust {}",
        text(&go_deleted),
        text(&rs_deleted)
    );
    assert_eq!(
        tokenised(&rs_deleted, &keys),
        tokenised(&go_deleted, &keys),
        "the deleted post"
    );
    assert!(
        parse(&rs_deleted).get("metadata").is_none(),
        "a store read: no metadata"
    );

    // A second delete finds nothing — a 500, because the store's Get errors on no rows.
    for id in [&go_id, &rs_id] {
        let ((gs, gb), (rs, rb)) = both(
            &client,
            &pair,
            &token,
            "DELETE",
            &format!("/api/v4/posts/schedule/{id}"),
            None,
        )
        .await;
        assert_eq!((gs, rs), (500, 500), "{}", text(&gb));
        let parsed = assert_error_bodies_match_except_known_gaps(&gb, &rb, "a second delete");
        assert_eq!(
            parsed["id"],
            "app.delete_scheduled_post.get_scheduled_post.error"
        );
    }

    let ((_, gb), (_, rb)) = both(&client, &pair, &token, "GET", &list, None).await;
    assert_eq!(text(&rb), text(&gb));
    assert_eq!(text(&gb), format!("{{\"{}\":[]}}\n", fx.team));

    unwind(&client, &admin, fx).await;
}

/// `includeDirectChannels=true` — exactly that — adds a `directChannels` key holding the posts in
/// channels with no team. The map's keys are sorted, as `json.Marshal` sorts them.
#[tokio::test]
async fn the_direct_channels_join_the_list_only_when_asked_for() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "scheddm").await;
    let token = fx.user.token.clone();
    let dm = common::create_direct_channel(&client, &token, &fx.user.id, logged_in_user_id()).await;

    let at = now_ms() + 3_600_000;
    for (channel, message, offset) in [(&dm, "to the admin", 0), (&fx.channel, "to the team", 1)] {
        let (status, created) = one(
            &client,
            &pair.go,
            false,
            &token,
            "POST",
            "/api/v4/posts/schedule",
            Some(&body(
                json!({"channel_id": channel, "message": message, "scheduled_at": at + offset}),
            )),
        )
        .await;
        assert_eq!(status, 201, "{}", text(&created));
    }

    let base = format!("/api/v4/posts/scheduled/team/{}", fx.team);
    for (query, direct) in [
        ("", false),
        ("?includeDirectChannels=true", true),
        ("?includeDirectChannels=True", false),
        ("?includeDirectChannels=1", false),
        (
            "?includeDirectChannels=true&includeDirectChannels=false",
            true,
        ),
    ] {
        let path = format!("{base}{query}");
        let ((gs, gb), (rs, rb)) = both(&client, &pair, &token, "GET", &path, None).await;
        assert_eq!((gs, rs), (200, 200), "{query}: {}", text(&gb));
        assert_eq!(text(&rb), text(&gb), "{query}");
        let listed = parse(&gb);
        assert_eq!(
            listed[&fx.team].as_array().map(Vec::len),
            Some(1),
            "{query}"
        );
        assert_eq!(
            listed.get("directChannels").is_some(),
            direct,
            "{query}: {listed}"
        );
        if direct {
            assert_eq!(listed["directChannels"][0]["channel_id"], dm.as_str());
            // `json.Marshal` sorts map keys bytewise, so which key comes first depends on the
            // team id: one starting with `a`–`c` precedes `directChannels`.
            let first = if fx.team.as_str() < "directChannels" {
                fx.team.as_str()
            } else {
                "directChannels"
            };
            assert!(
                text(&gb).starts_with(&format!("{{\"{first}\":")),
                "sorted keys: {}",
                text(&gb)
            );
        }
    }

    unwind(&client, &admin, fx).await;
}

/// `(what, method, path, token, body, status, id)` — one refusal to provoke on both servers.
type Refusal<'a> = (
    &'a str,
    &'a str,
    String,
    &'a str,
    Option<Vec<u8>>,
    u16,
    &'a str,
);

/// The refusals, in Go's order, with Go's statuses and ids.
#[tokio::test]
async fn the_refusals_match_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "schedno").await;
    let token = fx.user.token.clone();
    let at = now_ms() + 3_600_000;
    let private = create_channel_typed(&client, &admin, &fx.team, "schednop", "P").await;
    let archived = create_channel(&client, &admin, &fx.team, "schednoa").await;
    common::delete_channel(&client, &admin, &archived).await;
    let other_team = create_team(&client, &admin, "schednot").await;

    // Somebody else's post, and one of the user's own.
    let (status, admins) = one(
        &client,
        &pair.go,
        false,
        &admin,
        "POST",
        "/api/v4/posts/schedule",
        Some(&body(
            json!({"channel_id": fx.channel, "message": "the admin's", "scheduled_at": at}),
        )),
    )
    .await;
    assert_eq!(status, 201, "{}", text(&admins));
    let admins = parse(&admins);
    let admins_id = admins["id"].as_str().unwrap().to_owned();
    let (status, own) = one(
        &client,
        &pair.go,
        false,
        &token,
        "POST",
        "/api/v4/posts/schedule",
        Some(&body(
            json!({"channel_id": fx.channel, "message": "mine", "scheduled_at": at}),
        )),
    )
    .await;
    assert_eq!(status, 201, "{}", text(&own));
    let own = parse(&own);
    let own_id = own["id"].as_str().unwrap().to_owned();
    let own_with = |changes: Json| {
        let mut post = own.clone();
        for (k, v) in changes.as_object().unwrap() {
            if v.is_null() {
                post.as_object_mut().unwrap().remove(k);
            } else {
                post[k] = v.clone();
            }
        }
        body(post)
    };

    let create = "/api/v4/posts/schedule".to_owned();
    let own_path = format!("/api/v4/posts/schedule/{own_id}");
    let cases: Vec<Refusal> = vec![
        (
            "not json",
            "POST",
            create.clone(),
            &token,
            Some(b"not json".to_vec()),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "an array",
            "POST",
            create.clone(),
            &token,
            Some(b"[]".to_vec()),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "a mistyped field",
            "POST",
            create.clone(),
            &token,
            Some(br#"{"message":5}"#.to_vec()),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "no message",
            "POST",
            create.clone(),
            &token,
            Some(body(json!({"channel_id": fx.channel, "scheduled_at": at}))),
            400,
            "model.scheduled_post.is_valid.empty_post.app_error",
        ),
        (
            "in the past",
            "POST",
            create.clone(),
            &token,
            Some(body(
                json!({"channel_id": fx.channel, "message": "m", "scheduled_at": now_ms() - 60_000}),
            )),
            400,
            "model.scheduled_post.is_valid.scheduled_at.app_error",
        ),
        (
            "a private channel",
            "POST",
            create.clone(),
            &token,
            Some(body(
                json!({"channel_id": private, "message": "m", "scheduled_at": at}),
            )),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "weekly, flag off",
            "POST",
            create.clone(),
            &token,
            Some(body(
                json!({"channel_id": fx.channel, "message": "m", "scheduled_at": at, "repeat_type": "weekly", "repeat_timezone": "UTC"}),
            )),
            400,
            "app.scheduled_post.recurring_disabled.app_error",
        ),
        (
            "monthly",
            "POST",
            create.clone(),
            &token,
            Some(body(
                json!({"channel_id": fx.channel, "message": "m", "scheduled_at": at, "repeat_type": "monthly"}),
            )),
            400,
            "model.scheduled_post.is_valid.repeat_type.app_error",
        ),
        (
            "a prioritised reply",
            "POST",
            create.clone(),
            &token,
            Some(body(
                json!({"channel_id": fx.channel, "message": "m", "scheduled_at": at, "root_id": ABSENT_ID, "metadata": {"priority": {"priority": "urgent"}}}),
            )),
            400,
            "api.post.post_priority.priority_post_only_allowed_for_root_post.request_error",
        ),
        (
            "too long",
            "POST",
            create.clone(),
            &token,
            Some(body(
                json!({"channel_id": fx.channel, "message": "x".repeat(20_000), "scheduled_at": at}),
            )),
            400,
            "model.draft.is_valid.message_length.app_error",
        ),
        (
            "an archived channel",
            "POST",
            create.clone(),
            &admin,
            Some(body(
                json!({"channel_id": archived, "message": "m", "scheduled_at": at}),
            )),
            400,
            "app.save_scheduled_post.channel_deleted.app_error",
        ),
        (
            // Not `GetChannel`'s 404: `SessionHasPermissionToChannel` refuses a channel that is
            // not there even to an admin, so the permission check answers first. Measured.
            "no such channel, as an admin",
            "POST",
            create.clone(),
            &admin,
            Some(body(
                json!({"channel_id": ABSENT_ID, "message": "m", "scheduled_at": at}),
            )),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "a body naming another id",
            "PUT",
            own_path.clone(),
            &token,
            Some(own_with(json!({"id": admins_id}))),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "a null body",
            "PUT",
            own_path.clone(),
            &token,
            Some(b"null".to_vec()),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "an unknown post",
            "PUT",
            format!("/api/v4/posts/schedule/{ABSENT_ID}"),
            &token,
            Some(own_with(json!({"id": ABSENT_ID}))),
            500,
            "app.update_scheduled_post.get_scheduled_post.error",
        ),
        (
            "somebody else's",
            "PUT",
            format!("/api/v4/posts/schedule/{admins_id}"),
            &token,
            Some(body(admins.clone())),
            403,
            "app.update_scheduled_post.update_permission.error",
        ),
        (
            "no create_at",
            "PUT",
            own_path.clone(),
            &token,
            Some(own_with(json!({"create_at": null}))),
            400,
            "model.draft.is_valid.create_at.app_error",
        ),
        (
            "no user_id",
            "PUT",
            own_path.clone(),
            &token,
            Some(own_with(json!({"user_id": null}))),
            400,
            "model.draft.is_valid.user_id.app_error",
        ),
        (
            "turning weekly on, flag off",
            "PUT",
            own_path.clone(),
            &token,
            Some(own_with(
                json!({"repeat_type": "weekly", "repeat_timezone": "UTC"}),
            )),
            400,
            "app.scheduled_post.recurring_disabled.app_error",
        ),
        (
            "moved to a private channel",
            "PUT",
            own_path.clone(),
            &token,
            Some(own_with(json!({"channel_id": private}))),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "delete somebody else's",
            "DELETE",
            format!("/api/v4/posts/schedule/{admins_id}"),
            &token,
            None,
            403,
            "app.delete_scheduled_post.delete_permission.error",
        ),
        (
            "delete an unknown post",
            "DELETE",
            format!("/api/v4/posts/schedule/{ABSENT_ID}"),
            &token,
            None,
            500,
            "app.delete_scheduled_post.get_scheduled_post.error",
        ),
        (
            "a short team id",
            "GET",
            "/api/v4/posts/scheduled/team/short".to_owned(),
            &token,
            None,
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "a team the user is not in",
            "GET",
            format!("/api/v4/posts/scheduled/team/{other_team}"),
            &token,
            None,
            403,
            "api.context.permissions.app_error",
        ),
    ];
    for (what, method, path, who, payload, status, id) in cases {
        let ((gs, gb), (rs, rb)) =
            both(&client, &pair, who, method, &path, payload.as_deref()).await;
        assert_eq!(gs, status, "{what}: Go answered {}", text(&gb));
        assert_eq!(rs, gs, "{what}: Rust answered {}", text(&rb));
        let parsed = assert_error_bodies_match_except_known_gaps(&gb, &rb, what);
        assert_eq!(parsed["id"], id, "{what}");
    }

    // None of the refused writes wrote: the user still owns exactly the one post.
    let list = format!("/api/v4/posts/scheduled/team/{}", fx.team);
    let ((_, gb), (_, rb)) = both(&client, &pair, &token, "GET", &list, None).await;
    assert_eq!(text(&rb), text(&gb));
    let listed = parse(&gb);
    assert_eq!(
        listed[&fx.team].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
    assert_eq!(listed[&fx.team][0]["message"], "mine");

    if let Some(pool) = common::fixture_pool().await {
        let _ = sqlx::query("DELETE FROM scheduledposts WHERE id = $1")
            .bind(&admins_id)
            .execute(&pool)
            .await;
    }
    unwind(&client, &admin, fx).await;
}

/// Plant a weekly post for `user` directly: the flag that would let the API create one is off
/// on both servers, and editing an existing series is exactly what stays allowed without it.
async fn plant_weekly(pool: &sqlx::PgPool, id: &str, user: &str, channel: &str, at: i64) {
    sqlx::query(
        "INSERT INTO scheduledposts (id, createat, updateat, userid, channelid, rootid, message,
                                     props, fileids, priority, scheduledat, processedat,
                                     errorcode, type, repeattype, repeattimezone)
         VALUES ($1, 1700000000000, 1700000000000, $2, $3, '', 'every week', '{}', '[]', 'null',
                 $4, 0, '', '', 'weekly', 'America/New_York')
         ON CONFLICT (id) DO UPDATE SET userid = $2, channelid = $3, scheduledat = $4,
                                        message = 'every week', repeattype = 'weekly',
                                        repeattimezone = 'America/New_York'",
    )
    .bind(id)
    .bind(user)
    .bind(channel)
    .bind(at)
    .execute(pool)
    .await
    .expect("the weekly row is planted");
}

/// **An update that does not name `repeat_type` keeps the series; one that names it — with any
/// casing, even as `null` — sets it.** Measured against both servers on a planted weekly post
/// each.
#[tokio::test]
async fn naming_repeat_type_on_an_update_is_what_ends_a_series() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "schedrep").await;
    let token = fx.user.token.clone();
    let at = now_ms() + 3_600_000;
    let ids = ["mmrsweeklygoaaaaaaaaaaaaaa", "mmrsweeklyrsaaaaaaaaaaaaaa"];

    // The body is what a client holding the post would send, less the fields being tested.
    let body_for = |id: &str, extra: Json| {
        let mut post = json!({
            "id": id,
            // Not the row's: `RestoreNonUpdatableFields` puts the row's back. The two processing
            // fields are reset whatever the body says.
            "create_at": 1600000000000i64,
            "user_id": fx.user.id,
            "channel_id": fx.channel,
            "message": "every week, edited",
            "scheduled_at": at,
            "error_code": "unable_to_send",
            "processed_at": 5,
        });
        for (k, v) in extra.as_object().unwrap() {
            post[k] = v.clone();
        }
        body(post)
    };

    for (step, extra, expected_type, expected_zone) in [
        ("absent", json!({}), "weekly", "America/New_York"),
        ("null", json!({"repeat_type": null}), "", ""),
        (
            "named in capitals",
            json!({"REPEAT_TYPE": "", "repeat_timezone": "UTC"}),
            "",
            "UTC",
        ),
        (
            "timezone only",
            json!({"repeat_timezone": "UTC"}),
            "weekly",
            "America/New_York",
        ),
    ] {
        for id in ids {
            plant_weekly(&pool, id, &fx.user.id, &fx.channel, at).await;
        }
        let (gs, gb) = one(
            &client,
            &pair.go,
            false,
            &token,
            "PUT",
            &format!("/api/v4/posts/schedule/{}", ids[0]),
            Some(&body_for(ids[0], extra.clone())),
        )
        .await;
        let (rs, rb) = one(
            &client,
            &pair.rust,
            true,
            &token,
            "PUT",
            &format!("/api/v4/posts/schedule/{}", ids[1]),
            Some(&body_for(ids[1], extra.clone())),
        )
        .await;
        assert_eq!(
            (gs, rs),
            (201, 201),
            "{step}: Go {} / Rust {}",
            text(&gb),
            text(&rb)
        );
        assert_eq!(tokenised(&rb, &MINTED), tokenised(&gb, &MINTED), "{step}");
        let answered = parse(&rb);
        assert_eq!(
            parse(&gb)["create_at"],
            1700000000000i64,
            "{step}: Go restored it"
        );
        assert_eq!(answered["error_code"], "", "{step}: {answered}");
        assert_eq!(answered["processed_at"], 0, "{step}: {answered}");
        assert_eq!(answered["repeat_type"], expected_type, "{step}: {answered}");
        assert_eq!(
            answered["repeat_timezone"], expected_zone,
            "{step}: {answered}"
        );
        assert_eq!(
            answered["create_at"], 1700000000000i64,
            "{step}: restored from the row"
        );
    }

    let _ = sqlx::query("DELETE FROM scheduledposts WHERE id = ANY($1)")
        .bind(&ids[..])
        .execute(&pool)
        .await;
    unwind(&client, &admin, fx).await;
}

/// Each write publishes to the **user** only: `scheduled_post_created`, `_updated` and
/// `_deleted`, with the post as a JSON **string** under `scheduledPost` — the same bytes the
/// response carried, less the newline.
#[tokio::test]
async fn each_write_publishes_its_event_to_the_user() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin, "schedws").await;
    let token = fx.user.token.clone();
    let at = now_ms() + 3_600_000;

    let mut shapes = Vec::new();
    for (base, rust) in [(pair.go.clone(), false), (pair.rust.clone(), true)] {
        let label = if rust { "rust" } else { "go" };
        let mut probe = SocketProbe::connect(&base, &token).await;

        let (status, created) = one(
            &client,
            &base,
            rust,
            &token,
            "POST",
            "/api/v4/posts/schedule",
            Some(&body(
                json!({"channel_id": fx.channel, "message": "watched <ws>", "scheduled_at": at}),
            )),
        )
        .await;
        assert_eq!(status, 201, "{label}: {}", text(&created));
        let id = parse(&created)["id"].as_str().unwrap().to_owned();
        let mut updated_body = parse(&created);
        updated_body["message"] = json!("watched, edited");
        let (status, updated) = one(
            &client,
            &base,
            rust,
            &token,
            "PUT",
            &format!("/api/v4/posts/schedule/{id}"),
            Some(&body(updated_body)),
        )
        .await;
        assert_eq!(status, 201, "{label}: {}", text(&updated));
        let (status, deleted) = one(
            &client,
            &base,
            rust,
            &token,
            "DELETE",
            &format!("/api/v4/posts/schedule/{id}"),
            None,
        )
        .await;
        assert_eq!(status, 201, "{label}: {}", text(&deleted));

        let event_for = |frames: &[Json], name: &str| -> Option<Json> {
            frames
                .iter()
                .find(|f| {
                    f["event"] == name
                        && f["data"]["scheduledPost"]
                            .as_str()
                            .is_some_and(|s| s.contains(&id))
                })
                .cloned()
        };
        let found = probe
            .collect_until(Duration::from_secs(5), |frames| {
                [
                    "scheduled_post_created",
                    "scheduled_post_updated",
                    "scheduled_post_deleted",
                ]
                .iter()
                .all(|name| event_for(frames, name).is_some())
            })
            .await;
        assert!(found, "{label}: the three events: {:?}", probe.frames());
        let frames = probe.frames();
        for (name, answered) in [
            ("scheduled_post_created", &created),
            ("scheduled_post_updated", &updated),
            ("scheduled_post_deleted", &deleted),
        ] {
            let event = event_for(&frames, name).unwrap();
            let carried = event["data"]["scheduledPost"].as_str().unwrap();
            assert_eq!(
                carried,
                text(answered).trim_end_matches('\n'),
                "{label}: {name} carries the answer's bytes"
            );
            assert_eq!(
                event["broadcast"]["user_id"],
                fx.user.id.as_str(),
                "{label}: {name}"
            );
            assert_eq!(event["broadcast"]["channel_id"], "", "{label}: {name}");
            assert_eq!(event["broadcast"]["team_id"], "", "{label}: {name}");
            let mut shape = event.clone();
            shape["data"]["scheduledPost"] = json!("<post>");
            shape["seq"] = json!(0);
            shapes.push((name, shape));
        }
        probe.close().await;
    }
    let (go, rust) = shapes.split_at(3);
    for ((name, g), (_, r)) in go.iter().zip(rust.iter()) {
        assert_eq!(r, g, "{name}: the frame's shape");
    }

    unwind(&client, &admin, fx).await;
}
