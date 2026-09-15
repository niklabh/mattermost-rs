//! Cross-server parity for `POST /api/v4/data_retention/policies/{policy_id}/teams/search` and
//! `POST …/{policy_id}/channels/search` — `searchTeamsInPolicy` and `searchChannelsInPolicy`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity retention_search
//! ```
//!
//! No route creates a retention policy on this stack (the writes are the enterprise 501), so
//! the suite plants one policy row and one membership row per table, pointing at a team and a
//! channel of its own (prefix `mmrsretentionsearch`, outside the shared purge's reach — the
//! shared helper's first team can be a `mmrs-parity-` row another suite's purge deletes
//! mid-run), under a fixed 26-character id — and removes them first and last.

use crate::common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, assert_error_bodies_match_except_known_gaps,
    client, create_plain_user, fixture_pool, go_minted_token, post_both_raw, stack_enabled,
};

const POLICY: &str = "mmrsretentionsearch0000001";
const PREFIX: &str = "mmrsretentionsearch";

async fn go_post(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "POST {path} failed: {}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.expect("the response decodes")
}

/// A team and a public channel of this suite's own, made by the admin through Go. The display
/// names carry `<&>`, which `json.Marshal` escapes and the encoder would not.
async fn own_team_and_channel(client: &reqwest::Client, admin: &str) -> (String, String) {
    let team = go_post(
        client,
        admin,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}-team"),
            "display_name": "mmrs retention search <&>",
            "type": "O",
        }),
    )
    .await["id"]
        .as_str()
        .expect("a team id")
        .to_owned();
    let channel = go_post(
        client,
        admin,
        "/api/v4/channels",
        serde_json::json!({
            "team_id": team,
            "name": format!("{PREFIX}-channel"),
            "display_name": "mmrs retention search channel <&>",
            "type": "O",
        }),
    )
    .await["id"]
        .as_str()
        .expect("a channel id")
        .to_owned();
    (team, channel)
}

/// Remove the suite's own rows — the policy first, then the channel and the team.
async fn purge_own_rows(pool: &sqlx::PgPool) {
    unplant_policy(pool).await;
    const TEAMS: &str = "SELECT id FROM teams WHERE name LIKE 'mmrsretentionsearch%'";
    for statement in [
        format!(
            "DELETE FROM sidebarchannels WHERE categoryid IN (SELECT id FROM sidebarcategories WHERE teamid IN ({TEAMS}))"
        ),
        format!("DELETE FROM sidebarcategories WHERE teamid IN ({TEAMS})"),
        format!(
            "DELETE FROM channelmemberhistory WHERE channelid IN (SELECT id FROM channels WHERE teamid IN ({TEAMS}))"
        ),
        format!(
            "DELETE FROM channelmembers WHERE channelid IN (SELECT id FROM channels WHERE teamid IN ({TEAMS}))"
        ),
        format!(
            "DELETE FROM posts WHERE channelid IN (SELECT id FROM channels WHERE teamid IN ({TEAMS}))"
        ),
        format!("DELETE FROM publicchannels WHERE teamid IN ({TEAMS})"),
        format!("DELETE FROM channels WHERE teamid IN ({TEAMS})"),
        format!("DELETE FROM teammembers WHERE teamid IN ({TEAMS})"),
        "DELETE FROM teams WHERE name LIKE 'mmrsretentionsearch%'".to_owned(),
    ] {
        let _ = sqlx::query(&statement).execute(pool).await;
    }
}

fn teams_path(policy: &str) -> String {
    format!("/api/v4/data_retention/policies/{policy}/teams/search")
}

fn channels_path(policy: &str) -> String {
    format!("/api/v4/data_retention/policies/{policy}/channels/search")
}

/// The policy and its two memberships, planted for the admin's team and channel.
async fn plant_policy(team: &str, channel: &str) -> sqlx::PgPool {
    let pool = fixture_pool()
        .await
        .expect("DATABASE_URL is set for the parity stack");
    unplant_policy(&pool).await;
    sqlx::query("INSERT INTO retentionpolicies (id, displayname, postduration) VALUES ($1, 'mmrs retention search', 30)")
        .bind(POLICY)
        .execute(&pool)
        .await
        .expect("the policy is planted");
    sqlx::query("INSERT INTO retentionpoliciesteams (policyid, teamid) VALUES ($1, $2) ON CONFLICT (teamid) DO UPDATE SET policyid = $1")
        .bind(POLICY)
        .bind(team)
        .execute(&pool)
        .await
        .expect("the team membership is planted");
    sqlx::query("INSERT INTO retentionpolicieschannels (policyid, channelid) VALUES ($1, $2) ON CONFLICT (channelid) DO UPDATE SET policyid = $1")
        .bind(POLICY)
        .bind(channel)
        .execute(&pool)
        .await
        .expect("the channel membership is planted");
    pool
}

async fn unplant_policy(pool: &sqlx::PgPool) {
    for statement in [
        "DELETE FROM retentionpoliciesteams WHERE policyid = $1",
        "DELETE FROM retentionpolicieschannels WHERE policyid = $1",
        "DELETE FROM retentionpolicies WHERE id = $1",
    ] {
        let _ = sqlx::query(statement).bind(POLICY).execute(pool).await;
    }
}

/// Both servers, same status, byte-identical bodies; Go's decoded answer comes back.
async fn both_agree(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
    context: &str,
) -> serde_json::Value {
    let ((go_status, go), (rs_status, rs)) = post_both_raw(client, token, path, body).await;
    assert_eq!(
        go_status,
        200,
        "{context}: Go: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        200,
        "{context}: Rust: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{context}"
    );
    serde_json::from_slice(&go).unwrap_or_else(|e| panic!("{context}: {e}"))
}

async fn both_refuse(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
    expected: u16,
    context: &str,
) -> serde_json::Value {
    let ((go_status, go), (rs_status, rs)) = post_both_raw(client, token, path, body).await;
    assert_eq!(
        go_status,
        expected,
        "{context}: Go: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        expected,
        "{context}: Rust: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    serde_json::from_slice(&go).unwrap_or_else(|e| panic!("{context}: {e}"))
}

/// The planted policy names the admin's team and channel: both searches find exactly them,
/// with `policy_id` filled in; a term that matches nothing is `[]`; an unknown policy is `[]`.
#[tokio::test]
async fn the_planted_policy_is_searchable_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let pool = fixture_pool()
        .await
        .expect("DATABASE_URL is set for the parity stack");
    purge_own_rows(&pool).await;
    let (team, channel) = own_team_and_channel(&client, &admin).await;
    let pool = plant_policy(&team, &channel).await;

    let teams = both_agree(
        &client,
        &admin,
        &teams_path(POLICY),
        br#"{"term":""}"#,
        "teams",
    )
    .await;
    let found: Vec<&str> = teams
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(found, vec![team.as_str()]);
    assert_eq!(teams[0]["policy_id"], POLICY);

    let channels = both_agree(
        &client,
        &admin,
        &channels_path(POLICY),
        br#"{"term":""}"#,
        "channels",
    )
    .await;
    let found: Vec<&str> = channels
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(found, vec![channel.as_str()]);
    assert_eq!(channels[0]["policy_id"], POLICY);
    assert!(
        channels[0]["team_display_name"].is_string(),
        "the channel carries its team data"
    );

    let none = both_agree(
        &client,
        &admin,
        &teams_path(POLICY),
        br#"{"term":"mmrsnosuchteam"}"#,
        "teams, no match",
    )
    .await;
    assert_eq!(none, serde_json::json!([]));
    let none = both_agree(
        &client,
        &admin,
        &channels_path(POLICY),
        br#"{"term":"mmrsnosuchchannel"}"#,
        "channels, no match",
    )
    .await;
    assert_eq!(none, serde_json::json!([]));

    // A null body: a zero TeamSearch on the teams route, the 400 on the channels one.
    let teams = both_agree(&client, &admin, &teams_path(POLICY), b"null", "teams, null").await;
    assert_eq!(teams.as_array().unwrap().len(), 1);
    let err = both_refuse(
        &client,
        &admin,
        &channels_path(POLICY),
        b"null",
        400,
        "channels, null",
    )
    .await;
    assert_eq!(err["id"], "api.context.invalid_body_param.app_error");

    let none = both_agree(
        &client,
        &admin,
        &teams_path("mmrsretentionsearch0000009"),
        br#"{"term":""}"#,
        "unknown policy",
    )
    .await;
    assert_eq!(none, serde_json::json!([]));
    let none = both_agree(
        &client,
        &admin,
        &channels_path("mmrsretentionsearch0000009"),
        br#"{"term":""}"#,
        "unknown policy, channels",
    )
    .await;
    assert_eq!(none, serde_json::json!([]));

    // Marshalled, not encoded: the `<&>` in the display names reached the wire escaped, or the
    // byte comparison above would not have passed.
    assert_eq!(teams[0]["display_name"], "mmrs retention search <&>");

    purge_own_rows(&pool).await;
}

/// A short policy id is `RequirePolicyId`'s 400 — set, never checked, and appended after the
/// 200 body by the handler wrapper. The two halves are compared separately: the list byte for
/// byte, the error up to its `request_id`.
#[tokio::test]
async fn a_short_policy_id_is_a_200_with_the_400_appended() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;

    for (path, body) in [
        (teams_path("abc"), &br#"{"term":""}"#[..]),
        (channels_path("abc"), &br#"{"term":""}"#[..]),
    ] {
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &admin, &path, body).await;
        assert_eq!(
            go_status,
            200,
            "{path}: Go: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(
            rs_status,
            200,
            "{path}: Rust: {}",
            String::from_utf8_lossy(&rs)
        );
        let split = |bytes: &[u8]| -> (Vec<u8>, Vec<u8>) {
            let at = bytes
                .windows(6)
                .position(|w| w == b"{\"id\":")
                .expect("an appended error");
            (bytes[..at].to_vec(), bytes[at..].to_vec())
        };
        let (go_list, go_err) = split(&go);
        let (rs_list, rs_err) = split(&rs);
        assert_eq!(go_list, b"[]", "{path}");
        assert_eq!(rs_list, go_list, "{path}");
        assert_error_bodies_match_except_known_gaps(&go_err, &rs_err, &path);
        let err: serde_json::Value = serde_json::from_slice(&go_err).unwrap();
        assert_eq!(
            err["id"], "api.context.invalid_url_param.app_error",
            "{path}"
        );
        assert_eq!(err["status_code"], 400, "{path}");
    }
}

/// The permission is checked before the body on the teams route and after it on the channels
/// route: a plain user with a bad body is told about the permission on one and the body on the
/// other. The admin's bad body is the 400 on both.
#[tokio::test]
async fn the_refusals_come_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "retentionsearch").await;

    let err = both_refuse(
        &client,
        &plain.token,
        &teams_path(POLICY),
        b"not json",
        403,
        "plain, teams",
    )
    .await;
    assert_eq!(err["id"], "api.context.permissions.app_error");
    let err = both_refuse(
        &client,
        &plain.token,
        &channels_path(POLICY),
        b"not json",
        400,
        "plain, channels",
    )
    .await;
    assert_eq!(err["id"], "api.context.invalid_body_param.app_error");
    let err = both_refuse(
        &client,
        &plain.token,
        &channels_path(POLICY),
        br#"{"term":""}"#,
        403,
        "plain, channels, good body",
    )
    .await;
    assert_eq!(err["id"], "api.context.permissions.app_error");

    let err = both_refuse(
        &client,
        &admin,
        &teams_path(POLICY),
        b"not json",
        400,
        "admin, teams",
    )
    .await;
    assert_eq!(err["id"], "api.context.invalid_body_param.app_error");
    let err = both_refuse(
        &client,
        &admin,
        &channels_path(POLICY),
        b"[]",
        400,
        "admin, channels, array",
    )
    .await;
    assert_eq!(err["id"], "api.context.invalid_body_param.app_error");

    // No token at all: the session first, on both.
    for base in [GO, RUST] {
        let response = client
            .post(format!("{base}{}", teams_path(POLICY)))
            .body("{}")
            .send()
            .await
            .expect("answers");
        assert_eq!(response.status(), 401, "{base}");
    }
}
