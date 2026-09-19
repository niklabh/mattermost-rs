//! Cross-server parity for `POST /api/v4/notifications/test` — the system bot's DM to the caller,
//! in the caller's locale, with `force_notification` set.
//!
//! The response is `{"status":"OK"}` on both; the claim worth testing is the **post** each server
//! writes. Each test sends one test message through each server into the same DM and compares the
//! two posts field by field, except the fields that are per-post by construction.
//!
//! ```sh
//! scripts/parity.sh --test parity test_notifications
//! ```

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, client, create_plain_user, create_team, delete_plain_user,
    go_minted_token, purge_api_fixtures, request_raw, stack_enabled,
};
use reqwest::Method;

const PATH: &str = "/api/v4/notifications/test";

/// Keys that differ between any two posts, whoever wrote them.
const PER_POST: &[&str] = &["id", "create_at", "update_at", "edit_at"];

async fn system_bot_id(client: &reqwest::Client, admin: &str) -> String {
    let bot: serde_json::Value = client
        .get(format!("{GO}/api/v4/users/username/system-bot"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a user");
    bot["id"]
        .as_str()
        .expect("the system bot exists")
        .to_owned()
}

async fn set_locale(client: &reqwest::Client, admin: &str, user_id: &str, locale: &str) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "locale": locale }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "setting the locale failed");
}

async fn send(client: &reqwest::Client, base: &str, token: &str) {
    let (status, body, served) =
        request_raw(client, base, Method::POST, Some(token), PATH, None).await;
    assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
    assert_eq!(
        body, br#"{"status":"OK"}"#,
        "{base}: ReturnStatusOK, no newline"
    );
    if base == RUST {
        assert_eq!(served.as_deref(), Some("rust"), "the route is served here");
    }
}

/// The DM's posts, newest first, through Go.
async fn dm_posts(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    bot_id: &str,
) -> Vec<serde_json::Value> {
    let channel: serde_json::Value = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([user_id, bot_id]))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the DM");
    let channel_id = channel["id"].as_str().expect("a DM id");
    let list: serde_json::Value = client
        .get(format!("{GO}/api/v4/channels/{channel_id}/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a post list");
    list["order"]
        .as_array()
        .expect("an order")
        .iter()
        .map(|id| list["posts"][id.as_str().expect("an id")].clone())
        .collect()
}

/// Everything but the per-post keys and the prop that holds a fresh id.
fn comparable(post: &serde_json::Value) -> serde_json::Value {
    let mut post = post.clone();
    let object = post.as_object_mut().expect("an object");
    for key in PER_POST {
        object.remove(*key);
    }
    let force = object["props"]["force_notification"]
        .as_str()
        .unwrap_or_else(|| panic!("force_notification is a string id: {}", object["props"]))
        .to_owned();
    assert_eq!(force.len(), 26, "a fresh id");
    object["props"]["force_notification"] = serde_json::Value::String("<id>".to_owned());
    post
}

/// Send through `first` then `second`, and compare the two posts. Returns the message.
async fn compare(tag: &str, locale: Option<&str>, rust_first: bool) -> String {
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, tag).await;
    let user = create_plain_user(&client, &admin, &team, tag).await;
    if let Some(locale) = locale {
        set_locale(&client, &admin, &user.id, locale).await;
    }
    let bot = system_bot_id(&client, &admin).await;

    // The first call creates the DM, so each order proves a different server can.
    let (first, second) = if rust_first { (RUST, GO) } else { (GO, RUST) };
    send(&client, first, &user.token).await;
    send(&client, second, &user.token).await;

    let posts = dm_posts(&client, &user.token, &user.id, &bot).await;
    assert_eq!(posts.len(), 2, "one post per server: {posts:?}");
    let (newer, older) = (&posts[0], &posts[1]);
    assert_eq!(comparable(newer), comparable(older));
    assert_eq!(newer["user_id"], bot.as_str(), "the system bot posts");
    assert_eq!(newer["props"]["from_bot"], "true");
    assert_ne!(
        newer["props"]["force_notification"], older["props"]["force_notification"],
        "each post gets its own id"
    );

    delete_plain_user(&client, &admin, &user.id).await;
    newer["message"].as_str().expect("a message").to_owned()
}

#[tokio::test]
async fn the_test_post_matches_when_rust_creates_the_dm() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let message = compare("notiftestrs", None, true).await;
    assert_eq!(
        message,
        "If you received this test notification, it worked!"
    );
}

#[tokio::test]
async fn the_test_post_matches_when_go_creates_the_dm() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    compare("notiftestgo", None, false).await;
}

/// The caller's locale picks the text: German is translated in Go's bundle.
#[tokio::test]
async fn a_german_user_gets_the_german_message() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let message = compare("notiftestde", Some("de"), true).await;
    assert_eq!(
        message,
        "Wenn du diese Testbenachrichtigung erhalten hast, hat es funktioniert!"
    );
}

/// Spanish is a supported locale whose file lacks the id: English, by `tfuncWithFallback`.
#[tokio::test]
async fn an_untranslated_locale_falls_back_to_english() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let message = compare("notiftestes", Some("es"), true).await;
    assert_eq!(
        message,
        "If you received this test notification, it worked!"
    );
}

/// `APISessionRequired`: no session is the 401 on both.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let (go_status, go_body, _) = request_raw(&client, GO, Method::POST, None, PATH, None).await;
    let (rs_status, rs_body, _) = request_raw(&client, RUST, Method::POST, None, PATH, None).await;
    assert_eq!((go_status, rs_status), (401, 401));
    common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "no session");
}

/// The `posted` event each server publishes for the test post, projected onto what is not
/// per-post. `set_online` is `false`: Go passes only `ForceNotification`, and a port that reused
/// the REST create's flags would say `true` here and nowhere else.
#[tokio::test]
async fn the_posted_event_matches_and_does_not_set_online() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "notiftestws").await;
    let user = create_plain_user(&client, &admin, &team, "notiftestws").await;
    let bot = system_bot_id(&client, &admin).await;

    let mut frames = Vec::new();
    for base in [GO, RUST] {
        let mut probe = SocketProbe::connect(base, &user.token).await;
        send(&client, base, &user.token).await;
        let wanted = bot.clone();
        let arrived = probe
            .collect_until(Duration::from_secs(8), move |collected| {
                collected.iter().any(|frame| {
                    frame.get("event").and_then(|e| e.as_str()) == Some("posted")
                        && frame["data"]["post"]
                            .as_str()
                            .is_some_and(|p| p.contains(&wanted))
                })
            })
            .await;
        assert!(
            arrived,
            "{base} published no `posted` event: {:?}",
            probe.raw
        );
        let frame = probe
            .events_named("posted")
            .into_iter()
            .find(|frame| {
                frame["data"]["post"]
                    .as_str()
                    .is_some_and(|p| p.contains(&bot))
            })
            .expect("the test post's event");
        let data = frame["data"].as_object().expect("a data object").clone();
        let post: serde_json::Value =
            serde_json::from_str(data["post"].as_str().expect("a post string"))
                .expect("the post decodes");
        frames.push(serde_json::json!({
            "channel_type": data.get("channel_type"),
            "channel_display_name": data.get("channel_display_name"),
            "channel_name": data.get("channel_name"),
            "sender_name": data.get("sender_name"),
            "team_id": data.get("team_id"),
            "set_online": data.get("set_online"),
            "mentions": data.get("mentions"),
            "message": post["message"].clone(),
            "from_bot": post["props"]["from_bot"].clone(),
            "broadcast_channel_id": frame["broadcast"]["channel_id"].clone(),
        }));
    }

    assert_eq!(
        frames[0], frames[1],
        "the `posted` events differ\n go: {:#?}\nrust: {:#?}",
        frames[0], frames[1]
    );
    assert_eq!(frames[1]["set_online"], serde_json::json!(false));
    delete_plain_user(&client, &admin, &user.id).await;
}
