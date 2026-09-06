//! Cross-server parity for `GET /api/v4/channels/{channel_id}/common_teams` —
//! `getDirectOrGroupMessageMembersCommonTeams`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity common_teams
//! ```
//!
//! # The fixture's whole job is to make the intersection deterministic
//!
//! "Teams every member is in" is an intersection over the *fixture user's* team list too, and the
//! admin belongs to every team any suite in this binary ever creates. So each group message pairs
//! the admin with users who belong to **exactly** the teams this suite made — the intersection is
//! then those teams and nothing else, whatever the rest of the run is doing.
//!
//! # Four answers, two of them empty
//!
//! `[{…}]` for a common team, `[]` for none, `null` for a caller who is not an active member of
//! the channel (Go's nil slice, channel.go:4275), and a **forward** when an active member is a
//! bot — whose exemption is decided by the plugin manifests loaded in Go's memory
//! (app/bot.go:378), which this process cannot see.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, create_team, fetch_both_raw, fetch_both_stable, go_minted_token,
    purge_api_fixtures, remove_user_from_team, stack_enabled,
};

struct Fixture {
    team_a: String,
    team_b: String,
    /// `[admin, alpha, beta]` — alpha and beta are in both teams.
    both: String,
    /// `[admin, alpha, gamma]` — gamma is in `team_a` only.
    one: String,
    /// `[admin, gamma, delta]` — gamma is in A, delta in B, so nothing is common.
    none: String,
    /// `[alpha, beta, gamma]` — the admin is **not** a member, but can read it.
    theirs: String,
    /// `[admin, alpha]`, a direct message.
    dm: String,
    /// `[admin, alpha, <a bot>]`.
    with_bot: String,
    /// An ordinary public channel — the wrong type for this route.
    public: String,
    /// `[admin, alpha, epsilon]` where epsilon has since been **deactivated**: the active-member
    /// filter is the only thing keeping the answer at both teams.
    with_deactivated: String,
    /// alpha's own token — a caller the team sanitiser actually strips fields for.
    alpha_token: String,
    /// Logged in first, then promoted to `system_guest`.
    guest_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_a = create_team(client, token, "commonteamsa").await;
            let team_b = create_team(client, token, "commonteamsb").await;
            let public = create_channel(client, token, &team_a, "commonteams").await;

            let admin = common::logged_in_user_id().to_owned();
            let alpha = create_plain_user(client, token, &team_a, "ctalpha").await;
            let beta = create_plain_user(client, token, &team_a, "ctbeta").await;
            let gamma = create_plain_user(client, token, &team_a, "ctgamma").await;
            let delta = create_plain_user(client, token, &team_b, "ctdelta").await;
            join_team(client, token, &team_b, &alpha.id).await;
            join_team(client, token, &team_b, &beta.id).await;

            // **A membership gamma left.** `DELETE /teams/{id}/members/{id}` is a soft delete, so
            // the row survives with a non-zero `DeleteAt` — which is the only thing separating
            // "is a member" from "has a row" in the common-teams query.
            join_team(client, token, &team_b, &gamma.id).await;
            remove_user_from_team(client, token, &team_b, &gamma.id).await;

            // **A team everyone is in and that has since been archived.** `Teams.DeleteAt != 0`
            // is a second, independent filter in the same query.
            let team_c = create_team(client, token, "commonteamsc").await;
            for member in [&alpha.id, &beta.id, &gamma.id, &delta.id] {
                join_team(client, token, &team_c, member).await;
            }
            archive_team(client, token, &team_c).await;

            // **A channel member who has since been deactivated.**
            let epsilon = create_plain_user(client, token, &team_a, "ctepsilon").await;
            let with_deactivated =
                group_channel(client, token, &[&admin, &alpha.id, &epsilon.id]).await;
            deactivate_user(client, token, &epsilon.id).await;

            // The guest must log in **before** being promoted: guest accounts are disabled on this
            // server, so a login as a guest is refused — but an existing session survives the role
            // change, and `IsGuest()` reads the user row rather than the session. That is the only
            // way to reach this route's first gate at all.
            let guest = create_plain_user(client, token, &team_a, "ctguest").await;
            set_roles(client, token, &guest.id, "system_guest").await;

            let bot = a_bot_user_id().await;

            Fixture {
                both: group_channel(client, token, &[&admin, &alpha.id, &beta.id]).await,
                one: group_channel(client, token, &[&admin, &alpha.id, &gamma.id]).await,
                none: group_channel(client, token, &[&admin, &gamma.id, &delta.id]).await,
                // Created with **alpha's** token, not the admin's: `createGroupChannel` puts the
                // requesting user in the channel, so a group message the admin merely lists is
                // still one the admin joins. Only a channel it never touched leaves it a
                // non-member — which is the whole point of this fixture.
                theirs: group_channel(client, &alpha.token, &[&alpha.id, &beta.id, &gamma.id])
                    .await,
                dm: direct_channel(client, token, &admin, &alpha.id).await,
                with_bot: group_channel(client, token, &[&admin, &alpha.id, &bot]).await,
                team_a,
                team_b,
                public,
                with_deactivated,
                alpha_token: alpha.token,
                guest_token: guest.token,
            }
        })
        .await
}

async fn join_team(client: &reqwest::Client, admin_token: &str, team_id: &str, user_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": user_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "adding {user_id} to {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// `DELETE /api/v4/teams/{team_id}` — an **archive**, so `Teams.DeleteAt` becomes non-zero and
/// the row stays.
async fn archive_team(client: &reqwest::Client, admin_token: &str, team_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "archiving {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// `DELETE /api/v4/users/{user_id}` — a deactivation: `Users.DeleteAt` becomes non-zero and the
/// channel membership row survives, which is exactly the shape the active filter exists for.
async fn deactivate_user(client: &reqwest::Client, admin_token: &str, user_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deactivating {user_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn set_roles(client: &reqwest::Client, admin_token: &str, user_id: &str, roles: &str) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/roles"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "roles": roles }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "setting {user_id}'s roles failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn group_channel(client: &reqwest::Client, token: &str, user_ids: &[&String]) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/group"))
        .header("Authorization", format!("Bearer {token}"))
        .json(user_ids)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the group message failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn direct_channel(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&vec![a, b])
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the direct message failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Any bot on this installation — the development stack has the plugin-owned `calls` bot and the
/// built-in `system-bot`, and either reaches the branch, because the branch is "is a bot" and not
/// "is exempt".
async fn a_bot_user_id() -> String {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    sqlx::query_scalar::<_, String>(
        "SELECT b.userid FROM bots b JOIN users u ON u.id = b.userid
          WHERE b.deleteat = 0 AND u.deleteat = 0 ORDER BY b.userid LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("the development stack has at least one bot")
}

fn path(channel_id: &str) -> String {
    format!("/api/v4/channels/{channel_id}/common_teams")
}

fn team_ids(body: &[u8]) -> std::collections::BTreeSet<String> {
    serde_json::from_slice::<Vec<serde_json::Value>>(body)
        .unwrap_or_else(|e| panic!("decoding {}: {e}", String::from_utf8_lossy(body)))
        .into_iter()
        .map(|team| team["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The intersection, byte for byte, for a group message and for a direct message.
#[tokio::test]
async fn the_common_teams_are_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let expected: std::collections::BTreeSet<String> =
        [fixture.team_a.clone(), fixture.team_b.clone()]
            .into_iter()
            .collect();

    for channel in [&fixture.both, &fixture.dm] {
        let p = path(channel);
        let (go, rs) = fetch_both_stable(&client, &token, &p).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
        assert!(
            rs.ends_with(b"\n"),
            "{p}: `json.NewEncoder(w).Encode` writes a newline"
        );
        assert_eq!(
            team_ids(&go),
            expected,
            "{p}: both teams, and nothing the admin alone belongs to"
        );
    }
}

/// One member in only one of the two teams narrows the intersection to that team — so the answer
/// is the *intersection* and not "the teams of whoever asked".
#[tokio::test]
async fn a_member_in_one_team_narrows_the_intersection() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.one);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
    assert_eq!(
        team_ids(&go),
        [fixture.team_a.clone()].into_iter().collect(),
        "{p}: team B is out because one member is not in it"
    );
}

/// **`[]` and `null` are different answers.** No team in common is an empty array; a caller who is
/// not an active member of the channel is Go's nil slice.
#[tokio::test]
async fn no_common_team_is_an_empty_array_and_a_non_member_is_null() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.none);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, b"[]\n", "{p}: an empty intersection is an empty array");
    assert_eq!(rs, go, "{p}");

    // The admin is a system admin, so it may *read* a group message it is not in — which is the
    // only way to reach the short-circuit at all. A caller without that permission is refused
    // before the membership question is asked.
    let p = path(&fixture.theirs);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        go, b"null\n",
        "{p}: not an active member is a nil slice, not an empty one"
    );
    assert_eq!(rs, go, "{p}");
}

/// A bot member sends the request back to Go, because its exemption is decided by the plugin
/// manifests in Go's memory. The **answer** is Go's; what this asserts is that we did not answer.
#[tokio::test]
async fn a_bot_member_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.with_bot);
    let ours = client
        .get(format!("{RUST}{p}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "{p}: a bot member is not ours to reason about"
    );
    let ours_status = ours.status().as_u16();
    let ours_body = ours.bytes().await.expect("body reads").to_vec();

    let direct = client
        .get(format!("{GO}{p}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), ours_status, "{p}");
    assert_eq!(
        direct.bytes().await.expect("body reads").to_vec(),
        ours_body,
        "{p}: and the forward delivers Go's bytes unaltered"
    );
}

/// An ordinary channel is the wrong type: a **400** with its own id, raised after the permission
/// check passes.
#[tokio::test]
async fn a_normal_channel_is_the_wrong_type() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.public);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        body["id"], "app.channel.get_common_teams.incorrect_channel_type",
        "{p}"
    );
}

/// A channel that does not exist is a **403**, not a 404 — nothing looks it up before the
/// permission check, and the check answers false when it cannot fetch it. The same answer a
/// stranger gets, which is the point.
#[tokio::test]
async fn an_unknown_channel_is_a_403_and_not_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let p = path("mmrsnosuchchannel000000001");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 403, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.permissions.app_error", "{p}");
}

/// **The guest gate comes first**, and its id names group-message *conversion* on a read route.
/// A guest asking about a channel it could not read anyway gets this, not the permission error.
#[tokio::test]
async fn a_guest_is_refused_before_any_permission_question() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.both);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.guest_token, &p).await;
    assert_eq!(go_status, 403, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        body["id"], "api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error",
        "{p}: the guest id, not `api.context.permissions.app_error`"
    );

    // And the gate really is first: the same guest gets the same error for a channel that does
    // not exist, where a permission check would have refused it for a different reason.
    let p = path("mmrsnosuchchannel000000001");
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.guest_token, &p).await;
    assert_eq!(go_status, 403, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        body["id"], "api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error",
        "{p}: still the guest error, so the guest check ran before the permission one"
    );
}

/// A short id is `RequireChannelId`'s 400; a non-mux segment is forwarded.
#[tokio::test]
async fn a_short_id_is_a_400_and_a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let p = path("abc");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error", "{p}");

    let p = path("not-an-id");
    let ours = client
        .get(format!("{RUST}{p}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "{p}"
    );
    assert_eq!(ours.status().as_u16(), 404, "{p}");
}

/// **Three filters that each remove one thing**, and a fixture row for each.
///
/// - gamma **left** team B, so a membership row with a non-zero `DeleteAt` must not count.
/// - team C is **archived**, and everyone is in it, so a team with a non-zero `DeleteAt` must not
///   be common to anyone.
/// - epsilon is **deactivated**, so an inactive channel member must not narrow the intersection.
///
/// Each is invisible without its row: drop any one filter and this test still passes on the
/// others' fixtures, which is why they are asserted together and named apart.
#[tokio::test]
async fn left_memberships_archived_teams_and_deactivated_members_are_all_excluded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let both_teams: std::collections::BTreeSet<String> =
        [fixture.team_a.clone(), fixture.team_b.clone()]
            .into_iter()
            .collect();

    // gamma left team B and team C is archived — so the `[admin, alpha, gamma]` group message is
    // team A alone, not team A plus the two.
    let p = path(&fixture.one);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        team_ids(&go),
        [fixture.team_a.clone()].into_iter().collect(),
        "{p}: a left membership and an archived team are both out"
    );
    assert_eq!(team_ids(&rs), team_ids(&go), "{p}");

    // The archived team is common to alpha and beta too, and still absent.
    let p = path(&fixture.both);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        team_ids(&go),
        both_teams,
        "{p}: the archived team is not listed"
    );
    assert_eq!(team_ids(&rs), team_ids(&go), "{p}");

    // epsilon is deactivated and in team A only; counting them would drop team B.
    let p = path(&fixture.with_deactivated);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
    assert_eq!(
        team_ids(&go),
        both_teams,
        "{p}: a deactivated member does not narrow the intersection"
    );
}

/// **The teams are sanitised per caller.** The fixture admin has `manage_system`, so nothing is
/// stripped for it and the sanitiser is invisible; alpha is an ordinary member of both teams and
/// loses `email`, `allowed_domains` and `invite_id`.
#[tokio::test]
async fn the_teams_are_sanitised_for_an_ordinary_member() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.both);
    let (go, rs) = fetch_both_stable(&client, &fixture.alpha_token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}: as a plain member"
    );

    let teams: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("decodes");
    assert!(!teams.is_empty(), "{p}: alpha is in both teams");
    for team in &teams {
        // `email` only. `SanitizeTeam` clears `InviteId` unless the caller has **`invite_user`**,
        // which the default `team_user` role grants every member — so the invite id survives for
        // alpha, and asserting it away would be asserting a rule Go does not have.
        assert_eq!(team["email"], "", "{p}: sanitised away for a plain member");
        assert_ne!(
            team["invite_id"], "",
            "{p}: and the invite id is *not* stripped, because `invite_user` is a member's"
        );
    }

    // And the admin's own answer keeps them, so the assertion above is about the sanitiser and
    // not about the column being empty.
    let (go_admin, _) = fetch_both_stable(&client, &token, &p).await;
    let admin_teams: Vec<serde_json::Value> = serde_json::from_slice(&go_admin).expect("decodes");
    assert!(
        admin_teams.iter().any(|team| team["email"] != ""),
        "an admin sees the team email: {go_admin:?}"
    );
}
