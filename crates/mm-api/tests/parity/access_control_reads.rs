//! Cross-server parity for the three access-control reads — channel attributes, team attributes,
//! team policy — on a build whose access-control service is nil.
//!
//! ```sh
//! scripts/parity.sh --test parity access_control_reads
//! ```
//!
//! Both servers are the same build with the same nil service, so the two attribute reads are a
//! permission check and the 501 `app.pap.get_channel_access_control_attributes.app_error`, and the
//! policy read is a permission check and `{"policy":null,"enforced":false}` — byte for byte,
//! with no trailing newline. The `{}` arm behind `EnableChannelPolicyIndicators` (off) is not
//! measured: the setting is on for the stack.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, go_minted_token, stack_enabled,
};

struct Fixture {
    team_id: String,
    channel_id: String,
    /// In the team and the channel.
    member: common::PlainUser,
    /// In another team.
    stranger: common::PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team_id = create_team(client, token, "acl").await;
            let other = create_team(client, token, "aclo").await;
            let channel_id = create_channel_typed(client, token, &team_id, "acl", "P").await;
            let member = create_plain_user(client, token, &team_id, "acl").await;
            let stranger = create_plain_user(client, token, &other, "aclo").await;
            add_user_to_channel(client, token, &channel_id, &member.id).await;
            Fixture {
                team_id,
                channel_id,
                member,
                stranger,
            }
        })
        .await
}

async fn get(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .get(format!("{base}{path}"))
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

/// Both servers, the expected status, ours served; error bodies compared around the known gaps
/// and success bodies byte for byte. Returns Go's body.
async fn both(client: &reqwest::Client, token: &str, path: &str, status: u16) -> Vec<u8> {
    let (go_status, _, go) = get(client, GO, token, path).await;
    let (rs_status, served, rs) = get(client, RUST, token, path).await;
    assert_eq!(
        go_status,
        status,
        "Go {path}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {path}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served, "{path}: served here");
    if status < 400 {
        assert_eq!(
            go,
            rs,
            "{path}: go={} rust={}",
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs)
        );
    } else {
        assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    }
    go
}

/// A channel member gets the nil-service 501; a non-member the 403.
#[tokio::test]
async fn channel_attributes_are_the_nil_service_501_behind_read_channel() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!(
        "/api/v4/channels/{}/access_control/attributes",
        f.channel_id
    );

    let body = both(&client, &f.member.token, &path, 501).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert_eq!(
        parsed["id"],
        "app.pap.get_channel_access_control_attributes.app_error"
    );
    both(&client, &token, &path, 501).await;
    both(&client, &f.stranger.token, &path, 403).await;
}

/// A team member gets the same 501 behind `view_team`; a stranger the 403.
#[tokio::test]
async fn team_attributes_are_the_nil_service_501_behind_view_team() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/access_control/attributes", f.team_id);

    let body = both(&client, &f.member.token, &path, 501).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert_eq!(
        parsed["id"],
        "app.pap.get_channel_access_control_attributes.app_error"
    );
    both(&client, &f.stranger.token, &path, 403).await;
}

/// The policy read: the admin gets the empty document with no trailing newline; a plain member
/// lacks `manage_team_access_rules` and gets the 403; a **team admin** holds that permission
/// without `manage_system`, and the gate is an *or*, so they get the document too.
#[tokio::test]
async fn team_policy_is_the_empty_document_while_abac_is_off() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/access_control/policy", f.team_id);

    let body = both(&client, &token, &path, 200).await;
    assert_eq!(
        body, br#"{"policy":null,"enforced":false}"#,
        "json.Marshal, no newline"
    );
    both(&client, &f.member.token, &path, 403).await;

    let team_admin = create_plain_user(&client, &token, &f.team_id, "aclta").await;
    let response = client
        .put(format!(
            "{GO}/api/v4/teams/{}/members/{}/schemeRoles",
            f.team_id, team_admin.id
        ))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "scheme_user": true, "scheme_admin": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "promoting the team admin");
    let body = both(&client, &team_admin.token, &path, 200).await;
    assert_eq!(body, br#"{"policy":null,"enforced":false}"#);
}

/// A malformed id is the 400 on all three.
#[tokio::test]
async fn a_bad_id_is_the_400_on_all_three() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for path in [
        "/api/v4/channels/abc/access_control/attributes",
        "/api/v4/teams/abc/access_control/attributes",
        "/api/v4/teams/abc/access_control/policy",
    ] {
        both(&client, &token, path, 400).await;
    }
}
