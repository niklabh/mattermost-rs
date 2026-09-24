//! Cross-server parity for `POST /api/v4/channels/direct` under
//! `TeamSettings.RestrictDirectMessage = "team"` ([D-239]).
//!
//! ```sh
//! scripts/parity.sh --test parity dm_team_restriction
//! ```
//!
//! The setting is patched through this server (a forwarded write into Go's `Configurations`, which
//! both processes read) and restored, holding [`common::CONFIG_DOCUMENT`] exclusively throughout.
//!
//! A DM is found before it is created, so a pair asked twice answers the first answer again
//! whatever the setting says. Every case therefore has a pair of users of its own **per server**.

use futures_util::FutureExt;

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, logged_in_user_id, stack_enabled,
};

async fn set_restriction(http: &reqwest::Client, admin: &str, value: &str) {
    let response = http
        .put(format!("{RUST}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "TeamSettings": { "RestrictDirectMessage": value } }))
        .send()
        .await
        .expect("mm-api answers");
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status, 200, "RestrictDirectMessage={value}: {body}");
}

/// `(status, served here, body)`.
async fn send(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> (u16, bool, String) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, served, response.text().await.unwrap_or_default())
}

/// One case's answer: status, whether we served it, and the body with the ids of this side
/// replaced — a created DM's name and id differ per side, an error's do not.
struct Answer {
    status: u16,
    served: bool,
    body: String,
}

async fn create_dm(http: &reqwest::Client, base: &str, token: &str, a: &str, b: &str) -> Answer {
    let (status, served, body) = send(
        http,
        base,
        reqwest::Method::POST,
        "/api/v4/channels/direct",
        token,
        Some(&serde_json::json!([a, b])),
    )
    .await;
    Answer {
        status,
        served,
        body,
    }
}

/// Every case, built and asked once per server.
///
/// | case | pair | expected (Go) |
/// |---|---|---|
/// | `shared` | two members of one live team | 201 |
/// | `apart` | no team in common | 403 |
/// | `deleted` | common only in a soft-deleted team | 403 |
/// | `departed` | common only in a team one of them left | 403 |
/// | `self` | the caller with themself | 201 — the two-user query's self-join |
/// | `admin` | a system admin and a user it shares no team with | 201 — `manage_system` |
/// | `ownbot` | the caller and a teamless bot the caller owns | 201 — owner exemption |
/// | `otherbot` | the caller and a teamless bot somebody else owns | 403, **forwarded** while Go hosts plugins |
#[tokio::test]
async fn the_team_restriction_answers_like_go() {
    if !stack_enabled() {
        return;
    }
    let _document = common::CONFIG_DOCUMENT.write().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let admin_id = logged_in_user_id().to_owned();

    set_restriction(&http, &admin, "team").await;
    let result = std::panic::AssertUnwindSafe(run_cases(&http, &admin, &admin_id))
        .catch_unwind()
        .await;
    set_restriction(&http, &admin, "any").await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_cases(http: &reqwest::Client, admin: &str, admin_id: &str) {
    let mut answers: Vec<Vec<(&str, Answer)>> = Vec::new();
    let mut cleanup_users = Vec::new();
    let mut cleanup_bots = Vec::new();

    for side in ["go", "rs"] {
        let base = if side == "go" { GO } else { RUST };
        let tag = |case: &str| format!("dmr{case}{side}");
        let mut side_answers = Vec::new();

        // The caller, on a team of its own that everyone in `shared` also joins.
        let home = create_team(http, admin, &tag("home")).await;
        let caller = create_plain_user(http, admin, &home, &tag("caller")).await;
        cleanup_users.push(caller.id.clone());

        // shared
        let mate = create_plain_user(http, admin, &home, &tag("mate")).await;
        cleanup_users.push(mate.id.clone());
        side_answers.push((
            "shared",
            create_dm(http, base, &caller.token, &caller.id, &mate.id).await,
        ));

        // apart: a user on another team only.
        let away = create_team(http, admin, &tag("away")).await;
        let stranger = create_plain_user(http, admin, &away, &tag("stranger")).await;
        cleanup_users.push(stranger.id.clone());
        side_answers.push((
            "apart",
            create_dm(http, base, &caller.token, &caller.id, &stranger.id).await,
        ));

        // deleted: both on a team that is then archived.
        let doomed = create_team(http, admin, &tag("doomed")).await;
        let former = create_plain_user(http, admin, &doomed, &tag("former")).await;
        cleanup_users.push(former.id.clone());
        join_team(http, admin, &doomed, &caller.id).await;
        let (status, _, body) = send(
            http,
            GO,
            reqwest::Method::DELETE,
            &format!("/api/v4/teams/{doomed}"),
            admin,
            None,
        )
        .await;
        assert_eq!(status, 200, "archiving the team: {body}");
        side_answers.push((
            "deleted",
            create_dm(http, base, &caller.token, &caller.id, &former.id).await,
        ));

        // departed: both on a team, then the other leaves it.
        let left = create_team(http, admin, &tag("left")).await;
        let leaver = create_plain_user(http, admin, &left, &tag("leaver")).await;
        cleanup_users.push(leaver.id.clone());
        join_team(http, admin, &left, &caller.id).await;
        let (status, _, body) = send(
            http,
            GO,
            reqwest::Method::DELETE,
            &format!("/api/v4/teams/{left}/members/{}", leaver.id),
            admin,
            None,
        )
        .await;
        assert_eq!(status, 200, "leaving the team: {body}");
        side_answers.push((
            "departed",
            create_dm(http, base, &caller.token, &caller.id, &leaver.id).await,
        ));

        // self
        side_answers.push((
            "self",
            create_dm(http, base, &caller.token, &caller.id, &caller.id).await,
        ));

        // admin: the admin leaves the team it made, so it shares none with the user there.
        let solo = create_team(http, admin, &tag("solo")).await;
        let loner = create_plain_user(http, admin, &solo, &tag("loner")).await;
        cleanup_users.push(loner.id.clone());
        let (status, _, body) = send(
            http,
            GO,
            reqwest::Method::DELETE,
            &format!("/api/v4/teams/{solo}/members/{admin_id}"),
            admin,
            None,
        )
        .await;
        assert_eq!(status, 200, "the admin leaves: {body}");
        side_answers.push((
            "admin",
            create_dm(http, base, admin, admin_id, &loner.id).await,
        ));

        // ownbot / otherbot: bots on no team at all.
        let own = common::plant_bot(&tag("own"), &caller.id, 0)
            .await
            .expect("a bot");
        let other = common::plant_bot(&tag("other"), admin_id, 0)
            .await
            .expect("a bot");
        cleanup_bots.push(own.clone());
        cleanup_bots.push(other.clone());
        side_answers.push((
            "ownbot",
            create_dm(http, base, &caller.token, &caller.id, &own).await,
        ));
        side_answers.push((
            "otherbot",
            create_dm(http, base, &caller.token, &caller.id, &other).await,
        ));

        answers.push(side_answers);
    }

    let rust = answers.pop().expect("two sides");
    let go = answers.pop().expect("two sides");
    let expected = [
        ("shared", 201),
        ("apart", 403),
        ("deleted", 403),
        ("departed", 403),
        ("self", 201),
        ("admin", 201),
        ("ownbot", 201),
        ("otherbot", 403),
    ];
    for (((case, go), (_, rust)), (_, want)) in go.iter().zip(rust.iter()).zip(expected) {
        assert_eq!(go.status, want, "{case}: Go answered {}", go.body);
        assert_eq!(rust.status, go.status, "{case}: we answered {}", rust.body);
        // Forwarded exactly when a bot's exemption is Go's to decide.
        assert_eq!(
            rust.served,
            *case != "otherbot",
            "{case}: served here? {}",
            rust.body
        );
        if go.status == 403 {
            let id = serde_json::from_str::<serde_json::Value>(&go.body).expect("an error")["id"]
                .clone();
            assert_eq!(
                id, "api.channel.create_channel.direct_channel.team_restricted_error",
                "{case}"
            );
            assert_error_bodies_match_except_known_gaps(
                go.body.as_bytes(),
                rust.body.as_bytes(),
                case,
            );
        } else {
            let channel: serde_json::Value = serde_json::from_str(&rust.body).expect("a channel");
            assert_eq!(channel["type"], "D", "{case}: {}", rust.body);
        }
    }

    for user in cleanup_users {
        common::delete_plain_user(http, admin, &user).await;
    }
    for bot in cleanup_bots {
        common::unplant_bot(&bot).await;
    }
}

async fn join_team(http: &reqwest::Client, admin: &str, team_id: &str, user_id: &str) {
    let (status, _, body) = send(
        http,
        GO,
        reqwest::Method::POST,
        &format!("/api/v4/teams/{team_id}/members"),
        admin,
        Some(&serde_json::json!({ "team_id": team_id, "user_id": user_id })),
    )
    .await;
    assert!(status < 300, "joining the team: {body}");
}
