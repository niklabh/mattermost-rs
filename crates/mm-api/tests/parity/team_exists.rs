//! Cross-server parity for `GET /api/v4/teams/name/{team_name}/exists` — `teamExists`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity team_exists
//! ```
//!
//! # "Exists" means "you can see it"
//!
//! The route never 404s and never 403s. A name that matches nothing and a team the caller may not
//! see are the same answer, `{"exists":false}` with a 200 — which is the point: it is asked
//! before a join, and it must not tell a stranger which private teams are out there.
//! [`a_private_team_exists_for_an_admin_and_not_for_anybody_else`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fetch_both_raw, go_minted_token, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    /// The plain user is a member.
    joined: String,
    /// Nobody but the admin is in it, and it does not allow open invites.
    private: String,
    /// The same, but `AllowOpenInvite` is on.
    open: String,
    plain_token: String,
    /// Joined the private team and then left it: the membership row survives with a non-zero
    /// `DeleteAt`, which is the only thing separating `member.is_some()` from Go's check.
    left_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let joined = create_team(client, token, "exists").await;
            let private = create_team(client, token, "existspriv").await;
            let open = create_team(client, token, "existsopen").await;

            // `create_team` leaves `AllowOpenInvite` off, and that flag — not `Type` — is what
            // this route's second branch reads. Set on the fixture's own row.
            set_open_invite(&open, true).await;

            let plain = create_plain_user(client, token, &joined, "exists").await;

            let left = create_plain_user(client, token, &private, "existsleft").await;
            leave_team(client, token, &private, &left.id).await;

            Fixture {
                joined: name_of(client, token, &joined).await,
                private: name_of(client, token, &private).await,
                open: name_of(client, token, &open).await,
                plain_token: plain.token,
                left_token: left.token,
            }
        })
        .await
}

async fn name_of(client: &reqwest::Client, token: &str, team_id: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/teams/{team_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let team: serde_json::Value = response.json().await.expect("the team decodes");
    team["name"].as_str().expect("a name").to_owned()
}

/// `DELETE /teams/{team_id}/members/{user_id}` — a **soft** delete, which is the point.
async fn leave_team(client: &reqwest::Client, token: &str, team_id: &str, user_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}/members/{user_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "leaving {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn set_open_invite(team_id: &str, allow: bool) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    let _ = sqlx::query("UPDATE teams SET allowopeninvite = $2 WHERE id = $1")
        .bind(team_id)
        .bind(allow)
        .execute(&pool)
        .await;
}

fn path(name: &str) -> String {
    format!("/api/v4/teams/name/{name}/exists")
}

async fn exists_for(client: &reqwest::Client, token: &str, name: &str) -> bool {
    let p = path(name);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(client, token, &p).await;
    assert_eq!(go_status, 200, "{p}: this route does not refuse");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert!(
        !go.ends_with(b"\n"),
        "`w.Write(MapBoolToJSON(...))` appends no newline: {}",
        String::from_utf8_lossy(&go)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed.as_object().expect("an object").keys().len(),
        1,
        "one key, and it is `exists`: {parsed}"
    );
    parsed["exists"].as_bool().expect("a bool")
}

/// A team the caller is in exists, whatever the flags say.
#[tokio::test]
async fn a_team_you_are_in_exists() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    assert!(exists_for(&client, &f.plain_token, &f.joined).await);
    assert!(exists_for(&client, &token, &f.joined).await);
}

/// **The visibility rule.** A team with open invites off is visible only to a caller holding the
/// system-level `list_private_teams` — an admin — however public its *type* is.
#[tokio::test]
async fn a_private_team_exists_for_an_admin_and_not_for_anybody_else() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    assert!(
        exists_for(&client, &token, &f.private).await,
        "the admin holds list_private_teams"
    );
    assert!(
        !exists_for(&client, &f.plain_token, &f.private).await,
        "and a plain user does not, so the team does not exist for them"
    );
    // **A left membership is not a membership.** The row is still there — `DELETE` on a team
    // member is a soft delete — so `member != nil` is true and only `DeleteAt == 0` separates
    // this caller from a current member. Without this fixture user that check is dead code.
    assert!(
        !exists_for(&client, &f.left_token, &f.private).await,
        "a user who left the team stops being able to see that it exists"
    );
}

/// With `AllowOpenInvite` on, `list_public_teams` carries it — which every `system_user` has.
#[tokio::test]
async fn an_open_invite_team_exists_for_everyone() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    assert!(exists_for(&client, &token, &f.open).await);
    assert!(
        exists_for(&client, &f.plain_token, &f.open).await,
        "the flag is what moved, not the team's type or the caller's membership"
    );
}

/// A name that matches nothing is `false`, not a 404.
#[tokio::test]
async fn an_unknown_name_is_false_rather_than_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    assert!(!exists_for(&client, &token, "mmrs-parity-no-such-team").await);
}

/// `RequireTeamName` rejects the shapes `IsValidTeamName` refuses; the rest gorilla 404s.
#[tokio::test]
async fn an_invalid_name_is_a_400_and_a_bad_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    // Inside the mux charset (`[A-Za-z0-9_-]`), outside `IsValidTeamName` (`isValidAlphaNum`
    // plus a minimum length of **2**). The gap between the two is where the 400s live: an
    // underscore and a leading hyphen pass the router and fail the validator, and a single
    // character is simply too short.
    //
    // Two shapes that look invalid and are not. **Uppercase** is fine — the segment is
    // lowercased before the validator sees it, so `AB` is `ab`. And there is **no reserved-name
    // check**: `signup`, `login` and `admin` are all valid team names here, whatever
    // `CleanTeamName` next door might suggest.
    for bad in ["a", "-ab", "ab_cd"] {
        let p = path(bad);
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p} must be rejected by Go");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// No session is a 401 on both.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = path("mmrs-parity-anything");
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{p}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{p}");
    }
}

/// Every other method on this path is Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let p = path("mmrs-parity-anything");
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }
}

/// The segment is lowercased before anything reads it, so an uppercase name is the same request.
///
/// `params.TeamName = strings.ToLower(...)` (web/params.go:178) applies to **every** route with a
/// `{team_name}` segment. Both team routes in this port validated the raw segment instead and
/// answered 400 where Go answers 200 — measured, and fixed with this test.
#[tokio::test]
async fn an_uppercase_name_is_the_same_request() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let shouted = f.joined.to_uppercase();
    assert!(
        exists_for(&client, &token, &shouted).await,
        "the lowercased name names the fixture's team"
    );

    // And the sibling route under the same segment, which had the same bug.
    let p = format!("/api/v4/teams/name/{shouted}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 200, "{p}");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
}
