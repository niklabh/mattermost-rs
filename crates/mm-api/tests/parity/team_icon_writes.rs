//! Cross-server parity for `DELETE /api/v4/teams/{team_id}/image` — `removeTeamIcon`.
//!
//! ```sh
//! scripts/parity.sh --test parity team_icon_writes
//! ```
//!
//! Removing an icon does not delete the file: `RemoveTeamIcon` zeroes `LastTeamIconUpdate`
//! (and `UpdateAt` with it, in one `UPDATE`), publishes `update_team` with the **sanitised**
//! team as it was fetched — its `update_at` still the old one — and answers `{"status":"OK"}`.
//! The bytes stay on disk and `GET …/image` reads the file, not the timestamp, so the icon is
//! still served afterwards on both; a client learns it is gone from the team's
//! `last_team_icon_update`. The icon itself is set through Go's `POST` (multipart), which this
//! server does not serve.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, TINY_PNG, assert_error_bodies_match_except_known_gaps,
    client, create_plain_user, create_team, fixture_pool, go_minted_token, stack_enabled,
};

async fn set_team_icon(client: &reqwest::Client, token: &str, team_id: &str) {
    const BOUNDARY: &str = "mmrsparityteamiconboundary";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"i.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(TINY_PNG);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let response = client
        .post(format!("{GO}/api/v4/teams/{team_id}/image"))
        .header("Authorization", format!("Bearer {token}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "setting the icon failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn remove(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    team_id: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .delete(format!("{base}/api/v4/teams/{team_id}/image"))
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

/// `(lastteamiconupdate, updateat)` of the team row.
async fn team_row(team_id: &str) -> (i64, i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as("SELECT lastteamiconupdate, updateat FROM teams WHERE id = $1")
        .bind(team_id)
        .fetch_one(&pool)
        .await
        .expect("the team row")
}

async fn icon_status(client: &reqwest::Client, base: &str, token: &str, team_id: &str) -> u16 {
    client
        .get(format!("{base}/api/v4/teams/{team_id}/image"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"))
        .status()
        .as_u16()
}

/// Each server removes an icon it was given: the row's timestamp goes to zero, `UpdateAt`
/// moves with it, the file — and so the read — is untouched on both, and the `update_team`
/// frame carries the sanitised team with `last_team_icon_update: 0`.
#[tokio::test]
async fn removing_an_icon_zeroes_the_timestamp_and_publishes_the_team() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (base, tag) in [(GO, "ticog"), (RUST, "ticor")] {
        let team_id = create_team(&client, &token, tag).await;
        set_team_icon(&client, &token, &team_id).await;
        let (before_icon, before_update) = team_row(&team_id).await;
        assert!(before_icon > 0, "{base}: the icon was set");
        assert_eq!(
            icon_status(&client, base, &token, &team_id).await,
            200,
            "{base}: readable before"
        );

        let mut socket = SocketProbe::connect(base, &token).await;
        let (status, served, body) = remove(&client, base, &token, &team_id).await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            body, br#"{"status":"OK"}"#,
            "{base}: ReturnStatusOK, no newline"
        );

        let (after_icon, after_update) = team_row(&team_id).await;
        assert_eq!(after_icon, 0, "{base}");
        assert_eq!(
            after_update, 0,
            "{base}: UpdateLastTeamIconUpdate writes the same value to UpdateAt"
        );
        assert!(before_update > 0);
        for reader in [GO, RUST] {
            assert_eq!(
                icon_status(&client, reader, &token, &team_id).await,
                200,
                "{base} removed, {reader} reads: the file is not deleted and the read does not consult the timestamp"
            );
        }

        let updated = |frames: &[serde_json::Value]| {
            frames.iter().any(|f| {
                f["event"] == "update_team"
                    && f["data"]["team"]
                        .as_str()
                        .is_some_and(|t| t.contains(&team_id))
            })
        };
        assert!(
            socket
                .collect_until(Duration::from_millis(2500), updated)
                .await,
            "{base}: no update_team frame: {:?}",
            socket.raw
        );
        let frame = socket
            .events_named("update_team")
            .into_iter()
            .find(|f| {
                f["data"]["team"]
                    .as_str()
                    .is_some_and(|t| t.contains(&team_id))
            })
            .expect("the frame");
        let team: serde_json::Value =
            serde_json::from_str(frame["data"]["team"].as_str().expect("a string"))
                .expect("a team");
        // `last_team_icon_update` is `omitempty`: a zero is an absent key, on both.
        assert!(
            team.get("last_team_icon_update").is_none(),
            "{base}: a zeroed timestamp is omitted: {team}"
        );
        assert_eq!(
            team["update_at"], before_update,
            "{base}: the event carries the team as fetched, not the row as rewritten"
        );
        assert_eq!(team["email"], "", "{base}: sanitised");
        assert_eq!(team["invite_id"], "", "{base}: sanitised");
        assert_eq!(frame["broadcast"]["team_id"], team_id.as_str(), "{base}");
    }
}

/// Without `manage_team` the request is the 403, before any lookup; an unknown team is the
/// route's own 400 (`api.team.remove_team_icon.get_team.app_error`) — not a 404 — because
/// `RemoveTeamIcon` wraps `GetTeam`'s error into it.
#[tokio::test]
async fn a_plain_member_is_refused_and_an_unknown_team_is_a_400() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "ticop").await;
    let member = create_plain_user(&client, &token, &team_id, "ticop").await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = remove(&client, base, &member.token, &team_id).await;
        assert_eq!(status, 403, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "no manage_team");

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) =
            remove(&client, base, &token, "zzzzzzzzzzzzzzzzzzzzzzzzzz").await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(
            parsed["id"], "api.team.remove_team_icon.get_team.app_error",
            "{base}"
        );
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "unknown team");
}
