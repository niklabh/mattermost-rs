//! Cross-server parity for `GET /api/v4/teams/invite/{invite_id}` — `getInviteInfo`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity invite_info
//! ```
//!
//! # The only unauthenticated route that returns data
//!
//! `APIHandler`, not `APISessionRequired` (api4/team.go:75), because the join-by-link page shows a
//! team's name to someone who has not signed in. The two other sessionless routes this server
//! answers — SAML metadata and the session-attributes manifest — are both refusals. So this is the
//! one place where forgetting that distinction would turn a working sign-up flow into a 401, and
//! every request below is sent **with no token**.
//!
//! # Four fields, and the twenty it does not send
//!
//! The body is an anonymous struct, not a `Team`: `display_name`, `description`, `name`, `id`.
//! `email`, `allowed_domains`, the invite id itself and everything else stay on the server — not
//! by sanitisation, which this route never calls, but because they were never in the struct.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_team,
    go_minted_token, stack_enabled,
};

const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// Fetch with **no** `Authorization` header, from both servers.
async fn anonymous(
    client: &reqwest::Client,
    path: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>, Option<String>)) {
    let go = client
        .get(format!("{GO}{path}"))
        .send()
        .await
        .expect("Go answers");
    let go = (
        go.status().as_u16(),
        go.bytes().await.expect("reads").to_vec(),
    );

    let rs = client
        .get(format!("{RUST}{path}"))
        .send()
        .await
        .expect("we answer");
    let served_by = rs
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let rs = (
        rs.status().as_u16(),
        rs.bytes().await.expect("reads").to_vec(),
        served_by,
    );
    (go, rs)
}

/// The invite id of a team created for this suite, read straight from the row — the API never
/// returns it to a caller who is not a member.
async fn invite_id_of(team_id: &str) -> Option<String> {
    let pool = common::fixture_pool().await?;
    sqlx::query_scalar::<_, String>("SELECT inviteid FROM teams WHERE id = $1")
        .bind(team_id)
        .fetch_one(&pool)
        .await
        .ok()
}

/// Set a team's type directly. `PUT /teams/{id}/privacy` would do it over the API, but it needs a
/// session and this suite's point is that the route under test does not.
async fn set_team_type(team_id: &str, team_type: &str) -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    sqlx::query("UPDATE teams SET type = $2::team_type WHERE id = $1")
        .bind(team_id)
        .bind(team_type)
        .execute(&pool)
        .await
        .is_ok()
}

/// The happy path, byte for byte, **with no token** — and the four keys, in Go's order.
#[tokio::test]
async fn an_open_teams_invite_is_readable_without_a_session() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "invite").await;

    let Some(invite) = invite_id_of(&team).await else {
        return; // no DATABASE_URL
    };

    let path = format!("/api/v4/teams/invite/{invite}");
    let ((go_status, go), (rs_status, rs, served_by)) = anonymous(&client, &path).await;
    assert_eq!(go_status, 200, "{path}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(served_by.as_deref(), Some("rust"), "{path}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    assert!(rs.ends_with(b"\n"), "the encoder's newline");

    // Four keys, and the order is the struct's — a `serde_json::Value` comparison would not see
    // that, so the raw bytes are what the assertion above rests on. This states the shape.
    let info: serde_json::Value = serde_json::from_slice(&rs).expect("json");
    let object = info.as_object().expect("an object");
    assert_eq!(object.len(), 4, "{info}");
    for key in ["display_name", "description", "name", "id"] {
        assert!(object.contains_key(key), "{key}: {info}");
    }
    assert_eq!(info["id"], team.as_str());
    assert_eq!(info["name"], "mmrs-parity-invite");

    // **The invite id is not echoed back**, and neither is anything else `Team` carries.
    for absent in ["invite_id", "email", "allowed_domains", "type", "create_at"] {
        assert!(
            !object.contains_key(absent),
            "{absent} must not leak: {info}"
        );
    }
    assert!(
        !String::from_utf8_lossy(&rs).contains(&invite),
        "the invite id itself is not in the body"
    );
}

/// **A closed team is a 403, and an unknown invite is a 404** — so the pair tells a caller apart
/// "no such invite" from "that invite is for a team you may not see this way".
#[tokio::test]
async fn a_closed_team_is_a_403_and_an_unknown_invite_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "invclosed").await;

    let Some(invite) = invite_id_of(&team).await else {
        return;
    };

    // Unknown invite: 404, from the store.
    let path = format!("/api/v4/teams/invite/{NOWHERE}");
    let ((go_status, go), (rs_status, rs, served_by)) = anonymous(&client, &path).await;
    assert_eq!(go_status, 404, "{path}");
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(served_by.as_deref(), Some("rust"), "{path}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(body["id"], "app.team.get_by_invite_id.finding.app_error");

    // The same invite, once the team stops being open: 403 with a different id.
    if !set_team_type(&team, "I").await {
        return;
    }
    let path = format!("/api/v4/teams/invite/{invite}");
    let ((go_status, go), (rs_status, rs, served_by)) = anonymous(&client, &path).await;
    assert_eq!(
        go_status,
        403,
        "{path}: a closed team is refused, not missing: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(served_by.as_deref(), Some("rust"), "{path}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(body["id"], "api.team.get_invite_info.not_open_team");
    // The invite id goes into `detailed_error`, which `WipeDetailed` blanks before it ships.
    assert_eq!(body["detailed_error"], "", "the id must not reach a client");

    // Restored so the suite's other users can still join it.
    set_team_type(&team, "O").await;
}

/// An **empty** `InviteId` must not match a team. `Teams.InviteId` is not unique and rows with an
/// empty one exist (Go has a `GetByEmptyInviteID` for exactly that), so a store without the guard
/// would hand an arbitrary team to an anonymous caller.
#[tokio::test]
async fn an_empty_invite_id_matches_nothing() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "invempty").await;

    // Plant the condition the guard exists for: a team whose invite id is the empty string.
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    sqlx::query("UPDATE teams SET inviteid = '' WHERE id = $1")
        .bind(&team)
        .execute(&pool)
        .await
        .expect("empties the invite id");

    // The router will not route an empty segment — both servers 404 on the *path*, and ours is
    // forwarded because gorilla never matched it either.
    let path = "/api/v4/teams/invite/";
    let ((go_status, go), (rs_status, rs, served_by)) = anonymous(&client, path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "{path}: an empty segment is not a route on either server"
    );
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );

    // Restore, so `purge_api_fixtures` and the other suites see a normal team.
    sqlx::query("UPDATE teams SET inviteid = $2 WHERE id = $1")
        .bind(&team)
        .bind(&team)
        .execute(&pool)
        .await
        .expect("restores an invite id");
}

/// A token changes nothing — the route ignores the session entirely, so an authenticated caller
/// and an anonymous one get the same bytes.
#[tokio::test]
async fn a_session_makes_no_difference() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "invsess").await;

    let Some(invite) = invite_id_of(&team).await else {
        return;
    };
    let path = format!("/api/v4/teams/invite/{invite}");

    let (_, (_, anonymous_body, _)) = anonymous(&client, &path).await;
    let with_token = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("we answer")
        .bytes()
        .await
        .expect("reads")
        .to_vec();

    assert_eq!(
        String::from_utf8_lossy(&anonymous_body),
        String::from_utf8_lossy(&with_token),
        "the session is not consulted"
    );
}
