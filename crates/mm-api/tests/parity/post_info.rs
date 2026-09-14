//! Cross-server parity for `GET /api/v4/posts/{post_id}/info` — the permalink preflight.
//!
//! ```sh
//! scripts/parity.sh --test parity post_info
//! ```
//!
//! The route answers for posts the caller cannot read, which is its point: a link into a channel
//! the user has not joined is described (channel and team names, whether they have joined either)
//! so the client can offer the join. Two layers decide — the team, then the channel — and every
//! refusal is the one 404 `app.post.get.app_error`, so a probe learns nothing.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_direct_channel, create_plain_user, create_team, go_minted_token,
    logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    /// An open channel; the admin and `member` are in it.
    open_post: String,
    /// A private channel; only the admin is in it.
    private_post: String,
    /// A DM between the admin and `member`.
    dm_post: String,
    /// In the team and the open channel.
    member: common::PlainUser,
    /// In the team, in neither channel.
    teammate: common::PlainUser,
    /// In another, invite-only team; not in this one.
    stranger: common::PlainUser,
    /// Was in the team and the open channel, then removed from the team — a `TeamMembers` row
    /// with a `DeleteAt`, which is not the same as no row.
    leaver: common::PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn patch_team(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    patch: serde_json::Value,
) {
    let response = client
        .put(format!("{GO}/api/v4/teams/{team_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&patch)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "patching {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "pinfo").await;
            let other_team = create_team(client, token, "pinfoo").await;
            let open_id = create_channel_typed(client, token, &team_id, "pinfo", "O").await;
            let private_id = create_channel_typed(client, token, &team_id, "pinfop", "P").await;
            let member = create_plain_user(client, token, &team_id, "pinfo").await;
            let teammate = create_plain_user(client, token, &team_id, "pinfot").await;
            let stranger = create_plain_user(client, token, &other_team, "pinfos").await;
            let leaver = create_plain_user(client, token, &team_id, "pinfol").await;
            add_user_to_channel(client, token, &open_id, &member.id).await;
            add_user_to_channel(client, token, &open_id, &leaver.id).await;
            let response = client
                .delete(format!("{GO}/api/v4/teams/{team_id}/members/{}", leaver.id))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers");
            assert!(response.status().is_success(), "removing the leaver failed");
            let dm_id = create_direct_channel(client, token, logged_in_user_id(), &member.id).await;
            Fixture {
                team_id,
                open_post: post_message(client, token, &open_id, "pinfo open", None).await,
                private_post: post_message(client, token, &private_id, "pinfo private", None).await,
                dm_post: post_message(client, token, &dm_id, "pinfo dm", None).await,
                member,
                teammate,
                stranger,
                leaver,
            }
        })
        .await
}

async fn info(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .get(format!("{base}/api/v4/posts/{post_id}/info"))
        .header("Authorization", format!("Bearer {token}"))
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
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

/// Both bodies, byte for byte, after asserting the status and that ours was served.
async fn both(client: &reqwest::Client, token: &str, post_id: &str, status: u16) -> Vec<u8> {
    let (go_status, _, go) = info(client, GO, token, post_id).await;
    let (rs_status, served, rs) = info(client, RUST, token, post_id).await;
    assert_eq!(go_status, status, "Go: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, status, "Rust: {}", String::from_utf8_lossy(&rs));
    assert!(served, "served here");
    if status == 200 {
        assert_eq!(
            go,
            rs,
            "byte-identical: go={} rust={}",
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs)
        );
    } else {
        assert_error_bodies_match_except_known_gaps(&go, &rs, post_id);
    }
    go
}

/// A member of the team and the open channel sees everything joined; the body has no trailing
/// newline (`json.Marshal`, not the encoder).
#[tokio::test]
async fn a_channel_member_gets_the_full_description() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let body = both(&client, &f.member.token, &f.open_post, 200).await;
    assert!(!body.ends_with(b"\n"), "json.Marshal writes no newline");
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("a PostInfo");
    assert_eq!(parsed["has_joined_channel"], true);
    assert_eq!(parsed["has_joined_team"], true);
    assert_eq!(parsed["team_id"], f.team_id.as_str());
    assert_eq!(parsed["team_type"], "I", "a fresh team is invite-only");
    assert_eq!(parsed["channel_type"], "O");
}

/// A teammate who has not joined the open channel may still learn about it — `has_joined_channel`
/// is `false` — and a private channel they are not in is the 404.
#[tokio::test]
async fn a_teammate_outside_the_channel_sees_an_open_one_and_not_a_private_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let body = both(&client, &f.teammate.token, &f.open_post, 200).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("a PostInfo");
    assert_eq!(parsed["has_joined_channel"], false);
    assert_eq!(parsed["has_joined_team"], true);

    both(&client, &f.teammate.token, &f.private_post, 404).await;
}

/// A DM has no team: the team fields are empty and the team layer passes outright. The other
/// side of the DM sees it; a teammate who is not in it gets the 404.
#[tokio::test]
async fn a_dm_carries_no_team_and_is_visible_to_its_members_only() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let body = both(&client, &f.member.token, &f.dm_post, 200).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("a PostInfo");
    assert_eq!(parsed["channel_type"], "D");
    assert_eq!(parsed["team_id"], "");
    assert_eq!(parsed["team_type"], "");
    assert_eq!(parsed["has_joined_team"], false);
    assert_eq!(parsed["has_joined_channel"], true);

    both(&client, &f.teammate.token, &f.dm_post, 404).await;
}

/// A user from another team, and a user removed from this one (a membership row with a
/// `DeleteAt`, which the team layer treats as no membership): refused on an invite-only team,
/// and described — with both joined flags `false` and `team_type` `O` — once the team allows
/// open invites, because a system user may `join_public_teams`.
#[tokio::test]
async fn a_stranger_is_refused_by_an_invite_team_and_described_by_an_open_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    both(&client, &f.stranger.token, &f.open_post, 404).await;
    both(&client, &f.leaver.token, &f.open_post, 404).await;

    patch_team(
        &client,
        &token,
        &f.team_id,
        serde_json::json!({ "allow_open_invite": true }),
    )
    .await;
    let body = both(&client, &f.stranger.token, &f.open_post, 200).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("a PostInfo");
    assert_eq!(parsed["team_type"], "O");
    assert_eq!(parsed["has_joined_team"], false);
    assert_eq!(parsed["has_joined_channel"], false);
    // The leaver's deleted membership reads as not joined, on both.
    let body = both(&client, &f.leaver.token, &f.open_post, 200).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("a PostInfo");
    assert_eq!(parsed["has_joined_team"], false, "{parsed}");
    assert_eq!(parsed["has_joined_channel"], false, "{parsed}");
    // The private channel stays a 404 even on an open team.
    both(&client, &f.stranger.token, &f.private_post, 404).await;
    patch_team(
        &client,
        &token,
        &f.team_id,
        serde_json::json!({ "allow_open_invite": false }),
    )
    .await;
}

/// A malformed id is the 400 before anything is read; an unknown one is the 404. The malformed
/// id has to pass the mux class `[A-Za-z0-9]+` to reach the handler at all — a short one does,
/// and `IsValidId` refuses it on length — where a hyphenated one is the router's own 404, and
/// an uppercase one is *valid* (`unicode.IsLetter`) and merely unknown.
#[tokio::test]
async fn a_bad_id_and_a_missing_post_are_refused_alike_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let _ = f;

    both(&client, &token, "abc", 400).await;
    both(&client, &token, "zzzzzzzzzzzzzzzzzzzzzzzzzz", 404).await;
    both(&client, &token, "ZZZZZZZZZZZZZZZZZZZZZZZZZZ", 404).await;
}
