//! Cross-server parity for `Session.TeamMembers` — the field [D-077] left empty.
//!
//! Go's `SqlSessionStore.Get` hydrates the session's team members with a second query carrying
//! the scheme-roles join, and a member's **effective** roles are computed rather than read. This
//! test asserts our computation against the roles the running Go server reports for the same
//! rows, because getting the scheme-roles logic wrong is a silent permission difference — a
//! member who should be a team admin quietly is not — and no unit test can catch that on its own.
//!
//! ```sh
//! docker compose up -d
//! MM_PARITY_STACK=1 DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost \
//!   cargo test -p mm-api --test parity_session_team_members
//! ```

use crate::common;

use common::{GO, client, go_minted_token, stack_enabled};
use mm_store::{SessionStore, SqlStore};

/// What the Go server says this user's team memberships are.
async fn go_team_members(client: &reqwest::Client, token: &str) -> Vec<serde_json::Value> {
    let response = client
        .get(format!("{GO}/api/v4/users/me/teams/members"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Go server is reachable");
    assert_eq!(response.status(), 200);
    response.json().await.expect("team members decode")
}

#[tokio::test]
async fn session_team_members_match_the_go_servers_computed_roles() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();

    let token = go_minted_token(&client).await;

    // **A comparison over the intersection, which no amount of churn can break.**
    //
    // The original bracketed the store read between two Go reads and accepted a *count* matching
    // either end. That holds while at most one team is created during the read, and stopped
    // holding once a suite created eight in a run — creating a team makes the creator a member,
    // and every suite in this binary uses this same admin token.
    //
    // A settle loop does not fix it either: the churn is continuous, so there may be no quiet
    // moment. Nor is the direction monotonic — suites remove members as well as add them, so the
    // later read can be *smaller*. What survives all of it is the **intersection**: a team both
    // reads saw must agree in every computed field, which is the whole claim. A team only one of
    // them saw is churn, and the overlap is required to be near-total so churn cannot hollow the
    // test out.
    let go_members = go_team_members(&client, &token).await;

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let store = SqlStore::connect(&database_url, 2)
        .await
        .expect("the shared Postgres is reachable");
    let session = store
        .session()
        .get(&token)
        .await
        .expect("the session Go just minted is readable here");

    let ours = session
        .team_members
        .expect("D-077: team_members must be hydrated, not left null");

    // Neither direction is monotonic — suites create teams *and* remove members while this runs —
    // so the comparison is over the **intersection**. Every team both reads saw must agree in
    // every computed field, which is the whole claim; a team only one of them saw is churn.
    let shared: Vec<&serde_json::Value> = go_members
        .iter()
        .filter(|m| {
            let team_id = m["team_id"].as_str().unwrap_or_default();
            ours.iter().any(|mine| mine.team_id == team_id)
        })
        .collect();
    assert!(
        !shared.is_empty() && shared.len() * 10 >= go_members.len() * 9,
        "the two reads should overlap almost entirely; churn cannot explain {} of {}",
        go_members.len() - shared.len(),
        go_members.len()
    );

    // Compare per team id rather than by position — neither query has an ORDER BY, so the row
    // order is not part of the contract and asserting it would make this test flaky rather than
    // strict.
    for go_member in &go_members {
        let team_id = go_member["team_id"].as_str().expect("team_id is a string");
        let Some(mine) = ours.iter().find(|m| m.team_id == team_id) else {
            // Created or removed between the two reads — see the note above.
            continue;
        };

        // `roles` is the computed field and the reason this test exists.
        assert_eq!(
            mine.roles,
            go_member["roles"].as_str().unwrap_or_default(),
            "computed roles differ for team {team_id}"
        );
        assert_eq!(
            mine.explicit_roles,
            go_member["explicit_roles"].as_str().unwrap_or_default(),
            "explicit_roles differ for team {team_id}"
        );
        assert_eq!(mine.scheme_guest, go_member["scheme_guest"] == true);
        assert_eq!(mine.scheme_user, go_member["scheme_user"] == true);
        assert_eq!(mine.scheme_admin, go_member["scheme_admin"] == true);
        assert_eq!(
            mine.delete_at,
            go_member["delete_at"].as_i64().unwrap_or_default()
        );
    }

    // Go filters deleted members out of the session's list (session_store.go:118), so nothing
    // here may carry a delete_at.
    assert!(
        ours.iter().all(|m| m.delete_at == 0),
        "the session's member list must exclude deleted memberships"
    );
}

/// The membership list has to be non-empty for the test above to be meaningful — an empty list
/// would compare equal to an empty list and prove nothing. This makes that explicit rather than
/// letting the real assertion pass vacuously.
#[tokio::test]
async fn the_fixture_user_actually_belongs_to_a_team() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let members = go_team_members(&client, &token).await;
    assert!(
        !members.is_empty(),
        "the fixture user belongs to no team, so the parity assertions would be vacuous — \
         create one: POST /api/v4/teams then add the user"
    );
}
