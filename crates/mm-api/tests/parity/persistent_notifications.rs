//! Cross-server parity for persistent notifications ([D-401]'s row, [D-551]): the
//! `persistent_notifications: true` arm of `POST /api/v4/posts`, the three writes that resolve a
//! persistent notification (a reply, a reaction, an acknowledgement by a mentioned user), and the
//! `post_persistent_notifications` job.
//!
//! # The create and resolve halves run on the licensed pair
//!
//! `postPriorityCheck` refuses `persistent_notifications` without a Professional licence, so the
//! arm is only reachable there. Each server gets its own post; the rows each leaves behind are
//! read back and compared.
//!
//! # The job half runs on the stack's own Go and on a second mm-api
//!
//! No licence is needed to *run* the job — its worker is enabled by `PostPriority` and
//! `AllowPersistentNotifications` alone — so the rows are planted by SQL on posts created through
//! Go: a `PostsPriority` row and a `PersistentNotifications` row old enough to be due. Go's run
//! is its own watcher claiming a job created through the API (the stack's oracles run no jobs —
//! `scripts/go-*.sh`); this server's is a second mm-api with the workers on and a fast poll,
//! which claims before Go's fifteen-second poll does. The same users are mentioned in both runs
//! so the only differences are the posts' and channels' own ids, which are substituted before the
//! events and pushes are compared.

use std::time::Duration;

use crate::common;

use common::push_proxy::{PushRequest, normalize_push, push_proxy};
use common::{
    GO, SocketProbe, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, delete_channel, delete_plain_user, fixture_pool, go_minted_token, licensed,
    plain_username, post_message, stack_enabled,
};

/// The job-running mm-api; see `second_server_ports`.
const JOB_SERVER_PORT: u16 = 8125;

async fn send(
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, Vec<u8>, bool) {
    let mut request = client()
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("answers");
    let status = response.status().as_u16();
    let by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        response.bytes().await.expect("a body").to_vec(),
        by_rust,
    )
}

/// `(createat, lastsentat, deleteat, sentcount)` of a post's row, or `None`.
async fn row(pool: &sqlx::PgPool, post_id: &str) -> Option<(i64, i64, i64, i16)> {
    sqlx::query_as(
        "SELECT createat, lastsentat, deleteat, sentcount FROM persistentnotifications \
         WHERE postid = $1",
    )
    .bind(post_id)
    .fetch_optional(pool)
    .await
    .expect("the row query")
}

fn persistent_body(channel: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "channel_id": channel,
        "message": message,
        "metadata": {"priority": {"priority": "urgent", "persistent_notifications": true}},
    })
}

// ---------------------------------------------------------------------------------------------
// createPost
// ---------------------------------------------------------------------------------------------

/// A persistent-notification post is written with its row, and the recipient check answers as
/// Go's does: none mentioned and more than `PersistentNotificationMaxRecipients` (5) are both the
/// one **500** Go wraps its own two 400s in. A DM skips the check.
#[tokio::test]
async fn a_persistent_notification_post_matches_go() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let mut users = Vec::new();
    for n in 0..6 {
        users.push(create_plain_user(&http, &admin, &team, &format!("pnc{n}")).await);
    }
    let channel = create_channel(&http, &admin, &team, "pncreate").await;
    for user in &users {
        common::add_user_to_channel(&http, &admin, &channel, &user.id).await;
    }
    let mention = |n: usize| format!("@{}", plain_username(&format!("pnc{n}")));

    // Served, with a row whose CreateAt is the post's and every other column zero.
    let mut created = Vec::new();
    for base in [&pair.go, &pair.rust] {
        let (status, body, by_rust) = send(
            base,
            &admin,
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(persistent_body(&channel, &format!("{} look", mention(0)))),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&body));
        if base == &pair.rust {
            assert!(by_rust, "the persistent arm is served here");
        }
        let post: serde_json::Value = serde_json::from_slice(&body).expect("a post");
        created.push(post);
    }
    let strip = |post: &serde_json::Value| {
        let mut post = post.clone();
        for key in ["id", "create_at", "update_at"] {
            post[key] = serde_json::json!(0);
        }
        post
    };
    assert_eq!(strip(&created[1]), strip(&created[0]), "the created post");
    for post in &created {
        let id = post["id"].as_str().expect("an id");
        assert_eq!(
            row(&pool, id).await,
            Some((post["create_at"].as_i64().expect("a time"), 0, 0, 0)),
            "savePostsPersistentNotifications"
        );
    }

    // Exactly `PersistentNotificationMaxRecipients` (5) mentioned is allowed.
    let five: Vec<String> = (0..5).map(mention).collect();
    for base in [&pair.go, &pair.rust] {
        let (status, body, _) = send(
            base,
            &admin,
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(persistent_body(&channel, &five.join(" "))),
        )
        .await;
        assert_eq!(
            status,
            201,
            "{base}: five is the maximum, not past it: {}",
            String::from_utf8_lossy(&body)
        );
    }

    // Nobody mentioned, and six mentioned: the same wrapped 500 from both.
    let everyone: Vec<String> = (0..6).map(mention).collect();
    for message in ["nobody at all".to_owned(), everyone.join(" ")] {
        let (go_status, go, _) = send(
            &pair.go,
            &admin,
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(persistent_body(&channel, &message)),
        )
        .await;
        let (rs_status, rs, by_rust) = send(
            &pair.rust,
            &admin,
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(persistent_body(&channel, &message)),
        )
        .await;
        assert_eq!(
            go_status,
            500,
            "{message}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{message}");
        assert!(by_rust, "{message}: refused here");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &message);
        assert_eq!(
            parsed["id"],
            "api.post.post_priority.persistent_notification_validation_error.request_error"
        );
    }

    delete_channel(&http, &admin, &channel).await;
    for user in users {
        delete_plain_user(&http, &admin, &user.id).await;
    }
}

/// A reply, a reaction and an acknowledgement by a **mentioned** user each retire the row; the
/// same three by a user the post does not mention leave it. Each write is compared against Go on
/// a post of each server's own.
#[tokio::test]
async fn a_mentioned_users_reply_reaction_or_ack_resolves_it_as_go_does() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let mentioned = create_plain_user(&http, &admin, &team, "pnres1").await;
    let bystander = create_plain_user(&http, &admin, &team, "pnres2").await;
    let channel = create_channel(&http, &admin, &team, "pnresolve").await;
    common::add_user_to_channel(&http, &admin, &channel, &mentioned.id).await;
    common::add_user_to_channel(&http, &admin, &channel, &bystander.id).await;
    // The author mentions themself too: an author's own reaction, reply or acknowledgement never
    // resolves, mentioned or not.
    let me: serde_json::Value = serde_json::from_slice(
        &send(GO, &admin, reqwest::Method::GET, "/api/v4/users/me", None)
            .await
            .1,
    )
    .expect("me");
    let author = common::PlainUser {
        id: me["id"].as_str().expect("an id").to_owned(),
        token: admin.clone(),
    };
    let message = format!(
        "@{} @{} please",
        plain_username("pnres1"),
        me["username"].as_str().expect("a username")
    );

    type Write = fn(&str, &str, &str) -> (reqwest::Method, String, Option<serde_json::Value>);
    let writes: [(&str, Write); 3] = [
        ("reply", |post, channel, _user| {
            (
                reqwest::Method::POST,
                "/api/v4/posts".to_owned(),
                Some(serde_json::json!({"channel_id": channel, "root_id": post, "message": "ok"})),
            )
        }),
        ("reaction", |post, _channel, user| {
            (
                reqwest::Method::POST,
                "/api/v4/reactions".to_owned(),
                Some(serde_json::json!({"user_id": user, "post_id": post, "emoji_name": "eyes"})),
            )
        }),
        ("ack", |post, _channel, user| {
            (
                reqwest::Method::POST,
                format!("/api/v4/users/{user}/posts/{post}/ack"),
                None,
            )
        }),
    ];

    for (what, write) in writes {
        for (actor, resolves) in [(&bystander, false), (&author, false), (&mentioned, true)] {
            let mut outcome = Vec::new();
            for base in [&pair.go, &pair.rust] {
                let (status, body, _) = send(
                    base,
                    &admin,
                    reqwest::Method::POST,
                    "/api/v4/posts",
                    Some(persistent_body(&channel, &message)),
                )
                .await;
                assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
                let post: serde_json::Value = serde_json::from_slice(&body).expect("a post");
                let post_id = post["id"].as_str().expect("an id").to_owned();

                let (method, path, body) = write(&post_id, &channel, &actor.id);
                let (status, answer, by_rust) = send(base, &actor.token, method, &path, body).await;
                if base == &pair.rust {
                    assert!(by_rust, "{what}: served here");
                }
                let retired = row(&pool, &post_id).await.map(|r| r.2 != 0);
                outcome.push((status, retired, !answer.is_empty()));
            }
            assert_eq!(outcome[1], outcome[0], "{what} by {}", actor.id);
            assert_eq!(
                outcome[0].1,
                Some(resolves),
                "{what}: Go retires the row only for a mentioned user"
            );
        }
    }

    delete_channel(&http, &admin, &channel).await;
    delete_plain_user(&http, &admin, &mentioned.id).await;
    delete_plain_user(&http, &admin, &bystander.id).await;
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

/// The posts of one run's fixture.
struct JobFixture {
    channel: String,
    channel_name: String,
    display_name: String,
    archived: String,
    /// Due, sent never: notified, `SentCount` 0 → 1.
    fresh: String,
    /// Due, sent five times: notified, then retired by `DeleteExpired` at six.
    last: String,
    /// Due, in an archived channel: retired without a notification.
    orphan: String,
    /// Not due yet: untouched.
    early: String,
}

async fn plant_row(pool: &sqlx::PgPool, post: &str, channel: &str, age_ms: i64, sent: i16) {
    let at = mm_model::utils::get_millis() - age_ms;
    sqlx::query(
        "INSERT INTO postspriority (postid, channelid, priority, requestedack, \
         persistentnotifications) VALUES ($1, $2, 'urgent', false, true)",
    )
    .bind(post)
    .bind(channel)
    .execute(pool)
    .await
    .expect("the priority row");
    sqlx::query(
        "INSERT INTO persistentnotifications (postid, createat, lastsentat, deleteat, sentcount) \
         VALUES ($1, $2, $2, 0, $3)",
    )
    .bind(post)
    .bind(at)
    .bind(sent)
    .execute(pool)
    .await
    .expect("the persistent row");
}

async fn job_fixture(
    pool: &sqlx::PgPool,
    admin: &str,
    team: &str,
    tag: &str,
    mention: &str,
) -> JobFixture {
    let http = client();
    let channel = create_channel(&http, admin, team, tag).await;
    let archived = create_channel(&http, admin, team, &format!("{tag}x")).await;
    for user in ["pnja", "pnjb", "pnjd"] {
        let id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
            .bind(plain_username(user))
            .fetch_one(pool)
            .await
            .expect("the user");
        common::add_user_to_channel(&http, admin, &channel, &id).await;
        common::add_user_to_channel(&http, admin, &archived, &id).await;
    }
    let ten_minutes = 10 * 60 * 1000;
    let fresh = post_message(&http, admin, &channel, &format!("{mention} look now"), None).await;
    let last = post_message(
        &http,
        admin,
        &channel,
        &format!("{mention} last call"),
        None,
    )
    .await;
    let orphan = post_message(&http, admin, &archived, &format!("{mention} gone"), None).await;
    let early = post_message(&http, admin, &channel, &format!("{mention} not yet"), None).await;
    plant_row(pool, &fresh, &channel, ten_minutes, 0).await;
    plant_row(pool, &last, &channel, ten_minutes, 5).await;
    plant_row(pool, &early, &channel, 1000, 0).await;
    // Archived **before** its row is planted: archiving retires the channel's rows itself
    // (`DeleteByChannel`), and the row this case needs is one the walk finds and cleans up.
    delete_channel(&http, admin, &archived).await;
    plant_row(pool, &orphan, &archived, ten_minutes, 0).await;
    JobFixture {
        channel,
        channel_name: format!("mmrs-parity-{tag}"),
        display_name: format!("mmrs parity {tag}"),
        archived,
        fresh,
        last,
        orphan,
        early,
    }
}

/// `(sent_count, sent_since_planting, retired)` for each of the fixture's four rows.
async fn job_rows(pool: &sqlx::PgPool, f: &JobFixture) -> Vec<(i16, bool, bool)> {
    let mut out = Vec::new();
    for post in [&f.fresh, &f.last, &f.orphan, &f.early] {
        let (create_at, last_sent, delete_at, sent) = row(pool, post).await.expect("the row");
        out.push((sent, last_sent > create_at, delete_at != 0));
    }
    out
}

/// Everything a run leaves: rows, the events the websocket-connected mentioned user saw, and the
/// pushes the offline mentioned user's device got — with this run's own ids swapped for markers.
struct RunCapture {
    job_status: String,
    rows: Vec<(i16, bool, bool)>,
    events: Vec<serde_json::Value>,
    pushes: Vec<serde_json::Value>,
}

fn substitute(text: &str, f: &JobFixture) -> String {
    let mut text = text.to_owned();
    for (from, to) in [
        (&f.fresh, "<fresh>"),
        (&f.last, "<last>"),
        (&f.orphan, "<orphan>"),
        (&f.early, "<early>"),
        (&f.channel, "<channel>"),
        (&f.archived, "<archived>"),
        (&f.channel_name, "<channel-name>"),
        (&f.display_name, "<display-name>"),
    ] {
        text = text.replace(from.as_str(), to);
    }
    text
}

/// A value with its per-post volatile fields (timestamps) blanked, recursively through the JSON
/// strings Go nests.
fn blank_times(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if matches!(
                    key.as_str(),
                    "create_at" | "update_at" | "last_reply_at" | "seq"
                ) {
                    *inner = serde_json::json!(0);
                } else if let Some(text) = inner.as_str()
                    && (text.starts_with('{') || text.starts_with('['))
                    && let Ok(mut nested) = serde_json::from_str::<serde_json::Value>(text)
                {
                    blank_times(&mut nested);
                    *inner = serde_json::Value::String(nested.to_string());
                } else {
                    blank_times(inner);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(blank_times),
        _ => {}
    }
}

async fn capture_run(
    pool: &sqlx::PgPool,
    base: &str,
    reader_token: &str,
    device: &str,
    f: &JobFixture,
) -> RunCapture {
    let proxy = push_proxy().expect("the push proxy");
    // The planting posts mentioned the offline user, and those pushes are the create path's,
    // not the job's: drain them first.
    let device_filter = |r: &PushRequest| {
        r.json()["device_id"]
            .as_str()
            .is_some_and(|d| d.ends_with(device))
    };
    while proxy
        .take(Duration::from_secs(2), device_filter)
        .await
        .is_some()
    {}
    let mut probe = SocketProbe::connect(base, reader_token).await;
    let job =
        common::insert_pending_job(pool, "post_persistent_notifications", serde_json::json!({}))
            .await;
    let (job_status, _data) = common::wait_for_job(pool, &job).await;
    probe
        .collect_until(Duration::from_secs(5), |frames| {
            frames
                .iter()
                .filter(|fr| fr["event"] == "persistent_notification_triggered")
                .count()
                >= 2
        })
        .await;
    probe.collect_for(Duration::from_millis(500)).await;

    let mut events: Vec<serde_json::Value> = probe
        .raw_events_named("persistent_notification_triggered")
        .into_iter()
        .map(|raw| {
            let mut value: serde_json::Value =
                serde_json::from_str(&substitute(raw, f)).expect("a frame");
            blank_times(&mut value);
            // Go builds the list from a map, so its order is random; compare it as a set.
            if let Some(list) = value["data"]["mentions"].as_str() {
                let mut ids: Vec<String> = serde_json::from_str(list).expect("a JSON list");
                ids.sort();
                value["data"]["mentions"] =
                    serde_json::Value::String(serde_json::to_string(&ids).expect("encodes"));
            }
            value
        })
        .collect();
    events.sort_by_key(|e| e.to_string());

    let mut pushes = Vec::new();
    while let Some(request) = proxy.take(Duration::from_secs(3), device_filter).await {
        let mut value: serde_json::Value =
            serde_json::from_str(&substitute(&normalize_push(request.json()).to_string(), f))
                .expect("JSON");
        blank_times(&mut value);
        pushes.push(value);
    }
    pushes.sort_by_key(|p| p.to_string());

    RunCapture {
        job_status,
        rows: job_rows(pool, f).await,
        events,
        pushes,
    }
}

/// Retire every due row that is not one of this test's: the job walks the whole table, and a
/// leftover from another run would be notified by one server's run and not the other's.
async fn retire_foreign_due_rows(pool: &sqlx::PgPool, keep: &[&str]) {
    let keep: Vec<String> = keep.iter().map(|s| (*s).to_owned()).collect();
    sqlx::query(
        "UPDATE persistentnotifications SET deleteat = 1 \
         WHERE deleteat = 0 AND NOT (postid = ANY($1)) AND createat <= $2",
    )
    .bind(&keep)
    // Only rows already **due**: a fresh row belongs to a test running beside this one.
    .bind(mm_model::utils::get_millis() - 5 * 60 * 1000)
    .execute(pool)
    .await
    .expect("foreign rows retired");
}

#[tokio::test]
async fn the_persistent_notifications_job_matches_gos_run() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let _jobs = common::JOB_RUNS.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let reader = create_plain_user(&http, &admin, &team, "pnja").await;
    let dnd = create_plain_user(&http, &admin, &team, "pnjb").await;
    let offline = create_plain_user(&http, &admin, &team, "pnjd").await;
    let (status, body, _) = send(
        GO,
        &admin,
        reqwest::Method::PUT,
        &format!("/api/v4/users/{}/status", dnd.id),
        Some(serde_json::json!({"user_id": dnd.id, "status": "dnd"})),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let device = "android_rn:mmrspnjobdevice";
    let (status, _, _) = send(
        GO,
        &offline.token,
        reqwest::Method::PUT,
        "/api/v4/users/sessions/device",
        Some(serde_json::json!({"device_id": device})),
    )
    .await;
    assert_eq!(status, 200, "the device attaches");
    let mention = format!(
        "@{} @{} @{}",
        plain_username("pnja"),
        plain_username("pnjb"),
        plain_username("pnjd")
    );

    // Go's run.
    let go_fixture = job_fixture(&pool, &admin, &team, "pnjobgo", &mention).await;
    retire_foreign_due_rows(
        &pool,
        &[
            &go_fixture.fresh,
            &go_fixture.last,
            &go_fixture.orphan,
            &go_fixture.early,
        ],
    )
    .await;
    let go = capture_run(&pool, GO, &reader.token, "mmrspnjobdevice", &go_fixture).await;

    // The push badge counts the offline user's unread mentions everywhere, so Go's fixture
    // channel is read first or this run's badge would carry Go's mentions as well as its own.
    common::view_channel(&http, &offline.token, &go_fixture.channel).await;

    // This server's run, on a second mm-api whose watcher polls every 200 ms.
    let server = common::job_server(JOB_SERVER_PORT).await;
    // Go's watcher polls every fifteen seconds and this one every 200 ms, so this one claims the
    // job all but ~1% of the time. When Go wins, its events reach Go's hub and this probe sees
    // none: that attempt is discarded and the run repeated on a fresh fixture.
    let mut attempt = 0;
    let (rs_fixture, rs) = loop {
        attempt += 1;
        let fixture =
            job_fixture(&pool, &admin, &team, &format!("pnjobrs{attempt}"), &mention).await;
        retire_foreign_due_rows(
            &pool,
            &[
                &fixture.fresh,
                &fixture.last,
                &fixture.orphan,
                &fixture.early,
            ],
        )
        .await;
        let run = capture_run(
            &pool,
            &server.server.base,
            &reader.token,
            "mmrspnjobdevice",
            &fixture,
        )
        .await;
        common::view_channel(&http, &offline.token, &fixture.channel).await;
        if !run.events.is_empty() || attempt == 3 {
            break (fixture, run);
        }
        delete_channel(&http, &admin, &fixture.channel).await;
    };
    drop(server);

    assert_eq!(go.job_status, "success");
    assert_eq!(rs.job_status, go.job_status, "the job's outcome");
    assert_eq!(
        go.rows,
        vec![
            (1, true, false),
            (6, true, true),
            (1, true, true),
            (0, false, false)
        ],
        "Go's rows: sent, sent and expired, retired orphan, untouched"
    );
    assert_eq!(rs.rows, go.rows, "the rows");
    assert_eq!(
        go.events.len(),
        2,
        "the reader is told of both due posts: {:?}",
        go.events
    );
    assert_eq!(rs.events, go.events, "persistent_notification_triggered");
    assert!(!go.pushes.is_empty(), "the offline user's device is pushed");
    assert_eq!(rs.pushes, go.pushes, "the pushes");

    for f in [&go_fixture, &rs_fixture] {
        delete_channel(&http, &admin, &f.channel).await;
    }
    for user in [reader, dnd, offline] {
        delete_plain_user(&http, &admin, &user.id).await;
    }
}
