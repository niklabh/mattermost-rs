//! Cross-server parity for `POST /api/v4/teams/members/invite` — `addUserToTeamFromInvite`.
//!
//! ```sh
//! scripts/parity.sh --test parity team_invite_join
//! ```
//!
//! A user joins a team by its **invite id** (`?invite_id=`): served, a 201 with the new member.
//! The invitation-token way in (`?token=`) is forwarded whole, and it wins when both are sent.
//! Neither is the 400 whose `where` is `addTeamMember`. Behind the served path: a group-constrained
//! team is a 403, an unknown invite id a 404, and `JoinUserToTeam`'s own refusals — a domain
//! outside the team's `allowed_domains` — come through unchanged.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fixture_pool, go_minted_token, stack_enabled,
};

struct Fixture {
    /// The team being joined, and its invite id.
    team_id: String,
    invite_id: String,
    /// A home team for the joiners, so they exist without being members of the target.
    home_team: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn team_invite_id(client: &reqwest::Client, token: &str, team_id: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/teams/{team_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let team: serde_json::Value = response.json().await.expect("a team");
    team["invite_id"].as_str().expect("an invite id").to_owned()
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team_id = create_team(client, token, "tinv").await;
            let home_team = create_team(client, token, "tinvh").await;
            Fixture {
                invite_id: team_invite_id(client, token, &team_id).await,
                team_id,
                home_team,
            }
        })
        .await
}

async fn join(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    query: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/teams/members/invite{query}"))
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

fn normalised(member: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(member).expect("a member");
    let obj = value.as_object_mut().expect("an object");
    for key in ["user_id", "create_at"] {
        obj.insert(key.to_owned(), serde_json::json!(0));
    }
    value
}

/// `(deleteat, roles)` of the membership row.
async fn member_row(team_id: &str, user_id: &str) -> Option<(i64, String)> {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as("SELECT deleteat, roles FROM teammembers WHERE teamid = $1 AND userid = $2")
        .bind(team_id)
        .bind(user_id)
        .fetch_optional(&pool)
        .await
        .expect("the member query")
}

/// A fresh user joins by invite id on each server: a 201 with the member, `json.NewEncoder`
/// newline included, and a live `TeamMembers` row. Joining again answers the existing member.
#[tokio::test]
async fn a_user_joins_by_invite_id_and_joining_again_is_idempotent() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for (base, tag) in [(GO, "tinvg"), (RUST, "tinvr")] {
        let joiner = create_plain_user(&client, &token, &f.home_team, tag).await;
        assert!(member_row(&f.team_id, &joiner.id).await.is_none());

        let (status, served, body) = join(
            &client,
            base,
            &joiner.token,
            &format!("?invite_id={}", f.invite_id),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert!(
            body.ends_with(b"\n"),
            "{base}: json.NewEncoder writes a newline"
        );
        let member = normalised(&body);
        assert_eq!(member["team_id"], f.team_id.as_str(), "{base}");
        assert_eq!(member["delete_at"], 0, "{base}");
        assert_eq!(member["scheme_user"], true, "{base}");
        assert_eq!(member["scheme_guest"], false, "{base}");
        assert_eq!(
            member_row(&f.team_id, &joiner.id).await,
            Some((0, "".to_owned())),
            "{base}: a live row with scheme roles, not explicit ones"
        );
        bodies.push(member);

        // Again: the existing membership comes back, still a 201.
        let (status, served, again) = join(
            &client,
            base,
            &joiner.token,
            &format!("?invite_id={}", f.invite_id),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&again));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            normalised(&again),
            bodies[bodies.len() - 1],
            "{base}: the same member"
        );
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// The three refusals before the join: no parameter (400, `where` `addTeamMember`), an unknown
/// invite id (404), and a group-constrained team (403) — the last planted, since the flag is a
/// licensed edit.
#[tokio::test]
async fn the_refusals_before_the_join_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let joiner = create_plain_user(&client, &token, &f.home_team, "tinvx").await;

    for (query, status, id) in [
        (
            "",
            400,
            "api.team.add_user_to_team.missing_parameter.app_error",
        ),
        (
            "?invite_id=nosuchinviteidatallxxxxxx",
            404,
            "app.team.get_by_invite_id.finding.app_error",
        ),
    ] {
        let mut bodies = Vec::new();
        for base in [GO, RUST] {
            let (got, served, body) = join(&client, base, &joiner.token, query).await;
            assert_eq!(
                got,
                status,
                "{base} {query}: {}",
                String::from_utf8_lossy(&body)
            );
            assert_eq!(served, base == RUST, "{base} {query}");
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            assert_eq!(parsed["id"], id, "{base} {query}");
            bodies.push(body);
        }
        assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], query);
    }

    // Group-constrained: planted on a team of its own.
    let constrained = create_team(&client, &token, "tinvc").await;
    let invite = team_invite_id(&client, &token, &constrained).await;
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query("UPDATE teams SET groupconstrained = true WHERE id = $1")
        .bind(&constrained)
        .execute(&pool)
        .await
        .expect("planted");
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = join(
            &client,
            base,
            &joiner.token,
            &format!("?invite_id={invite}"),
        )
        .await;
        assert_eq!(status, 403, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(
            parsed["id"], "app.team.invite_id.group_constrained.error",
            "{base}"
        );
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "group constrained");
    assert!(member_row(&constrained, &joiner.id).await.is_none());
}

/// `JoinUserToTeam`'s own refusal comes through: a team restricted to a domain the joiner's
/// email is not in is the 400 `allowed_domains`, on both.
#[tokio::test]
async fn a_domain_restriction_refuses_the_join_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let restricted = create_team(&client, &token, "tinvd").await;
    let response = client
        .put(format!("{GO}/api/v4/teams/{restricted}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "allowed_domains": "example.com" }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "restricting the domain");
    let invite = team_invite_id(&client, &token, &restricted).await;
    let joiner = create_plain_user(&client, &token, &f.home_team, "tinvdo").await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = join(
            &client,
            base,
            &joiner.token,
            &format!("?invite_id={invite}"),
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(
            parsed["id"], "api.team.join_user_to_team.allowed_domains.app_error",
            "{base}"
        );
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "allowed_domains");
}

/// `?token=` is forwarded, and it wins over an `invite_id` sent beside it: a bogus token is
/// Go's own answer, served-by `go`, with no membership written.
#[tokio::test]
async fn a_token_is_forwarded_and_wins_over_an_invite_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let joiner = create_plain_user(&client, &token, &f.home_team, "tinvt").await;

    for query in [
        "?token=nosuchtokenxxxxxxxxxxxxxxxxxx".to_owned(),
        format!(
            "?token=nosuchtokenxxxxxxxxxxxxxxxxxx&invite_id={}",
            f.invite_id
        ),
    ] {
        let (go_status, _, go) = join(&client, GO, &joiner.token, &query).await;
        let (rs_status, served, rs) = join(&client, RUST, &joiner.token, &query).await;
        assert_eq!(rs_status, go_status, "{query}");
        assert!(!served, "{query}: the token path is Go's");
        assert!(
            go_status >= 400,
            "{query}: a bogus token fails: {}",
            String::from_utf8_lossy(&go)
        );
        // Both answers are Go's; only the request id differs.
        let strip = |body: &[u8]| -> serde_json::Value {
            let mut value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            if let Some(obj) = value.as_object_mut() {
                obj.remove("request_id");
            }
            value
        };
        assert_eq!(strip(&go), strip(&rs), "{query}");
        assert!(
            member_row(&f.team_id, &joiner.id).await.is_none(),
            "{query}: the invite id beside the token was not used"
        );
    }
}
