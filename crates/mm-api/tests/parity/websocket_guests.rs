//! Cross-server parity for what a **guest** hears on the websocket: `ShouldSendEventToGuest`
//! (web_conn.go:852), which asks `UserCanSeeOtherUser` whether this guest may see the user a
//! `user_updated` or `new_user` event is about.
//!
//! ```sh
//! scripts/parity.sh --test parity websocket_guests
//! ```
//!
//! # The fixture
//!
//! Runs on the licensed **guest** pair: a guest cannot log in on a server with guest accounts off.
//! The guest is left a member of one channel only, so the three subjects each answer a different
//! arm of `UserCanSeeOtherUser`:
//!
//! - `beside` shares that channel — the channel arm;
//! - `teammate` shares only the team — the team arm, which is true only if the guest holds
//!   `view_members` in the team (compared, not assumed);
//! - `stranger` shares neither.
//!
//! A plain listener on each server proves each event was raised, so a guest hearing nothing is a
//! decision and not a missing broadcast.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::common;

use common::{
    PLAIN_USER_PASSWORD, PlainUser, SocketProbe, add_user_to_channel, client, create_channel_typed,
    create_plain_user, create_team, go_minted_token, licensed_guest, plain_username, stack_enabled,
};

const GUEST_TAG: &str = "wsgg";

struct Fixture {
    guest_id: String,
    listener: PlainUser,
    beside: PlainUser,
    teammate: PlainUser,
    stranger: PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(http: &reqwest::Client, admin: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(http, admin, "wsg").await;
            let other_team = create_team(http, admin, "wsgo").await;
            let channel = create_channel_typed(http, admin, &team, "wsg", "O").await;
            let guest = create_plain_user(http, admin, &team, GUEST_TAG).await;
            let listener = create_plain_user(http, admin, &team, "wsgl").await;
            let beside = create_plain_user(http, admin, &team, "wsgb").await;
            let teammate = create_plain_user(http, admin, &team, "wsgt").await;
            let stranger = create_plain_user(http, admin, &other_team, "wsgx").await;
            add_user_to_channel(http, admin, &channel, &guest.id).await;
            add_user_to_channel(http, admin, &channel, &beside.id).await;

            // Guest in the three places the role lives, and a member of `channel` alone — the
            // team's default channels would otherwise make the teammate visible through them.
            let pool = common::fixture_pool().await.expect("the fixture database");
            for statement in [
                "UPDATE users SET roles = 'system_guest' WHERE id = $1",
                "UPDATE teammembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
                "UPDATE channelmembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
            ] {
                sqlx::query(statement)
                    .bind(&guest.id)
                    .execute(&pool)
                    .await
                    .expect("the guest fixture is written");
            }
            sqlx::query("DELETE FROM channelmembers WHERE userid = $1 AND channelid <> $2")
                .bind(&guest.id)
                .bind(&channel)
                .execute(&pool)
                .await
                .expect("the guest leaves every other channel");

            Fixture {
                guest_id: guest.id,
                listener,
                beside,
                teammate,
                stranger,
            }
        })
        .await
}

/// Log the guest in on the guest-enabled server; a session minted after the role change carries
/// `is_guest`.
async fn guest_token(http: &reqwest::Client, base: &str) -> String {
    let login = http
        .post(format!("{base}/api/v4/users/login"))
        .json(&json!({"login_id": plain_username(GUEST_TAG), "password": PLAIN_USER_PASSWORD}))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    assert_eq!(login.status(), 200, "the guest cannot log in on {base}");
    login
        .headers()
        .get("token")
        .and_then(|v| v.to_str().ok())
        .expect("a token header")
        .to_owned()
}

async fn served_by_rust(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust")
}

fn user_updated_for(frames: &[Value], user_id: &str) -> bool {
    frames
        .iter()
        .any(|f| f["event"] == "user_updated" && f["data"]["user"]["id"] == user_id)
}

fn new_user_for(frames: &[Value], user_id: &str) -> bool {
    frames
        .iter()
        .any(|f| f["event"] == "new_user" && f["data"]["user_id"] == user_id)
}

#[tokio::test]
async fn a_guest_hears_about_users_it_can_see_and_nobody_else() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let pair = licensed_guest().await;
    let f = fixture(&http, &admin).await;
    let token = guest_token(&http, &pair.go).await;

    let mut heard_by_server = Vec::new();
    for (round, (base, ours)) in [(pair.go.as_str(), false), (pair.rust.as_str(), true)]
        .into_iter()
        .enumerate()
    {
        let mut guest = SocketProbe::connect(base, &token).await;
        let mut listener = SocketProbe::connect(base, &f.listener.token).await;

        let subjects = [
            ("beside", &f.beside),
            ("teammate", &f.teammate),
            ("stranger", &f.stranger),
        ];
        for (_, user) in &subjects {
            let response = http
                .put(format!("{base}/api/v4/users/{}/patch", user.id))
                .header("Authorization", format!("Bearer {admin}"))
                .json(&json!({"nickname": format!("wsg-{round}")}))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
            assert!(response.status().is_success(), "{base}: patch failed");
            if ours {
                assert!(
                    served_by_rust(&response).await,
                    "the patch must be ours, or our hub raised nothing"
                );
            }
        }

        let username = plain_username(&format!("wsgn{round}"));
        let response = http
            .post(format!("{base}/api/v4/users"))
            .header("Authorization", format!("Bearer {admin}"))
            .json(&json!({
                "email": format!("{username}@mmrs.invalid"),
                "username": username,
                "password": PLAIN_USER_PASSWORD,
            }))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
        assert!(response.status().is_success(), "{base}: the create failed");
        if ours {
            assert!(served_by_rust(&response).await, "the create must be ours");
        }
        let created: Value = response.json().await.expect("a user");
        let new_id = created["id"].as_str().expect("an id").to_owned();

        let ids: Vec<String> = subjects.iter().map(|(_, u)| u.id.clone()).collect();
        let raised = {
            let new_id = new_id.clone();
            move |frames: &[Value]| {
                ids.iter().all(|id| user_updated_for(frames, id)) && new_user_for(frames, &new_id)
            }
        };
        assert!(
            listener.collect_until(Duration::from_secs(5), raised).await,
            "{base}: the plain listener did not hear every event: {:?}",
            listener.raw
        );
        // The opposite of hearing needs a window.
        guest.collect_for(Duration::from_millis(800)).await;

        let frames = guest.frames();
        let mut heard = BTreeMap::new();
        for (name, user) in &subjects {
            heard.insert(*name, user_updated_for(&frames, &user.id));
        }
        heard.insert("new user", new_user_for(&frames, &new_id));
        heard_by_server.push((base.to_owned(), heard));
    }

    let (go_base, go) = &heard_by_server[0];
    let (_, rust) = &heard_by_server[1];
    assert!(go["beside"], "{go_base}: the channel arm: {go:?}");
    assert!(!go["stranger"], "{go_base}: nothing shared: {go:?}");
    assert!(
        !go["new user"],
        "{go_base}: a new user shares nothing: {go:?}"
    );
    assert_eq!(
        go, rust,
        "what the guest heard differs (guest {})",
        f.guest_id
    );
}
