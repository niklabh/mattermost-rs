//! Cross-server parity for `POST /api/v4/posts` into a **direct** or **group** message.
//!
//! ```sh
//! scripts/parity.sh --test parity post_create_dm
//! ```
//!
//! A DM post mentions the other side (`DMMention`), a group message mentions every member
//! (`GMMention`), the `posted` event names the channel differently for each (the sender with
//! an `@`; the sorted member list), and its `team_id` is empty. The one DM write this server
//! does not do is the auto-response, forwarded when the receiver has it switched on.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, client, create_direct_channel, create_plain_user,
    create_team, fixture_pool, go_minted_token, logged_in_user_id, purge_api_fixtures,
    stack_enabled, username_of,
};

struct Fixture {
    team_id: String,
    /// The admin's DM with `reader`.
    dm_id: String,
    /// A GM of the admin, `reader` and `other`.
    gm_id: String,
    reader: common::PlainUser,
    other: common::PlainUser,
    admin_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "cpdm").await;
            let reader = create_plain_user(client, token, &team_id, "cpdm").await;
            let other = create_plain_user(client, token, &team_id, "cpdmo").await;
            let dm_id = create_direct_channel(client, token, logged_in_user_id(), &reader.id).await;
            let response = client
                .post(format!("{GO}/api/v4/channels/group"))
                .header("Authorization", format!("Bearer {token}"))
                .json(&serde_json::json!([
                    logged_in_user_id(),
                    reader.id,
                    other.id
                ]))
                .send()
                .await
                .expect("Go answers");
            assert!(response.status().is_success(), "creating the GM failed");
            let gm: serde_json::Value = response.json().await.expect("a channel");
            Fixture {
                team_id,
                dm_id,
                gm_id: gm["id"].as_str().expect("an id").to_owned(),
                reader,
                other,
                admin_name: username_of(client, token, logged_in_user_id()).await,
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
) -> (u16, bool, serde_json::Value) {
    let response = client
        .post(format!("{base}/api/v4/posts"))
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
        response.json().await.unwrap_or(serde_json::Value::Null),
    )
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

async fn rows_with_message(channel_id: &str, message: &str) -> i64 {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE channelid = $1 AND message = $2")
        .bind(channel_id)
        .bind(message)
        .fetch_one(&pool)
        .await
        .expect("counts")
}

async fn patch_notify_props(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    props: serde_json::Value,
) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "notify_props": props }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "patching {user_id} failed");
}

fn default_props() -> serde_json::Value {
    serde_json::json!({
        "channel": "true", "comments": "never", "desktop": "mention", "desktop_sound": "true",
        "email": "true", "first_name": "false", "mention_keys": "", "push": "mention",
        "push_status": "online", "auto_responder_active": "false", "auto_responder_message": "",
    })
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// A DM post is served, answers Go's shape, and mentions the other side: their counters move
/// by one on both, the sender's by none.
#[tokio::test]
async fn a_direct_message_is_served_and_mentions_the_other_side() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // A DM of its own: the event test posts into the fixture's, and a counter read there could
    // see its post land between "before" and "after".
    let partner = create_plain_user(&client, &token, &f.team_id, "cpdmc").await;
    let dm_id = create_direct_channel(&client, &token, logged_in_user_id(), &partner.id).await;
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let before = mentions_of(&dm_id, &partner.id).await;
        let mine_before = mentions_of(&dm_id, logged_in_user_id()).await;
        let (status, served, body) = create(&client, base, &token, &dm_id, "cpdm hello").await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}: served by");
        let after = mentions_of(&dm_id, &partner.id).await;
        assert_eq!(
            (after.0 - before.0, after.1 - before.1),
            (1, 1),
            "{base}: the other side is mentioned by every DM post"
        );
        let mine_after = mentions_of(&dm_id, logged_in_user_id()).await;
        assert_eq!(mine_after, mine_before, "{base}: the sender is not");
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1], "the DM post bodies differ");

    // And from the other side, so that whichever of the two users sorts first in the channel's
    // name is the poster once: `getExplicitMentionsAndKeywords` tests each named user against
    // the poster separately, and a wrong test on the first name is invisible while only the
    // second user ever posts. Measured: a mutation there survived until this half existed.
    for base in [GO, RUST] {
        let mine_before = mentions_of(&dm_id, logged_in_user_id()).await;
        let (status, served, body) =
            create(&client, base, &partner.token, &dm_id, "cpdm hello back").await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let mine_after = mentions_of(&dm_id, logged_in_user_id()).await;
        assert_eq!(
            (mine_after.0 - mine_before.0, mine_after.1 - mine_before.1),
            (1, 1),
            "{base}: the admin is the other side now"
        );
        // The poster's own counters are **zero** afterwards — not "unchanged": a root post is
        // followed by `MarkChannelsAsViewed`, which clears the poster's mentions on both.
        assert_eq!(
            mentions_of(&dm_id, &partner.id).await,
            (0, 0),
            "{base}: the poster's counters are cleared by the view that follows"
        );
    }
    common::delete_plain_user(&client, &token, &partner.id).await;
}

/// A group message mentions every other member once, on both.
#[tokio::test]
async fn a_group_message_mentions_every_other_member() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // A GM of its own, for the same reason as the DM test's.
    let third = create_plain_user(&client, &token, &f.team_id, "cpdmg").await;
    let response = client
        .post(format!("{GO}/api/v4/channels/group"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([
            logged_in_user_id(),
            f.reader.id,
            third.id
        ]))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "creating the GM failed");
    let gm: serde_json::Value = response.json().await.expect("a channel");
    let gm_id = gm["id"].as_str().expect("an id").to_owned();
    for base in [GO, RUST] {
        let reader_before = mentions_of(&gm_id, &f.reader.id).await;
        let other_before = mentions_of(&gm_id, &third.id).await;
        let mine_before = mentions_of(&gm_id, logged_in_user_id()).await;
        let (status, served, body) =
            create(&client, base, &token, &gm_id, "cpdm group hello").await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let reader_after = mentions_of(&gm_id, &f.reader.id).await;
        let other_after = mentions_of(&gm_id, &third.id).await;
        assert_eq!(
            (
                reader_after.0 - reader_before.0,
                reader_after.1 - reader_before.1
            ),
            (1, 1),
            "{base}: reader"
        );
        assert_eq!(
            (
                other_after.0 - other_before.0,
                other_after.1 - other_before.1
            ),
            (1, 1),
            "{base}: other"
        );
        assert_eq!(
            mentions_of(&gm_id, logged_in_user_id()).await,
            mine_before,
            "{base}: not the poster"
        );
    }
    common::delete_plain_user(&client, &token, &third.id).await;
}

/// The `posted` event's names: a DM's `channel_display_name` is the **sender** with an `@`, a
/// group message's is every member's display name sorted and comma-joined, and `team_id` is
/// empty on both — compared frame against frame on the reader's socket.
#[tokio::test]
async fn the_posted_event_names_the_channel_by_type() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for (label, channel_id, message) in [
        ("dm", &f.dm_id, "cpdm ev dm"),
        ("gm", &f.gm_id, "cpdm ev gm"),
    ] {
        let mut frames = Vec::new();
        for base in [GO, RUST] {
            let mut socket = SocketProbe::connect(base, &f.reader.token).await;
            let (status, _, body) = create(&client, base, &token, channel_id, message).await;
            assert_eq!(status, 201, "{base} {label}: {body}");
            let post_id = body["id"].as_str().expect("an id").to_owned();
            let want = |fr: &[serde_json::Value]| {
                fr.iter().any(|f| {
                    f["event"] == "posted"
                        && f["data"]["post"]
                            .as_str()
                            .is_some_and(|p| p.contains(&post_id))
                })
            };
            assert!(
                socket
                    .collect_until(Duration::from_millis(2500), want)
                    .await,
                "{base} {label}: no posted frame: {:?}",
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
            let mut data = frame["data"].clone();
            data.as_object_mut().unwrap().remove("post");
            frames.push((data, frame["broadcast"].clone()));
        }
        assert_eq!(frames[0].0, frames[1].0, "{label}: the posted data differs");
        assert_eq!(frames[0].1, frames[1].1, "{label}: the addressing differs");
        let data = &frames[1].0;
        assert_eq!(
            data["team_id"], "",
            "{label}: a DM/GM has no team on the event"
        );
        assert_eq!(
            data["mentions"],
            serde_json::Value::String(format!("[\"{}\"]", f.reader.id)),
            "{label}"
        );
        match label {
            "dm" => {
                assert_eq!(data["channel_type"], "D");
                assert_eq!(
                    data["channel_display_name"],
                    format!("@{}", f.admin_name),
                    "the sender, with an @"
                );
            }
            _ => {
                assert_eq!(data["channel_type"], "G");
                let name = data["channel_display_name"].as_str().expect("a name");
                let other_name = username_of(&client, &token, &f.other.id).await;
                assert!(name.contains(&other_name), "every member is named: {name}");
                let mut names: Vec<&str> = name.split(", ").collect();
                assert_eq!(names.len(), 3, "every member: {name}");
                let joined = names.join(", ");
                names.sort();
                assert_eq!(names.join(", "), joined, "sorted: {name}");
            }
        }
    }
}

/// A DM whose receiver has the auto-responder on is forwarded: Go writes the auto-response as a
/// second post, and this server writes nothing. With it off, served.
#[tokio::test]
async fn an_active_auto_responder_forwards_the_dm_and_go_writes_the_response() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    // A DM of its own, so the response post does not land in the shared one.
    let responder = create_plain_user(&client, &token, &f.team_id, "cpdmr").await;
    let dm = create_direct_channel(&client, &token, logged_in_user_id(), &responder.id).await;

    let mut props = default_props();
    props["auto_responder_active"] = serde_json::json!("true");
    props["auto_responder_message"] = serde_json::json!("cpdm out of office");
    patch_notify_props(&client, &token, &responder.id, props).await;

    let (status, served, body) = create(&client, RUST, &token, &dm, "cpdm are you there").await;
    assert_eq!(status, 201, "{body}");
    assert!(!served, "an active auto-responder is forwarded");
    // Go's goroutine writes the response; wait for it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while rows_with_message(&dm, "cpdm out of office").await == 0
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        rows_with_message(&dm, "cpdm are you there").await,
        1,
        "one row for the message"
    );
    assert_eq!(
        rows_with_message(&dm, "cpdm out of office").await,
        1,
        "and Go's auto-response"
    );

    // Off again: served, no response.
    patch_notify_props(&client, &token, &responder.id, default_props()).await;
    let (status, served, _) = create(&client, RUST, &token, &dm, "cpdm still there").await;
    assert_eq!(status, 201);
    assert!(served, "with the auto-responder off the DM is served");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rows_with_message(&dm, "cpdm out of office").await,
        1,
        "no second response"
    );

    common::delete_plain_user(&client, &token, &responder.id).await;
}

/// A self-DM mentions nobody: `GetBothUsersForDM` names the sender twice and the sender is not
/// the other side of themselves.
#[tokio::test]
async fn a_self_dm_mentions_nobody() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let me = create_direct_channel(&client, &token, logged_in_user_id(), logged_in_user_id()).await;
    let _ = &f.team_id;

    for base in [GO, RUST] {
        let before = mentions_of(&me, logged_in_user_id()).await;
        let (status, served, body) = create(&client, base, &token, &me, "cpdm note to self").await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            mentions_of(&me, logged_in_user_id()).await,
            before,
            "{base}: no mention"
        );
    }
}
