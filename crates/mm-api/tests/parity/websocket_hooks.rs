//! Cross-server parity for the broadcast hooks the hub runs per connection — as seen on a
//! `posted` event from `POST /api/v4/posts`, the one served route that Go attaches them to.
//!
//! ```sh
//! scripts/parity.sh --test parity websocket_hooks
//! ```
//!
//! # What is observable, and from where
//!
//! `SendNotifications` attaches `posted_ack` to **every** `posted` event and `add_mentions` /
//! `add_followers` when there is anyone to name. A hook that writes a key makes a per-connection
//! copy, and the copy leaves through `json.Encoder` — compact, newline-terminated — where the
//! shared event leaves precomputed, with a space after each colon and no newline. So a recipient
//! can see two things: whether `should_ack` is in `data`, and which encoding the frame took.
//!
//! Both are asserted here, **raw**: a value comparison cannot see the encoding.
//!
//! # The recipient has to opt in twice
//!
//! `posted_ack` acks a connection only when it connected with `?posted_ack=true` **and** there is
//! a reason: the frame already carries `mentions`/`followers`, the channel is a DM, or the user is
//! in `users` — the members whose desktop notification level makes this post notify them
//! (`shouldAckWebsocketNotification`). A plain post in an open channel to a member with default
//! notify props therefore acks **nobody**, flag or not, and every frame leaves precomputed. That
//! is the first test, and it is the case this server reproduces today.
//!
//! The acking case needs the recipient's `notify_props.desktop` set to `all`. Go answers it with
//! `should_ack: true` in the encoder shape, and this server answers it the same way now that
//! `SendNotifications`' port attaches the hook (2026-09-14).
//!
//! # Fixture rows all begin `mmrshubhooks`
//!
//! Its own prefix, for the reason `post_creates.rs` gives: `common::purge_api_fixtures` runs once
//! per test binary, concurrently with this one.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, add_user_to_channel, client, create_plain_user,
    delete_plain_user, go_minted_token, stack_enabled,
};

const PREFIX: &str = "mmrshubhooks";

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

async fn fixture_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

static PURGED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn purge_hook_fixtures() {
    PURGED.get_or_init(purge_hook_fixtures_once).await;
}

async fn purge_hook_fixtures_once() {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let like = format!("{PREFIX}%");
    for statement in [
        "DELETE FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE $1)",
        "DELETE FROM channelmembers WHERE channelid IN (SELECT id FROM channels WHERE name LIKE $1)",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT id FROM channels WHERE name LIKE $1)",
        "DELETE FROM channels WHERE name LIKE $1",
        "DELETE FROM teammembers WHERE teamid IN (SELECT id FROM teams WHERE name LIKE $1)",
        "DELETE FROM teams WHERE name LIKE $1",
    ] {
        let _ = sqlx::query(statement).bind(&like).execute(&pool).await;
    }
}

/// A team, an open channel in it, and a plain member of both — created through Go, which both
/// servers read.
struct Fixture {
    channel_id: String,
    member: common::PlainUser,
}

/// Whether the member's `notify_props.desktop` is set to `all` — **before** they join the
/// channel. Joining posts a system message whose `SendNotifications` runs on a goroutine and
/// fills Go's profiles-in-channel cache from a read that can predate a patch made right after
/// the join, so a patch-after-join fixture measured Go acking nobody (2026-09-14). Patched first,
/// the cache is filled with the props the test needs.
#[derive(Clone, Copy)]
enum Desktop {
    Default,
    All,
}

async fn fixture(client: &reqwest::Client, admin: &str, tag: &str, desktop: Desktop) -> Fixture {
    purge_hook_fixtures().await;

    let team: serde_json::Value = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "name": format!("{PREFIX}team{tag}"),
            "display_name": format!("mmrs hub hooks {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the team decodes");
    let team_id = team["id"].as_str().expect("a team id").to_owned();

    let channel: serde_json::Value = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("{PREFIX}chan{tag}"),
            "display_name": format!("mmrs hub hooks {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the channel decodes");
    let channel_id = channel["id"].as_str().expect("a channel id").to_owned();

    let member = create_plain_user(client, admin, &team_id, &format!("hubhooks{tag}")).await;
    if let Desktop::All = desktop {
        notify_on_every_post(client, admin, &member.id).await;
    }
    add_user_to_channel(client, admin, &channel_id, &member.id).await;

    // `SendNotifications` reads the channel members' notify props through a per-channel cache
    // that the channel's own creation post has already warmed — with the creator alone — and a
    // join does not invalidate it. A member joined after that reads as absent, `""` rather than
    // `"default"`, and `shouldAckWebsocketNotification`'s second arm fails: measured, Go acked
    // nobody in a fresh channel (2026-09-14). This server reads the rows, so the comparison is
    // only fair once Go is fresh too.
    common::invalidate_go_caches(client, admin).await;

    Fixture { channel_id, member }
}

/// Put the member in `usersToAck` for every post in an open channel: `desktop: all` with the
/// channel member's level left at `default` is `shouldAckWebsocketNotification`'s second arm.
///
/// `User.Patch` **replaces** `NotifyProps` (user.go:673), so the map is sent whole — and with
/// `mention_keys` empty and `first_name` off, so the member cannot be keyword-mentioned, which is
/// the shape this server forwards rather than serves.
async fn notify_on_every_post(client: &reqwest::Client, admin: &str, user_id: &str) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "notify_props": {
                "desktop": "all",
                "push": "mention",
                "email": "true",
                "channel": "true",
                "mention_keys": "",
                "first_name": "false",
                "desktop_sound": "true",
                "comments": "never",
                "push_status": "away",
            },
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "setting desktop=all failed: {}",
        response.text().await.unwrap_or_default()
    );
}

// ---------------------------------------------------------------------------------------------
// the exchange
// ---------------------------------------------------------------------------------------------

/// The three connections a `posted` event tells apart, plus the frame each got for one post.
struct Frames {
    /// The member, connected with `?posted_ack=true`.
    member_flagged: String,
    /// The member, connected without the flag.
    member_plain: String,
    /// The poster, connected with the flag — never acked for their own post.
    poster_flagged: String,
}

/// Open the three sockets against `base`, post `message` there as the admin, and collect the
/// `posted` frame for that post on each. Every wait is scoped to the post id, because the rest of
/// the suite is broadcasting on these servers at the same time.
async fn post_and_collect(
    client: &reqwest::Client,
    base: &str,
    admin: &str,
    f: &Fixture,
    message: &str,
) -> Frames {
    let mut member_flagged =
        SocketProbe::connect_with_query(base, &f.member.token, "posted_ack=true").await;
    let mut member_plain = SocketProbe::connect(base, &f.member.token).await;
    let mut poster_flagged = SocketProbe::connect_with_query(base, admin, "posted_ack=true").await;

    let response = client
        .post(format!("{base}/api/v4/posts"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "channel_id": f.channel_id, "message": message }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}/api/v4/posts is unreachable: {e}"));
    assert_eq!(
        response.status().as_u16(),
        201,
        "{base}: the post was not created"
    );
    if base == RUST {
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "a plain post in an open channel must be served here, or the frames are Go's"
        );
    }
    let post: serde_json::Value = response.json().await.expect("the post decodes");
    let post_id = post["id"].as_str().expect("a post id").to_owned();

    let mut frames = Vec::with_capacity(3);
    for (name, probe) in [
        ("member_flagged", &mut member_flagged),
        ("member_plain", &mut member_plain),
        ("poster_flagged", &mut poster_flagged),
    ] {
        let arrived = probe
            .collect_until(Duration::from_millis(3_000), |frames| {
                frames.iter().any(|frame| is_posted_for(frame, &post_id))
            })
            .await;
        assert!(
            arrived,
            "{base}: no posted frame for {post_id} on {name}: {:?}",
            probe.raw
        );
        let raw = probe
            .raw
            .iter()
            .find(|raw| {
                serde_json::from_str::<serde_json::Value>(raw)
                    .is_ok_and(|frame| is_posted_for(&frame, &post_id))
            })
            .expect("collect_until saw it")
            .clone();
        frames.push(raw);
    }
    let poster_flagged = frames.pop().expect("three");
    let member_plain = frames.pop().expect("three");
    let member_flagged = frames.pop().expect("three");

    Frames {
        member_flagged,
        member_plain,
        poster_flagged,
    }
}

/// A `posted` frame whose `data.post` — a JSON **string** — is this post.
fn is_posted_for(frame: &serde_json::Value, post_id: &str) -> bool {
    frame["event"] == "posted"
        && frame["data"]["post"]
            .as_str()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .is_some_and(|post| post["id"] == post_id)
}

fn data(raw: &str) -> serde_json::Map<String, serde_json::Value> {
    let frame: serde_json::Value = serde_json::from_str(raw).expect("a frame decodes");
    frame["data"]
        .as_object()
        .cloned()
        .expect("data is an object")
}

fn data_keys(raw: &str) -> Vec<String> {
    data(raw).keys().cloned().collect()
}

/// `precomputedJSONBuf` (websocket_message.go:397): hand-concatenated with a space after each
/// colon between the four top-level keys, and no trailing newline.
fn assert_precomputed_shape(raw: &str, context: &str) {
    assert!(
        raw.starts_with(r#"{"event": "posted", "data": "#),
        "{context}: expected the precomputed (spaced) shape, got {raw:?}"
    );
    assert!(
        !raw.ends_with('\n'),
        "{context}: the precomputed shape has no trailing newline"
    );
}

/// `json.Encoder.Encode` of `webSocketEventJSON`: compact, and newline-terminated. This is the
/// shape a hook-modified copy takes, because `RemovePrecomputedJSON` is what made the copy.
fn assert_encoder_shape(raw: &str, context: &str) {
    assert!(
        raw.starts_with(r#"{"event":"posted","data":"#),
        "{context}: expected the json.Encoder (compact) shape, got {raw:?}"
    );
    assert!(
        raw.ends_with('\n'),
        "{context}: json.Encoder appends a newline"
    );
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// A plain post in an open channel to a member with default notify props: `posted_ack` is
/// attached and runs, finds no reason to ack anyone, and every connection — flagged or not,
/// member or poster — gets the shared, precomputed frame with no `should_ack`. Identical on both.
#[tokio::test]
async fn a_post_that_acks_nobody_leaves_precomputed_for_every_connection_on_both() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin, "noack", Desktop::Default).await;

    let go = post_and_collect(&client, GO, &admin, &f, "mmrs hooks noack go").await;
    let rs = post_and_collect(&client, RUST, &admin, &f, "mmrs hooks noack rs").await;

    for (name, go_raw, rs_raw) in [
        ("member_flagged", &go.member_flagged, &rs.member_flagged),
        ("member_plain", &go.member_plain, &rs.member_plain),
        ("poster_flagged", &go.poster_flagged, &rs.poster_flagged),
    ] {
        assert!(
            data(go_raw).get("should_ack").is_none(),
            "Go acks nobody here: {name}: {go_raw}"
        );
        assert!(
            data(rs_raw).get("should_ack").is_none(),
            "and neither do we: {name}: {rs_raw}"
        );
        assert_precomputed_shape(go_raw, &format!("Go {name}"));
        assert_precomputed_shape(rs_raw, &format!("Rust {name}"));
        assert_eq!(
            data_keys(go_raw),
            data_keys(rs_raw),
            "{name}: the posted data carries different keys"
        );
    }

    delete_plain_user(&client, &admin, &f.member.id).await;
}

/// **The Go oracle for the acking case.** With the member's desktop level at `all`, Go's
/// `posted_ack` acks the member's *flagged* connection and makes a copy to do it — so that frame
/// alone carries `should_ack: true` and takes the encoder shape. The same member without the
/// flag, and the poster's own flagged connection, get the shared precomputed frame.
#[tokio::test]
async fn go_acks_a_desktop_all_member_only_on_a_flagged_connection_and_never_the_poster() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin, "goack", Desktop::All).await;

    let go = post_and_collect(&client, GO, &admin, &f, "mmrs hooks goack").await;

    assert_eq!(
        data(&go.member_flagged).get("should_ack"),
        Some(&serde_json::Value::Bool(true)),
        "the flagged member is told to ack: {}",
        go.member_flagged
    );
    assert_encoder_shape(&go.member_flagged, "Go member_flagged");

    assert!(
        data(&go.member_plain).get("should_ack").is_none(),
        "no flag, no ack: {}",
        go.member_plain
    );
    assert_precomputed_shape(&go.member_plain, "Go member_plain");

    assert!(
        data(&go.poster_flagged).get("should_ack").is_none(),
        "the poster is never acked to themselves: {}",
        go.poster_flagged
    );
    assert_precomputed_shape(&go.poster_flagged, "Go poster_flagged");

    delete_plain_user(&client, &admin, &f.member.id).await;
}

/// The cross-server half of the test above. The hub runs the hook and the encoding follows the
/// copy; the raiser is `mm_app::notification::App::send_notifications`, which attaches
/// `posted_ack` with `users` from `shouldAckWebsocketNotification` on every `posted` event.
#[tokio::test]
async fn a_desktop_all_member_is_acked_on_a_flagged_connection_the_same_way_on_both() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin, "bothack", Desktop::All).await;

    let go = post_and_collect(&client, GO, &admin, &f, "mmrs hooks bothack go").await;
    let rs = post_and_collect(&client, RUST, &admin, &f, "mmrs hooks bothack rs").await;

    // The acked frame: `should_ack` present and true, and the copy's encoding, on both.
    for (server, raw) in [("Go", &go.member_flagged), ("Rust", &rs.member_flagged)] {
        assert_eq!(
            data(raw).get("should_ack"),
            Some(&serde_json::Value::Bool(true)),
            "{server}: the flagged member is told to ack: {raw}"
        );
        assert_encoder_shape(raw, &format!("{server} member_flagged"));
    }
    assert_eq!(
        data_keys(&go.member_flagged),
        data_keys(&rs.member_flagged),
        "member_flagged: the posted data carries different keys"
    );

    // The two that are not acked: no key, shared frame, on both.
    for (name, go_raw, rs_raw) in [
        ("member_plain", &go.member_plain, &rs.member_plain),
        ("poster_flagged", &go.poster_flagged, &rs.poster_flagged),
    ] {
        assert!(
            data(go_raw).get("should_ack").is_none(),
            "Go {name}: {go_raw}"
        );
        assert!(
            data(rs_raw).get("should_ack").is_none(),
            "Rust {name}: {rs_raw}"
        );
        assert_precomputed_shape(go_raw, &format!("Go {name}"));
        assert_precomputed_shape(rs_raw, &format!("Rust {name}"));
        assert_eq!(
            data_keys(go_raw),
            data_keys(rs_raw),
            "{name}: different keys"
        );
    }

    delete_plain_user(&client, &admin, &f.member.id).await;
}
