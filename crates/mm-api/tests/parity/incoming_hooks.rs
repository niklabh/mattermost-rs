//! Cross-server parity for `GET /api/v4/hooks/incoming` — `getIncomingHooks`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity incoming_hooks
//! ```
//!
//! # The rows are planted, not created through the API
//!
//! Only an admin may create an incoming webhook — `manage_own_incoming_webhooks` is granted to
//! `system_admin` and `team_admin` and to nothing else on a stock server — so the API cannot
//! produce a hook owned by a **non**-admin, which is exactly the row that makes
//! `manage_others_incoming_webhooks` observable. It also cannot produce a soft-deleted hook, or
//! two hooks sharing a display name. All five rows are written straight into `IncomingWebhooks`;
//! Go caches only `GetIncoming(id)` (localcachelayer/webhook_layer.go:41) and never the list
//! queries, so a direct write is visible to both servers immediately.
//!
//! # What each planted row is for
//!
//! | Row | Why |
//! |---|---|
//! | two hooks sharing a display name, inserted in **reverse** id order | the `ORDER BY DisplayName, Id` tiebreak — without `Id` the heap order wins and the two disagree |
//! | a hook owned by a plain user | `manage_others_incoming_webhooks` clears the user filter; without such a row the admin's answer is the same either way |
//! | a soft-deleted hook | the `DeleteAt = 0` predicate |
//! | a hook on a second team | the `TeamId` predicate, and that the unscoped list crosses teams |

use crate::common;

use common::{
    RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures, stack_enabled,
};

const PATH: &str = "/api/v4/hooks/incoming";

struct Fixture {
    team: String,
    other_team: String,
    /// `[alpha_low_id, alpha_high_id, beta_owned_by_the_plain_user]`, in the order the route must
    /// answer with.
    expected: [String; 3],
    deleted: String,
    other_team_hook: String,
    plain_token: String,
    /// A **team admin** of `team` and nothing more: holds both webhook permissions *on that team*
    /// and neither at system scope, which is the only way to tell the route's two permission
    /// scopes apart.
    team_admin_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team = create_team(client, token, "hooks").await;
            let other_team = create_team(client, token, "hooksother").await;
            let plain = create_plain_user(client, token, &team, "hooks").await;
            let team_admin = create_plain_user(client, token, &team, "hooksadmin").await;
            promote_to_team_admin(client, token, &team, &team_admin.id).await;
            let admin = common::logged_in_user_id().to_owned();

            // 26 characters each, and **`0001` sorts after `0002` in insertion order on purpose**:
            // the two share a display name, so a query that dropped the `Id` tiebreak would fall
            // back to the heap and answer them the other way round.
            let id = |n: u32| format!("mmrshook000000000000000{n:03}");
            let ids: Vec<String> = (1..=5).map(id).collect();
            for hook_id in &ids {
                assert_eq!(hook_id.len(), 26, "{hook_id} must be a valid id");
            }

            plant(&[
                // (id, team, owner, display_name, delete_at)
                (&ids[1], &team, &admin, "mmrs hook alpha", 0),
                (&ids[0], &team, &admin, "mmrs hook alpha", 0),
                (&ids[2], &team, &plain.id, "mmrs hook beta", 0),
                (
                    &ids[3],
                    &team,
                    &admin,
                    "mmrs hook deleted",
                    1_788_636_490_000,
                ),
                (&ids[4], &other_team, &admin, "mmrs hook otherteam", 0),
            ])
            .await;

            Fixture {
                team,
                other_team,
                expected: [ids[0].clone(), ids[1].clone(), ids[2].clone()],
                deleted: ids[3].clone(),
                other_team_hook: ids[4].clone(),
                plain_token: plain.token,
                team_admin_token: team_admin.token,
            }
        })
        .await
}

/// `PUT /teams/{team_id}/members/{user_id}/schemeRoles` — the only way to grant a permission on
/// one team and not system-wide, which is what separates `SessionHasPermissionToTeam` from
/// `SessionHasPermissionTo`.
async fn promote_to_team_admin(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    user_id: &str,
) {
    let response = client
        .put(format!(
            "{}/api/v4/teams/{team_id}/members/{user_id}/schemeRoles",
            common::GO
        ))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "scheme_admin": true, "scheme_user": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "promoting {user_id} to team admin failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Clear this suite's rows and write the given ones, in the order given.
///
/// Cleared on the way **in**: an assertion panics past any teardown, and a hook left behind would
/// join the next run's page and turn a deterministic list into a growing one.
async fn plant(rows: &[(&str, &str, &str, &str, i64)]) {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");

    sqlx::query("DELETE FROM incomingwebhooks WHERE id LIKE 'mmrshook%'")
        .execute(&pool)
        .await
        .expect("earlier rows are cleared");

    for (id, team, owner, display_name, delete_at) in rows {
        // Every column Go's `model.IncomingWebhook` scans into a non-pointer field gets a value.
        // `LastUsed` is the only one the schema itself makes `NOT NULL`; the rest would be a scan
        // failure for the **Go** server, which reads these same rows.
        sqlx::query(
            "INSERT INTO incomingwebhooks
                (id, createat, updateat, deleteat, userid, channelid, teamid,
                 displayname, description, username, iconurl, channellocked, lastused)
             VALUES ($1, 1788636490668, 1788636490669, $5, $3, '', $2,
                     $4, 'planted by the parity suite', '', '', false, 0)",
        )
        .bind(id)
        .bind(team)
        .bind(owner)
        .bind(display_name)
        .bind(delete_at)
        .execute(&pool)
        .await
        .expect("the hook row is written");
    }
}

fn ids_of(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Vec<serde_json::Value>>(body)
        .unwrap_or_else(|e| panic!("decoding {}: {e}", String::from_utf8_lossy(body)))
        .into_iter()
        .map(|hook| hook["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The team-scoped list, byte for byte, in `DisplayName, Id` order — and it contains the plain
/// user's hook, which is the whole of `manage_others_incoming_webhooks`.
#[tokio::test]
async fn the_team_scoped_list_is_byte_identical_and_ordered() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{PATH}?team_id={}", fixture.team);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;

    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}: the list must be byte-identical"
    );
    assert!(
        !rs.ends_with(b"\n"),
        "`json.Marshal` + `w.Write` (webhook.go:267) — no encoder, no newline"
    );
    assert_eq!(
        ids_of(&go),
        fixture.expected.to_vec(),
        "three live hooks, ordered by display name then id — and the third belongs to someone else"
    );
}

/// A soft-deleted hook and another team's hook are both absent; the second one is present when
/// its own team is asked for, so the absence is the predicate and not a missing row.
#[tokio::test]
async fn deleted_and_other_team_hooks_are_excluded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{PATH}?team_id={}", fixture.team);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    let listed = ids_of(&go);
    assert!(
        !listed.contains(&fixture.deleted),
        "DeleteAt = 0: {listed:?}"
    );
    assert!(
        !listed.contains(&fixture.other_team_hook),
        "TeamId filter: {listed:?}"
    );
    assert_eq!(ids_of(&rs), listed, "{p}");

    let other = format!("{PATH}?team_id={}", fixture.other_team);
    let (go_other, rs_other) = fetch_both_stable(&client, &token, &other).await;
    assert_eq!(
        ids_of(&go_other),
        vec![fixture.other_team_hook.clone()],
        "the other team's hook really does exist"
    );
    assert_eq!(ids_of(&rs_other), ids_of(&go_other), "{other}");
}

/// The unscoped list has **no team predicate**, so it crosses teams — the same admin, the same
/// permission, a different answer.
#[tokio::test]
async fn the_unscoped_list_crosses_teams() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let (go, rs) = fetch_both_stable(&client, &token, PATH).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}: the unscoped list must be byte-identical"
    );

    // Absolute membership rather than an absolute list: this reads a table no other suite writes
    // to, but a page size of 60 is not a promise that every planted row fits once other runs'
    // rows exist. Both hooks below are on *different* teams, which is the claim.
    let listed = ids_of(&go);
    for planted in [&fixture.expected[0], &fixture.other_team_hook] {
        assert!(
            listed.contains(planted),
            "{planted} is missing from the unscoped list: {listed:?}"
        );
    }
    assert!(
        !listed.contains(&fixture.deleted),
        "and the deleted one is still excluded: {listed:?}"
    );
}

/// `include_total_count` turns an **array** into an **object**. A client parsing the response has
/// to branch on it, so it is a type change and not a field addition.
#[tokio::test]
async fn include_total_count_changes_the_shape() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{PATH}?team_id={}&include_total_count=true", fixture.team);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );

    let body: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert!(body.is_object(), "{p}: an object, not an array: {body}");
    assert_eq!(
        body["total_count"], 3,
        "the count uses the same cleared user filter as the page: {body}"
    );
    assert_eq!(
        body["incoming_webhooks"]
            .as_array()
            .expect("an array")
            .len(),
        3
    );

    // And the values Go's `strconv.ParseBool` rejects keep the array shape.
    for query in [
        "include_total_count",
        "include_total_count=yes",
        "include_total_count=",
    ] {
        let p = format!("{PATH}?team_id={}&{query}", fixture.team);
        let (go, rs) = fetch_both_stable(&client, &token, &p).await;
        assert!(
            serde_json::from_slice::<serde_json::Value>(&go)
                .expect("decodes")
                .is_array(),
            "{p}: `{query}` is false, so the shape stays an array"
        );
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
    }
}

/// `page * per_page` is the offset, and the pages concatenate back into the whole list.
#[tokio::test]
async fn the_pages_split_the_list() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let mut paged: Vec<String> = Vec::new();
    for page in 0..2 {
        let p = format!("{PATH}?team_id={}&page={page}&per_page=2", fixture.team);
        let (go, rs) = fetch_both_stable(&client, &token, &p).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
        paged.extend(ids_of(&go));
    }
    assert_eq!(
        paged,
        fixture.expected.to_vec(),
        "page 1 starts where page 0 stopped — the offset is page * per_page"
    );

    // An empty page is `[]`, not `null`: both store functions start from a literal empty slice
    // (webhook_store.go:179, :199), unlike `getUserAudits`' nil.
    let p = format!("{PATH}?team_id={}&page=9&per_page=60", fixture.team);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, b"[]", "an empty page is an empty array, not null");
    assert_eq!(rs, go, "{p}");
}

/// A user with neither permission is refused, on **both** branches — and the id Go reports is the
/// `manage_own` one even though the route also consults `manage_others`.
#[tokio::test]
async fn a_user_without_the_permission_is_refused_on_both_branches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for p in [PATH.to_owned(), format!("{PATH}?team_id={}", fixture.team)] {
        let ((go_status, go), (rs_status, rs)) =
            fetch_both_raw(&client, &fixture.plain_token, &p).await;
        assert_eq!(go_status, 403, "{p}: a plain user has neither permission");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(body["id"], "api.context.permissions.app_error", "{p}");
    }
}

/// A `team_id` that names no team: the check is `SessionHasPermissionToTeam`, which a system
/// admin passes unconditionally, so this is a 200 over an empty list rather than a 404.
#[tokio::test]
async fn an_unknown_team_id_is_an_empty_list_for_an_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let p = format!("{PATH}?team_id=mmrsnosuchteam000000000001");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 200, "{p}: not a 404 — nothing looks the team up");
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(go, b"[]", "{p}");
    assert_eq!(rs, go, "{p}");
}

/// Registering the `GET` must not turn the neighbouring `POST` into our 405.
/// **Repointed when `POST` was migrated.** The point of this test is that a method this server
/// does not register still reaches Go rather than meeting axum's 405 — not that any particular
/// method is unmigrated. `POST` is now served here, so the probe is `PATCH`, which Go does not
/// register on this path either (it answers its own 404).
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ours = client
        .patch(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "PATCH {PATH} must be forwarded"
    );
}

/// **The two permission scopes are different questions.** A team admin holds both webhook
/// permissions on its own team and neither system-wide, so `?team_id=<its team>` is a 200 and the
/// bare route is a 403 — for the same caller, in the same second.
///
/// A port that asked `SessionHasPermissionTo` on both branches would answer 403 to the first; one
/// that asked `SessionHasPermissionToTeam` on both would need a team it does not have. This is the
/// only fixture in the suite that can tell the two apart, and it fails **open** without it.
#[tokio::test]
async fn the_team_scope_and_the_system_scope_are_different_answers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let scoped = format!("{PATH}?team_id={}", fixture.team);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.team_admin_token, &scoped).await;
    assert_eq!(go_status, 200, "a team admin may list its own team's hooks");
    assert_eq!(rs_status, go_status, "{scoped}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{scoped}"
    );
    assert_eq!(
        ids_of(&go),
        fixture.expected.to_vec(),
        "and `manage_others_incoming_webhooks` on the team clears the user filter too"
    );

    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.team_admin_token, PATH).await;
    assert_eq!(
        go_status,
        403,
        "the same caller has no *system*-scoped grant: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{PATH}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, PATH);
    assert_eq!(body["id"], "api.context.permissions.app_error");
}

/// The other team is one the team admin is **not** a member of at all, so its team-scoped check
/// fails there — the grant is per team, not "any team".
#[tokio::test]
async fn a_team_admin_is_refused_on_a_team_it_does_not_administer() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{PATH}?team_id={}", fixture.other_team);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.team_admin_token, &p).await;
    assert_eq!(go_status, 403, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
}
