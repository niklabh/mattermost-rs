//! Cross-server parity for `GET /api/v4/teams/{team_id}/channels/name/{channel_name}`.
//!
//! The by-name twin of `getChannel`, and it differs from that route in exactly the places a
//! port would copy across by reflex: the name is lower-cased before validation, a private
//! channel refuses with a **404** rather than a 403, a team admin is admitted to a private
//! channel through `manage_team`, the 403 it does issue names `read_public_channel`, and the
//! store's team filter admits a DM under any team's path.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_channel_by_name
//! ```

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable,
    go_minted_token, logged_in_user_id, purge_api_fixtures, stack_enabled,
};

async fn create_private_channel(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    tag: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "P",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the private fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// The plain success, byte for byte — and the same channel through a **mixed-case** segment,
/// which Go lower-cases before `RequireChannelName` would otherwise reject it.
#[tokio::test]
async fn the_channel_body_is_byte_identical_and_the_name_is_case_folded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "bynamebody").await;

    for segment in ["mmrs-parity-bynamebody", "MMRS-Parity-ByNameBody"] {
        let path = format!("/api/v4/teams/{team_id}/channels/name/{segment}");
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{segment}: the two servers must agree byte for byte"
        );
        let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
        assert_eq!(parsed["id"].as_str(), Some(channel_id.as_str()));
        assert_eq!(rs_body.last(), Some(&b'\n'), "encoder newline ([D-086])");
    }

    delete_channel(&client, &token, &channel_id).await;
}

/// `?include_deleted` selects the store variant: an archived channel is a 404 by default and a
/// 200 with `include_deleted=true`; `=yes` is `ParseBool`'s error case and so false.
#[tokio::test]
async fn include_deleted_is_the_only_way_to_an_archived_channel() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "bynamearch").await;
    delete_channel(&client, &token, &channel_id).await; // archives

    let base = format!("/api/v4/teams/{team_id}/channels/name/mmrs-parity-bynamearch");

    for query in ["", "?include_deleted=false", "?include_deleted=yes"] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &format!("{base}{query}")).await;
        assert_eq!(go_status, 404, "{query:?}");
        assert_eq!(rs_status, 404, "{query:?}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, query);
        assert_eq!(
            go["id"].as_str(),
            Some("app.channel.get_by_name.missing.app_error")
        );
    }

    for query in ["?include_deleted=true", "?include_deleted=1"] {
        let (go_body, rs_body) =
            fetch_both_stable(&client, &token, &format!("{base}{query}")).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{query}: both serve the archived channel"
        );
        let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
        assert_ne!(
            parsed["delete_at"].as_i64(),
            Some(0),
            "it really is archived"
        );
    }
}

/// A team member who never joined a public channel reads it through `read_public_channel` on
/// the team — the open branch's team gate.
#[tokio::test]
async fn a_team_member_reads_a_public_channel_by_name_without_joining() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "bynamepublic").await;
    let reader = create_plain_user(&client, &token, &team_id, "bynamereader").await;

    let path = format!("/api/v4/teams/{team_id}/channels/name/mmrs-parity-bynamepublic");
    let (go_body, rs_body) = fetch_both_stable(&client, &reader.token, &path).await;
    delete_channel(&client, &token, &channel_id).await;
    delete_plain_user(&client, &token, &reader.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
}

/// The non-open branch refuses a non-member with a **404** wearing the store's `missing` id —
/// indistinguishable from a channel that does not exist, which the second half of this test
/// asks for. `getChannel` answers the same caller a 403; this route must not.
#[tokio::test]
async fn a_non_member_gets_the_same_404_for_a_private_channel_as_for_no_channel() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let private_id = create_private_channel(&client, &token, &team_id, "bynamepriv").await;
    let reader = create_plain_user(&client, &token, &team_id, "bynameprivr").await;

    for name in ["mmrs-parity-bynamepriv", "mmrs-parity-bynamenosuch"] {
        let path = format!("/api/v4/teams/{team_id}/channels/name/{name}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &reader.token, &path).await;
        assert_eq!(go_status, 404, "{name}");
        assert_eq!(rs_status, 404, "{name}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, name);
        assert_eq!(
            go["id"].as_str(),
            Some("app.channel.get_by_name.missing.app_error"),
            "{name}: a private channel and a missing one are the same answer"
        );
    }

    delete_channel(&client, &token, &private_id).await;
    delete_plain_user(&client, &token, &reader.id).await;
}

/// The fixture user is a system admin, so `manage_team` admits it to a private channel it
/// never joined — the "allows team admins" branch. The test channel is created by a **plain**
/// user so the admin genuinely has no membership row.
#[tokio::test]
async fn manage_team_admits_a_non_member_to_a_private_channel() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let owner = create_plain_user(&client, &token, &team_id, "bynameowner").await;
    let private_id =
        create_private_channel(&client, &owner.token, &team_id, "bynameadminpriv").await;

    let path = format!("/api/v4/teams/{team_id}/channels/name/mmrs-parity-bynameadminpriv");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_channel(&client, &token, &private_id).await;
    delete_plain_user(&client, &token, &owner.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["type"].as_str(), Some("P"));
    assert_eq!(parsed["creator_id"].as_str(), Some(owner.id.as_str()));
}

/// The store's team filter is `TeamId = ? OR TeamId = ''`, so a DM — whose team is `""` — is
/// served under **any** team's path. Not the wildcard `getByNames` uses; the opposite rule.
#[tokio::test]
async fn a_direct_channel_answers_under_a_teams_path() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let other = create_plain_user(&client, &token, &team_id, "bynamedm").await;

    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([logged_in_user_id(), other.id]))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success());
    let dm: serde_json::Value = response.json().await.expect("the DM decodes");
    let dm_name = dm["name"].as_str().expect("a name").to_owned();

    let path = format!("/api/v4/teams/{team_id}/channels/name/{dm_name}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_plain_user(&client, &token, &other.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["type"].as_str(), Some("D"));
    assert_eq!(parsed["team_id"].as_str(), Some(""));
}

/// The two validation failures: a malformed team id and a name that passes the mux class but
/// fails `IsValidChannelIdentifier`. Both are `invalid_url_param` 400s from both servers.
#[tokio::test]
async fn malformed_segments_are_gos_400s() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;

    for path in [
        "/api/v4/teams/short/channels/name/town-square".to_owned(),
        format!("/api/v4/teams/{team_id}/channels/name/-leading"),
        format!("/api/v4/teams/{team_id}/channels/name/_"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, 400, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"].as_str(),
            Some("api.context.invalid_url_param.app_error")
        );
    }
}

/// A name segment outside `[A-Za-z0-9_-]+` never matches Go's route; the mux 404 must be Go's
/// own ([D-150]). A dot is the interesting case — legal in a username, not in a channel name.
#[tokio::test]
async fn a_segment_outside_the_name_class_is_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;

    for segment in ["town.square", "town%20square"] {
        let path = format!("/api/v4/teams/{team_id}/channels/name/{segment}");
        let response = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the Rust server answers");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{segment}: Go would not have routed this"
        );
        let status = response.status().as_u16();
        let body = response.bytes().await.expect("body reads").to_vec();
        assert_eq!(status, 404, "{segment}");

        let direct = client
            .get(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(direct.status().as_u16(), status);
        assert_eq!(
            String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
            String::from_utf8_lossy(&body)
        );
    }
}
