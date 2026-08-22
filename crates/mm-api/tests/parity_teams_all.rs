//! `GET /api/v4/teams` — `getAllTeams` — against the running Go server.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_teams_all
//! ```
//!
//! # Why this suite mints its own roles
//!
//! The route branches on `list_private_teams` and `list_public_teams`, and no pair of built-in
//! roles covers the matrix: `system_admin` holds both, `system_user` holds only
//! `list_public_teams`, and nothing at all grants private-without-public. So the fixture writes
//! four rows into `Roles` — both, private-only, public-only, neither — and assigns each to its
//! own user through Go's `PUT /users/{id}/roles`. Both servers read the same `Roles` table, so
//! the actor is as real for one as for the other, and every cell of the matrix is **measured**
//! rather than reasoned about. A fifth role adds
//! `sysconsole_read_compliance_data_retention_policy` for the `exclude_policy_constrained` gate.
//!
//! # Why nothing here asserts an absolute count
//!
//! This route lists the whole `Teams` table, which three sibling worktrees are also writing to.
//! Every assertion is either a byte-for-byte comparison of the two servers' answers to the same
//! request (taken with [`common::fetch_both_stable`], which re-reads Go on both sides of ours and
//! retries until it settles) or a membership question about the ids this file created. A test
//! that pinned `total_count` would be green on the wrong evidence.

mod common;

use common::{GO, RUST, fetch_both_raw, fetch_both_stable, stack_enabled};

/// Teams and users this file authors. `mmrs-parity-` and `mmrsplain` are the two prefixes
/// [`common::purge_api_fixtures`] already clears, so the teardown for this suite is the next
/// run's purge.
const TAG: &str = "tall";

struct Fixture {
    admin_token: String,
    /// `list_private_teams list_public_teams`.
    both: String,
    /// `list_private_teams` alone.
    private_only: String,
    /// `list_public_teams` alone.
    public_only: String,
    /// No permissions at all.
    neither: String,
    /// Both list permissions plus `sysconsole_read_compliance_data_retention_policy`.
    retention: String,
    open_team: String,
    private_team: String,
    archived_team: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the stack tests");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

/// Write a role both servers will read, and hand it to a freshly created user.
///
/// The role is inserted straight into `Roles`: Mattermost has no create-role endpoint, and the
/// built-in roles cannot express three of the four cells this suite needs. `schememanaged` and
/// `builtin` are false so nothing tries to reconcile it against a scheme.
async fn user_with_role(
    client: &reqwest::Client,
    pool: &sqlx::PgPool,
    admin_token: &str,
    suffix: &str,
    permissions: &str,
) -> String {
    let role = format!("mmrs_tall_{suffix}");
    let id = format!("mmrstall{suffix:0<18}");
    sqlx::query(
        "INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                            permissions, schememanaged, builtin, schemeid)
         VALUES ($1, $2, $2, $2, 1, 1, 0, $3, false, false, NULL)",
    )
    .bind(&id[..26.min(id.len())])
    .bind(&role)
    .bind(permissions)
    .execute(pool)
    .await
    .expect("inserts the fixture role");

    let username = format!("mmrsplain{TAG}{suffix}");
    let password = "Mmrs-Plain-1234";
    let created = client
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "email": format!("{username}@mmrs.invalid"),
            "username": username,
            "password": password,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        created.status().is_success(),
        "creating {username} failed: {}",
        created.text().await.unwrap_or_default()
    );
    let user: serde_json::Value = created.json().await.expect("the user decodes");
    let user_id = user["id"].as_str().expect("an id").to_owned();

    // Before the login: the session captures the user's roles when it is minted, so assigning
    // them afterwards would leave the token carrying `system_user`.
    let assigned = client
        .put(format!("{GO}/api/v4/users/{user_id}/roles"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "roles": role }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        assigned.status().is_success(),
        "assigning {role} failed: {}",
        assigned.text().await.unwrap_or_default()
    );

    let login = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": password }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(login.status(), 200, "{username} cannot log in");
    login
        .headers()
        .get("token")
        .expect("Go returns a token header")
        .to_str()
        .expect("ASCII")
        .to_owned()
}

async fn create_team(
    client: &reqwest::Client,
    admin_token: &str,
    suffix: &str,
    allow_open_invite: bool,
) -> String {
    let name = format!("mmrs-parity-{TAG}{suffix}");
    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "name": name,
            // Distinct, and chosen to sort last: `~` is the last printable ASCII byte, so these
            // never interleave with another worktree's fixtures under `ORDER BY DisplayName`.
            "display_name": format!("~mmrs {TAG} {suffix}"),
            "type": if allow_open_invite { "O" } else { "I" },
            "allow_open_invite": allow_open_invite,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    let id = created["id"].as_str().expect("an id").to_owned();

    // `createTeam` does not always honour the posted flag; patch it so the column is exactly what
    // the open-invite filter is being tested against.
    let patched = client
        .put(format!("{GO}/api/v4/teams/{id}/patch"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "allow_open_invite": allow_open_invite }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        patched.status().is_success(),
        "patching allow_open_invite failed: {}",
        patched.text().await.unwrap_or_default()
    );

    id
}

async fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(build_fixture).await
}

async fn build_fixture() -> Fixture {
    common::purge_api_fixtures().await;
    let client = common::client();
    let admin_token = common::go_minted_token(&client).await;
    let pool = pool().await;

    // The purge clears users and teams by name prefix; the roles this suite invents are its own
    // to clean up, and a leftover row would collide on the unique `name`.
    sqlx::query("DELETE FROM roles WHERE name LIKE 'mmrs_tall_%'")
        .execute(&pool)
        .await
        .expect("clears leftover fixture roles");

    let open_team = create_team(&client, &admin_token, "open", true).await;
    let private_team = create_team(&client, &admin_token, "priv", false).await;
    let archived_team = create_team(&client, &admin_token, "arch", false).await;
    let archived = client
        .delete(format!("{GO}/api/v4/teams/{archived_team}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        archived.status().is_success(),
        "archiving the fixture team failed: {}",
        archived.text().await.unwrap_or_default()
    );

    // `ORDER BY DisplayName` carries no tiebreak on either server, so two teams sharing a display
    // name would let Go and Postgres disagree on their order for reasons that have nothing to do
    // with this handler. Diagnose that here rather than as a mystery byte diff.
    let duplicates: Vec<(Option<String>, i64)> =
        sqlx::query_as("SELECT displayname, count(*) FROM teams GROUP BY 1 HAVING count(*) > 1")
            .fetch_all(&pool)
            .await
            .expect("checks for tied sort keys");
    assert!(
        duplicates.is_empty(),
        "two teams share a display name {duplicates:?}; `ORDER BY DisplayName` has no tiebreak, \
         so the byte comparisons below would flake on the tie rather than on the handler"
    );

    Fixture {
        both: user_with_role(
            &client,
            &pool,
            &admin_token,
            "both",
            "list_private_teams list_public_teams",
        )
        .await,
        private_only: user_with_role(&client, &pool, &admin_token, "priv", "list_private_teams")
            .await,
        public_only: user_with_role(&client, &pool, &admin_token, "publ", "list_public_teams")
            .await,
        neither: user_with_role(&client, &pool, &admin_token, "none", "").await,
        retention: user_with_role(
            &client,
            &pool,
            &admin_token,
            "retn",
            "list_private_teams list_public_teams sysconsole_read_compliance_data_retention_policy",
        )
        .await,
        admin_token,
        open_team,
        private_team,
        archived_team,
    }
}

fn ids(body: &[u8]) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_slice(body).expect("the body is JSON");
    let array = match &value {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => o["teams"].as_array().expect("teams is an array").clone(),
        other => panic!("unexpected body shape: {other}"),
    };
    array
        .iter()
        .map(|t| t["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The default request, byte for byte. Everything else in this file narrows from here.
#[tokio::test]
async fn the_default_listing_matches_go_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) = fetch_both_stable(&client, &f.admin_token, "/api/v4/teams").await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rust));
}

/// `GetAllPage` has **no `DeleteAt` filter**, so an archived team is listed like any other — and
/// `total_count` counts it, because `AnalyticsTeamCount`'s deleted filter only engages on an
/// explicit `IncludeDeleted = false` that no route sets.
#[tokio::test]
async fn archived_teams_are_listed_and_counted() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) = fetch_both_stable(&client, &f.admin_token, "/api/v4/teams?per_page=200").await;
    assert_eq!(go, rust);
    assert!(
        ids(&go).contains(&f.archived_team),
        "Go itself lists the archived team; a deleteat = 0 predicate would be a divergence"
    );

    let (go, rust) = fetch_both_stable(
        &client,
        &f.admin_token,
        "/api/v4/teams?per_page=200&include_total_count=true",
    )
    .await;
    assert_eq!(go, rust);
    assert!(ids(&go).contains(&f.archived_team));
}

/// `include_total_count=true` switches the body from a bare array to an object. Compared as
/// bytes, then re-checked as a shape so a future array-shaped regression cannot pass by
/// agreeing with a Go server that also broke.
#[tokio::test]
async fn include_total_count_switches_the_response_shape() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) = fetch_both_stable(
        &client,
        &f.admin_token,
        "/api/v4/teams?include_total_count=true",
    )
    .await;
    assert_eq!(go, rust);

    let value: serde_json::Value = serde_json::from_slice(&rust).expect("JSON");
    let object = value.as_object().expect("an object, not an array");
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        vec!["teams", "total_count"],
        "both keys, in Go's field order"
    );
    assert!(object["total_count"].is_i64());

    let (go, rust) = fetch_both_stable(&client, &f.admin_token, "/api/v4/teams").await;
    assert_eq!(go, rust);
    assert!(
        serde_json::from_slice::<serde_json::Value>(&rust)
            .expect("JSON")
            .is_array(),
        "without the flag it is a bare array"
    );
}

/// `per_page=0` is an **empty page** on this route — squirrel's `LIMIT 0` — and not "everything",
/// which is what the same parameter means to `getChannelMembers`. The count half is unaffected
/// by the limit, so the object form still reports the real total beside an empty list.
#[tokio::test]
async fn per_page_zero_is_an_empty_page_not_the_whole_table() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) = fetch_both_stable(&client, &f.admin_token, "/api/v4/teams?per_page=0").await;
    assert_eq!(go, rust);
    assert_eq!(rust, b"[]", "an empty page, serialised as an array");

    let (go, rust) = fetch_both_stable(
        &client,
        &f.admin_token,
        "/api/v4/teams?per_page=0&include_total_count=true",
    )
    .await;
    assert_eq!(go, rust);
    let value: serde_json::Value = serde_json::from_slice(&rust).expect("JSON");
    assert_eq!(value["teams"], serde_json::json!([]));
    assert!(
        value["total_count"].as_i64().expect("a number") > 0,
        "the count ignores the limit"
    );
}

/// Paging is `offset = per_page * page`, and both servers walk the same `ORDER BY DisplayName`
/// sequence. Asserted page by page as bytes, plus a disjointness check so a mutation that
/// dropped the offset would fail on more than one page's worth of luck.
#[tokio::test]
async fn paging_walks_the_same_order_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let mut seen: Vec<String> = Vec::new();
    for page in 0..3 {
        let path = format!("/api/v4/teams?page={page}&per_page=2");
        let (go, rust) = fetch_both_stable(&client, &f.admin_token, &path).await;
        assert_eq!(go, rust, "page {page} differs");
        for id in ids(&go) {
            assert!(!seen.contains(&id), "page {page} repeats {id}");
            seen.push(id);
        }
    }

    // And the concatenation is the prefix of the unpaged listing.
    let (go, _) = fetch_both_stable(&client, &f.admin_token, "/api/v4/teams?per_page=200").await;
    let all = ids(&go);
    assert_eq!(&all[..seen.len()], &seen[..]);
}

/// A caller holding only `list_public_teams` sees `AllowOpenInvite = true` teams and nothing
/// else. Crossing the two values here would hand every private team to the weaker permission.
#[tokio::test]
async fn the_public_only_caller_sees_only_open_invite_teams() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) = fetch_both_stable(&client, &f.public_only, "/api/v4/teams?per_page=200").await;
    assert_eq!(go, rust);
    let listed = ids(&go);
    assert!(listed.contains(&f.open_team), "the open team is public");
    assert!(!listed.contains(&f.private_team));
    assert!(!listed.contains(&f.archived_team));
}

/// The mirror image: `list_private_teams` alone means `AllowOpenInvite = false`, which includes
/// the archived team — being archived is orthogonal to being private.
#[tokio::test]
async fn the_private_only_caller_sees_only_the_non_open_teams() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) =
        fetch_both_stable(&client, &f.private_only, "/api/v4/teams?per_page=200").await;
    assert_eq!(go, rust);
    let listed = ids(&go);
    assert!(listed.contains(&f.private_team));
    assert!(listed.contains(&f.archived_team));
    assert!(!listed.contains(&f.open_team));
}

/// Both permissions filter nothing, so the same caller with both roles sees the union of the two
/// listings above — the branch a reader might "simplify" into one of the single-permission ones.
#[tokio::test]
async fn both_permissions_see_the_union_of_the_two_halves() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) = fetch_both_stable(&client, &f.both, "/api/v4/teams?per_page=200").await;
    assert_eq!(go, rust);
    let listed = ids(&go);
    for id in [&f.open_team, &f.private_team, &f.archived_team] {
        assert!(
            listed.contains(id),
            "{id} must be listed for both permissions"
        );
    }
}

/// Neither permission is the route's **own** 403, with an id no other refusal on this server
/// uses — `SetPermissionError`'s `api.context.permissions.app_error` would be the wrong string
/// and the webapp branches on it.
#[tokio::test]
async fn neither_list_permission_is_the_routes_own_403() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.neither, "/api/v4/teams").await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go = common::assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rs_body,
        "getAllTeams with neither list permission",
    );
    assert_eq!(go["id"], "api.team.get_all_teams.insufficient_permissions");
    assert_eq!(go["detailed_error"], "");
}

/// `exclude_policy_constrained` is gated on the data-retention read, and that refusal is the
/// *ordinary* `SetPermissionError` — a different id from the one above, on the same route.
#[tokio::test]
async fn exclude_policy_constrained_without_the_permission_is_a_plain_permission_error() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &f.both,
        "/api/v4/teams?exclude_policy_constrained=true",
    )
    .await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go = common::assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rs_body,
        "getAllTeams with exclude_policy_constrained and no retention read",
    );
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // The gate is checked before the list matrix, so a caller with *neither* list permission
    // gets this refusal rather than the one above.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &f.neither,
        "/api/v4/teams?exclude_policy_constrained=true",
    )
    .await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go = common::assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rs_body,
        "getAllTeams with exclude_policy_constrained and no permissions at all",
    );
    assert_eq!(
        go["id"], "api.context.permissions.app_error",
        "the retention gate wins over the neither-permission refusal"
    );
}

/// With the retention read the flag is accepted — and the same permission independently turns on
/// `IncludePolicyID`, which changes the query's column list. No team here is governed by a
/// retention policy (the table is empty on Team Edition), so the observable is that both servers
/// still agree byte for byte across all four combinations of the two flags.
#[tokio::test]
async fn the_retention_reader_gets_the_policy_columns_and_the_exclusion() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    for query in [
        "",
        "?exclude_policy_constrained=true",
        "?include_total_count=true",
        "?exclude_policy_constrained=true&include_total_count=true",
    ] {
        let path = format!("/api/v4/teams{query}");
        let (go, rust) = fetch_both_stable(&client, &f.retention, &path).await;
        assert_eq!(go, rust, "{path} differs");
    }
}

/// `SanitizeTeams` runs over every element. A caller who is not a member of a team holds neither
/// `manage_team` nor `invite_user` on it, so **both** `email` and `invite_id` are emptied; the
/// admin, who created these teams and is their team admin, sees both.
#[tokio::test]
async fn sanitize_teams_strips_email_and_invite_id_for_a_non_member() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go, rust) = fetch_both_stable(&client, &f.both, "/api/v4/teams?per_page=200").await;
    assert_eq!(go, rust);
    let listed: serde_json::Value = serde_json::from_slice(&rust).expect("JSON");
    for team in listed.as_array().expect("an array") {
        assert_eq!(team["email"], "", "{} leaked an email", team["id"]);
        assert_eq!(team["invite_id"], "", "{} leaked an invite id", team["id"]);
    }

    let (go, rust) = fetch_both_stable(&client, &f.admin_token, "/api/v4/teams?per_page=200").await;
    assert_eq!(go, rust);
    let listed: serde_json::Value = serde_json::from_slice(&rust).expect("JSON");
    let mine = listed
        .as_array()
        .expect("an array")
        .iter()
        .find(|t| t["id"] == f.open_team.as_str())
        .expect("the admin lists its own team");
    assert_ne!(mine["email"], "", "the team admin keeps the email");
    assert_ne!(mine["invite_id"], "", "and the invite id");
}

/// `for_directory` only reaches the ABAC directory filter, which is dark without an Enterprise
/// Advanced licence — so on this deployment it is accepted and ignored, and the flagged answer
/// is the unflagged one. This is the measurement behind the handler's claim that the filter block
/// can be omitted; it says nothing about a licensed server.
#[tokio::test]
async fn for_directory_is_accepted_and_ignored_with_abac_off() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let (go_plain, rust_plain) =
        fetch_both_stable(&client, &f.public_only, "/api/v4/teams?per_page=200").await;
    assert_eq!(go_plain, rust_plain);
    let (go_flag, rust_flag) = fetch_both_stable(
        &client,
        &f.public_only,
        "/api/v4/teams?per_page=200&for_directory=true",
    )
    .await;
    assert_eq!(go_flag, rust_flag);
    assert_eq!(go_plain, go_flag, "Go itself ignores the flag here");
}

/// `limit * page` overflows `int` on both sides and the database rejects the result, so an
/// absurd page is the same 500 on both servers — carrying `app.team.get_all.app_error`, the
/// *listing* error id rather than the count one, which is what pins the order of the two store
/// calls behind `include_total_count=true`.
#[tokio::test]
async fn an_overflowing_page_is_the_same_500_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    for query in [
        "?page=46116860184273880&per_page=200",
        "?page=46116860184273880&per_page=200&include_total_count=true",
    ] {
        let path = format!("/api/v4/teams{query}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &f.admin_token, &path).await;
        assert_eq!((go_status, rs_status), (500, 500), "{path}");
        let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "app.team.get_all.app_error", "{path}");
    }
}

/// `json.Marshal` + `w.Write`, so no trailing newline on either shape — the call-site rule that
/// separates this route from the channel lists ([D-086]).
#[tokio::test]
async fn neither_body_shape_ends_in_a_newline() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    for query in ["", "?include_total_count=true"] {
        let path = format!("/api/v4/teams{query}");
        let (go, rust) = fetch_both_stable(&client, &f.admin_token, &path).await;
        assert_eq!(go, rust);
        assert_ne!(
            rust.last(),
            Some(&b'\n'),
            "{path} must not end in a newline"
        );
    }
}

/// Registering `GET` on `/api/v4/teams` must not capture `POST` — `partially_migrated`'s method
/// fallback forwards it, and axum would otherwise answer 405 for a route Go still owns. An empty
/// body fails `Team.IsValid` before anything is created, so this asserts the routing without
/// authoring a team.
#[tokio::test]
async fn post_to_the_same_path_is_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = common::client();

    let response = client
        .post(format!("{RUST}/api/v4/teams"))
        .header("Authorization", format!("Bearer {}", f.admin_token))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("the proxy answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "createTeam is not migrated; it must still be forwarded"
    );
    assert_eq!(response.status(), 400, "and Go rejects the empty body");
}
