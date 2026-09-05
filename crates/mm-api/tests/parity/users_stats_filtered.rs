//! Cross-server parity for `GET /api/v4/users/stats/filtered` — `getFilteredUsersStats`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity users_stats_filtered
//! ```
//!
//! # Every assertion is relative
//!
//! The count is over the **whole users table**, which the other suites add to and delete from
//! throughout the run, so no absolute number means anything here. Each test asks two questions in
//! one bracket and compares them to each other — "does `include_deleted` raise the count", not
//! "is the count 69".

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel_typed,
    create_plain_user, create_team, delete_plain_user, fetch_both_raw, go_minted_token,
    purge_api_fixtures, stack_enabled,
};

const PATH: &str = "/api/v4/users/stats/filtered";

struct Fixture {
    team_id: String,
    channel_id: String,
    /// Deactivated, so it is counted only with `include_deleted`.
    deactivated_ok: bool,
    /// Given a non-empty `RemoteId`, so it is counted only with `include_remote_users`. No API
    /// creates one, and without it that filter is dead: this installation has no remote users.
    remote_ok: bool,
    plain_token: String,
    /// A team everybody has **left**. `TeamMembers` rows survive a leave with a non-zero
    /// `DeleteAt`, so its count is 0 only because the join carries `tm.DeleteAt = 0`.
    deserted_team: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "filtstats").await;
            let channel_id = create_channel_typed(client, token, &team_id, "filtstats", "O").await;

            let plain = create_plain_user(client, token, &team_id, "filtstats").await;
            common::add_user_to_channel(client, token, &channel_id, &plain.id).await;

            let doomed = create_plain_user(client, token, &team_id, "filtstatsdead").await;
            delete_plain_user(client, token, &doomed.id).await;

            let remote = create_plain_user(client, token, &team_id, "filtstatsremote").await;
            let remote_ok = plant_remote_id(&remote.id).await;

            // Every membership here is soft-deleted, which is the only state that tells
            // `tm.DeleteAt = 0` apart from no predicate at all.
            let deserted_team = create_team(client, token, "filtstatsgone").await;
            let leaver = create_plain_user(client, token, &deserted_team, "filtstatsleft").await;
            leave_team(client, token, &deserted_team, &leaver.id).await;
            leave_team(client, token, &deserted_team, common::logged_in_user_id()).await;

            Fixture {
                team_id,
                channel_id,
                deactivated_ok: true,
                remote_ok,
                plain_token: plain.token,
                deserted_team,
            }
        })
        .await
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

/// Write a `RemoteId` no API can set.
async fn plant_remote_id(user_id: &str) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return false;
    };
    sqlx::query("UPDATE users SET remoteid = 'mmrsparityremote' WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .is_ok()
}

/// Both servers' answer to one query, asserted byte-identical, as a number.
async fn count(client: &reqwest::Client, token: &str, query: &str) -> i64 {
    let path = if query.is_empty() {
        PATH.to_owned()
    } else {
        format!("{PATH}?{query}")
    };
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(client, token, &path).await;
    assert_eq!(go_status, 200, "{path}");
    assert_eq!(rs_status, go_status, "{path}: statuses must match");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "`json.NewEncoder(w).Encode` appends the newline `/users/stats` does not"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed.as_object().expect("an object").keys().len(),
        1,
        "one field: {parsed}"
    );
    parsed["total_users_count"].as_i64().expect("a number")
}

/// The three include flags each raise the count, and an unparseable value is `false`.
#[tokio::test]
async fn each_include_flag_raises_the_count() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let base = count(&client, &token, "").await;

    assert!(
        f.deactivated_ok && count(&client, &token, "include_deleted=true").await > base,
        "`include_deleted` drops the `DeleteAt = 0` predicate"
    );
    assert!(
        count(&client, &token, "include_bots=true").await > base,
        "`include_bots` drops the `Bots.UserId IS NULL` anti-join"
    );
    if f.remote_ok {
        assert!(
            count(&client, &token, "include_remote_users=true").await > base,
            "`include_remote_users` drops the `RemoteId = ''` predicate — dead until a row \
             carries a remote id, which no API can set"
        );
    }

    // `strconv.ParseBool`'s error is discarded, so this is `false`, not a 400 and not `true`.
    assert_eq!(
        count(&client, &token, "include_deleted=yes").await,
        base,
        "an unparseable boolean is false"
    );
}

/// `in_team` and `in_channel` each narrow the count, and the team wins when both are given.
#[tokio::test]
async fn the_team_filter_wins_over_the_channel_filter() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let team_only = count(&client, &token, &format!("in_team={}", f.team_id)).await;
    let channel_only = count(&client, &token, &format!("in_channel={}", f.channel_id)).await;
    let both = count(
        &client,
        &token,
        &format!("in_team={}&in_channel={}", f.team_id, f.channel_id),
    )
    .await;

    assert!(team_only > 0, "the fixture's users are in the team");
    assert!(channel_only > 0, "and two of them are in the channel");
    assert_eq!(
        both, team_only,
        "Go's `else if` filters on the team alone, not the intersection"
    );

    // **The membership's own `DeleteAt`.** Every row in this team is soft-deleted, and the count
    // is 0 only because the join carries `tm.DeleteAt = 0`; without it the leavers are counted.
    // An absolute number is safe here because no other suite writes to this team.
    assert_eq!(
        count(&client, &token, &format!("in_team={}", f.deserted_team)).await,
        0,
        "a team everybody left counts nobody"
    );
}

/// The `sysconsole` permission, which an ordinary user does not hold.
#[tokio::test]
async fn a_plain_caller_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, PATH).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// The role parameters are Go's, at any value — including the empty string.
#[tokio::test]
async fn the_role_parameters_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for query in [
        "roles=system_admin",
        "roles=",
        "channel_roles=channel_user",
        "team_roles=team_user",
    ] {
        let rs = client
            .get(format!("{RUST}{PATH}?{query}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "?{query} must be forwarded"
        );
    }
}

/// No session is a 401 on both.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{PATH}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{PATH}");
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
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {PATH} must be forwarded"
        );
    }
}
