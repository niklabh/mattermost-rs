//! Cross-server parity for the three single-user lookups asked by a **view-restricted** caller:
//! `GET /api/v4/users/{user_id}`, `/users/username/{username}` and `/users/email/{email}`.
//!
//! ```sh
//! scripts/parity.sh --test parity view_restricted_lookups
//! ```
//!
//! The caller is made a guest by SQL, as `parity::view_restricted_creates` makes one. A guest
//! lacks `view_members`, so `GetViewUsersRestrictions` is non-nil and it sees only the members of
//! its own channels. The cases that discriminate:
//!
//! * **a missing target** — the guest gets the `view_members` 403 on all three routes, where an
//!   unrestricted caller gets the lookup's 404. `getUser` reaches the 403 through
//!   `UserCanSeeOtherUser` *before* the fetch; the other two through the fetch-failure branch
//!   that re-asks for the restrictions.
//! * **a user in none of the guest's channels** — the 403 after a successful fetch.
//! * **a channel mate** and **self** — served, body for body.
//!
//! # `GET /users/stats` for a restricted caller
//!
//! Go's count joins the permitted teams and channels **without `DISTINCT`**, so a user counts once
//! per (team, channel) pair. A stock `team_user` holds no `view_members`, so a guest's team list
//! is empty; the teams half is reached by giving the caller's team memberships a synthetic custom
//! role — fresh name per run, so neither server has it cached — that grants only `view_members`.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, fixture_pool, go_minted_token,
    invalidate_go_caches, stack_enabled,
};

/// `POST /teams/{team_id}/members` through Go, as the admin.
async fn join_team(http: &reqwest::Client, admin: &str, team_id: &str, user_id: &str) {
    let response = http
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({"team_id": team_id, "user_id": user_id}))
        .send()
        .await
        .expect("Go reachable");
    assert_eq!(response.status().as_u16(), 201, "{user_id} joins {team_id}");
}

async fn make_guest(pool: &sqlx::PgPool, user_id: &str) {
    for statement in [
        "UPDATE users SET roles = 'system_guest' WHERE id = $1",
        "UPDATE teammembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
        "UPDATE channelmembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("the guest fixture is written");
    }
}

/// `(status, served here, body)`.
async fn get(http: &reqwest::Client, base: &str, path: &str, token: &str) -> (u16, bool, String) {
    let response = http
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
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

/// `(username, email)` of a user, read by the admin.
async fn identity(http: &reqwest::Client, admin: &str, user_id: &str) -> (String, String) {
    let (status, _, body) = get(http, GO, &format!("/api/v4/users/{user_id}"), admin).await;
    assert_eq!(status, 200, "the admin reads {user_id}: {body}");
    let user: serde_json::Value = serde_json::from_str(&body).expect("a user");
    (
        user["username"].as_str().expect("a username").to_owned(),
        user["email"].as_str().expect("an email").to_owned(),
    )
}

/// The three paths that name one user.
fn paths(id: &str, username: &str, email: &str) -> [(&'static str, String); 3] {
    [
        ("by id", format!("/api/v4/users/{id}")),
        ("by username", format!("/api/v4/users/username/{username}")),
        ("by email", format!("/api/v4/users/email/{email}")),
    ]
}

async fn assert_same_answer(
    http: &reqwest::Client,
    case: &str,
    path: &str,
    token: &str,
    want: u16,
) {
    let (go_status, _, go_body) = get(http, GO, path, token).await;
    let (rust_status, served, rust_body) = get(http, RUST, path, token).await;
    assert_eq!(go_status, want, "{case}: Go answered {go_body}");
    assert_eq!(rust_status, go_status, "{case}: we answered {rust_body}");
    assert!(served, "{case}: served here: {rust_body}");
    if go_status == 200 {
        let go_user: serde_json::Value = serde_json::from_str(&go_body).expect("a user");
        let rust_user: serde_json::Value = serde_json::from_str(&rust_body).expect("a user");
        assert_eq!(
            go_user, rust_user,
            "{case}: the same user, sanitised the same"
        );
    } else {
        assert_error_bodies_match_except_known_gaps(go_body.as_bytes(), rust_body.as_bytes(), case);
    }
}

#[tokio::test]
async fn a_guest_looks_up_only_users_it_can_see() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "vrl").await;
    let channel = create_channel_typed(&http, &admin, &team, "vrl", "O").await;
    let guest = create_plain_user(&http, &admin, &team, "vrlguest").await;
    let seen = create_plain_user(&http, &admin, &team, "vrlseen").await;
    // Unrestricted: the contrast that proves the missing-target 403 is the restriction's.
    let plain = create_plain_user(&http, &admin, &team, "vrlplain").await;
    let elsewhere = create_team(&http, &admin, "vrlaway").await;
    let unseen = create_plain_user(&http, &admin, &elsewhere, "vrlunseen").await;
    for user in [&guest.id, &seen.id] {
        add_user_to_channel(&http, &admin, &channel, user).await;
    }
    make_guest(&pool, &guest.id).await;
    invalidate_go_caches(&http, &admin).await;

    let (guest_name, guest_email) = identity(&http, &admin, &guest.id).await;
    let (seen_name, seen_email) = identity(&http, &admin, &seen.id).await;
    let (unseen_name, unseen_email) = identity(&http, &admin, &unseen.id).await;
    // Valid in every charset, and nobody's.
    let missing = (
        "vrlmissing0000000000000000",
        "vrlmissing",
        "vrlmissing@mmrs.invalid",
    );

    let targets = [
        (
            "self",
            guest.id.as_str(),
            guest_name.as_str(),
            guest_email.as_str(),
            200,
        ),
        ("channel mate", &seen.id, &seen_name, &seen_email, 200),
        ("unseen", &unseen.id, &unseen_name, &unseen_email, 403),
        ("missing", missing.0, missing.1, missing.2, 403),
    ];
    for (target, id, username, email, want) in targets {
        for (route, path) in paths(id, username, email) {
            let case = format!("guest, {target}, {route}");
            assert_same_answer(&http, &case, &path, &guest.token, want).await;
        }
    }

    // The unrestricted caller's missing target is the lookup's own 404 on every route.
    for (route, path) in paths(missing.0, missing.1, missing.2) {
        let case = format!("plain user, missing, {route}");
        assert_same_answer(&http, &case, &path, &plain.token, 404).await;
    }

    for user in [&guest.id, &seen.id, &plain.id, &unseen.id] {
        common::delete_plain_user(&http, &admin, user).await;
    }
}

/// `total_users_count` from both servers for one caller.
async fn both_counts(http: &reqwest::Client, token: &str, case: &str) -> (i64, i64) {
    let mut counts = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = get(http, base, "/api/v4/users/stats", token).await;
        assert_eq!(status, 200, "{case} at {base}: {body}");
        if base == RUST {
            assert!(served, "{case}: served here: {body}");
        }
        let stats: serde_json::Value = serde_json::from_str(&body).expect("stats");
        counts.push(stats["total_users_count"].as_i64().expect("a count"));
    }
    (counts[0], counts[1])
}

#[tokio::test]
async fn a_restricted_callers_user_count_is_gos() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let one = create_team(&http, &admin, "vrs").await;
    let two = create_team(&http, &admin, "vrstwo").await;
    let shared = create_channel_typed(&http, &admin, &one, "vrs", "O").await;
    let other = create_channel_typed(&http, &admin, &one, "vrsother", "O").await;

    // Teams and channels: on both teams, in both channels.
    let both = create_plain_user(&http, &admin, &one, "vrsboth").await;
    // Channels only: a plain guest.
    let guest = create_plain_user(&http, &admin, &one, "vrsguest").await;
    // Teams only: the custom role, and no channel memberships at all.
    let teams_only = create_plain_user(&http, &admin, &one, "vrsteams").await;
    // Neither: a guest on no team and in no channel — Go's `1 = 0`.
    let nobody = create_plain_user(&http, &admin, &one, "vrsnobody").await;
    // The people counted: one on both teams and in both channels, which is the multiplication,
    // and one on one team only.
    let wide = create_plain_user(&http, &admin, &one, "vrswide").await;
    let narrow = create_plain_user(&http, &admin, &two, "vrsnarrow").await;
    // Once on team two: a soft-deleted membership, which `rtm.DeleteAt = 0` must not count.
    let departed = create_plain_user(&http, &admin, &one, "vrsdeparted").await;
    for user in [&both.id, &teams_only.id, &wide.id, &departed.id] {
        join_team(&http, &admin, &two, user).await;
    }
    for user in [&both.id, &guest.id, &wide.id] {
        add_user_to_channel(&http, &admin, &shared, user).await;
        add_user_to_channel(&http, &admin, &other, user).await;
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis();
    let role_id = format!("mmrsvrs{stamp:019}");
    let role_name = format!("mmrs_vrs_viewer_{stamp}");
    sqlx::query(
        "INSERT INTO roles
            (id, name, displayname, description, createat, updateat, deleteat,
             permissions, schememanaged, builtin, schemeid)
         VALUES
            ($1, $2, 'mmrs team viewer', 'written straight into the table',
             1701355039000, 1701355040000, 0, ' view_members', false, false, NULL)",
    )
    .bind(&role_id)
    .bind(&role_name)
    .execute(&pool)
    .await
    .expect("the synthetic role is written");

    for user in [&both.id, &guest.id, &teams_only.id, &nobody.id] {
        make_guest(&pool, user).await;
    }
    for user in [&both.id, &teams_only.id] {
        sqlx::query("UPDATE teammembers SET roles = $2 WHERE userid = $1")
            .bind(user)
            .bind(&role_name)
            .execute(&pool)
            .await
            .expect("the custom team role is granted");
    }
    sqlx::query("DELETE FROM channelmembers WHERE userid = ANY($1)")
        .bind(vec![teams_only.id.clone(), nobody.id.clone()])
        .execute(&pool)
        .await
        .expect("the channel memberships are dropped");
    sqlx::query("DELETE FROM teammembers WHERE userid = $1")
        .bind(&nobody.id)
        .execute(&pool)
        .await
        .expect("the team memberships are dropped");
    sqlx::query(
        "UPDATE teammembers SET deleteat = 1701355041000 WHERE userid = $1 AND teamid = $2",
    )
    .bind(&departed.id)
    .bind(&two)
    .execute(&pool)
    .await
    .expect("the departure is written");
    invalidate_go_caches(&http, &admin).await;

    for (case, token) in [
        ("teams and channels", &both.token),
        ("channels only", &guest.token),
        ("teams only", &teams_only.token),
        ("neither", &nobody.token),
    ] {
        let (go, rust) = both_counts(&http, token, case).await;
        assert_eq!(rust, go, "{case}");
        if case == "neither" {
            assert_eq!(go, 0, "Go's `1 = 0`");
        } else {
            assert!(go > 0, "{case}: a count that proves something");
        }
    }
    // The multiplication is visible: the caller on two teams and in several channels counts more
    // rows than the teams-only caller, whose channel join adds none.
    let (with_channels, _) = both_counts(&http, &both.token, "again").await;
    let (teams_alone, _) = both_counts(&http, &teams_only.token, "again").await;
    assert!(
        with_channels > teams_alone,
        "{with_channels} rows against {teams_alone}: the join multiplies"
    );

    for user in [
        &both.id,
        &guest.id,
        &teams_only.id,
        &nobody.id,
        &wide.id,
        &narrow.id,
        &departed.id,
    ] {
        common::delete_plain_user(&http, &admin, user).await;
    }
    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(&role_id)
        .execute(&pool)
        .await
        .expect("the synthetic role is removed");
}

/// `(status, served here, body)` for a JSON POST.
async fn post_json(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> (u16, bool, String) {
    let response = http
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
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

/// `POST /users/ids` and `POST /users/usernames`: a restricted caller's list keeps only the users
/// it can see, each once, in `Username ASC` — Go reads no cache for a restricted caller, so the
/// order is the query's on both servers and the bodies compare byte for byte.
#[tokio::test]
async fn a_restricted_callers_user_lists_are_gos() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "vrb").await;
    let away = create_team(&http, &admin, "vrbaway").await;
    let first = create_channel_typed(&http, &admin, &team, "vrb", "O").await;
    let second = create_channel_typed(&http, &admin, &team, "vrbsecond", "O").await;

    let guest = create_plain_user(&http, &admin, &team, "vrbguest").await;
    let granted = create_plain_user(&http, &admin, &team, "vrbgranted").await;
    let nobody = create_plain_user(&http, &admin, &team, "vrbnobody").await;
    // In both of the guest's channels: one row per channel before `DISTINCT`.
    let twice = create_plain_user(&http, &admin, &team, "vrbtwice").await;
    // On the team, in neither channel: visible only through a team grant.
    let teammate = create_plain_user(&http, &admin, &team, "vrbmate").await;
    // Elsewhere entirely.
    let stranger = create_plain_user(&http, &admin, &away, "vrbstranger").await;
    for user in [&guest.id, &granted.id, &twice.id] {
        add_user_to_channel(&http, &admin, &first, user).await;
        add_user_to_channel(&http, &admin, &second, user).await;
    }
    // A DM with the stranger each: a channel of the caller's with no team, so the stranger is in
    // a permitted channel and on no permitted team.
    for caller in [&guest.id, &granted.id] {
        let (status, _, body) = post_json(
            &http,
            GO,
            "/api/v4/channels/direct",
            &admin,
            &serde_json::json!([caller, stranger.id]),
        )
        .await;
        assert_eq!(status, 201, "the admin's DM: {body}");
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis();
    let role_id = format!("mmrsvrb{stamp:019}");
    let role_name = format!("mmrs_vrb_viewer_{stamp}");
    sqlx::query(
        "INSERT INTO roles
            (id, name, displayname, description, createat, updateat, deleteat,
             permissions, schememanaged, builtin, schemeid)
         VALUES
            ($1, $2, 'mmrs team viewer', 'written straight into the table',
             1701355039000, 1701355040000, 0, ' view_members', false, false, NULL)",
    )
    .bind(&role_id)
    .bind(&role_name)
    .execute(&pool)
    .await
    .expect("the synthetic role is written");
    for user in [&guest.id, &granted.id, &nobody.id] {
        make_guest(&pool, user).await;
    }
    sqlx::query("UPDATE teammembers SET roles = $2 WHERE userid = $1")
        .bind(&granted.id)
        .bind(&role_name)
        .execute(&pool)
        .await
        .expect("the custom team role is granted");
    for table in ["channelmembers", "teammembers"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE userid = $1"))
            .bind(&nobody.id)
            .execute(&pool)
            .await
            .expect("the memberships are dropped");
    }
    // Out of town-square and off-topic too, which joining the team put it in.
    sqlx::query("DELETE FROM channelmembers WHERE userid = $1")
        .bind(&teammate.id)
        .execute(&pool)
        .await
        .expect("the teammate's channel memberships are dropped");
    invalidate_go_caches(&http, &admin).await;

    let everyone = [&guest, &granted, &nobody, &twice, &teammate, &stranger];
    let ids: Vec<&str> = everyone.iter().map(|u| u.id.as_str()).collect();
    let mut usernames = Vec::new();
    for user in everyone {
        usernames.push(identity(&http, &admin, &user.id).await.0);
    }

    // The ids each caller must get back. With both lists non-empty Go inner-joins **both**, so
    // the team-granted caller sees only users on its team *and* in its channels: not the teammate
    // (no channel), not the stranger (no team) — where `UserCanSeeOtherUser` would take either.
    let expect = [
        (
            "guest",
            &guest.token,
            vec![&guest.id, &granted.id, &twice.id, &stranger.id],
        ),
        (
            "team-granted",
            &granted.token,
            vec![&guest.id, &granted.id, &twice.id],
        ),
        ("nobody", &nobody.token, vec![]),
    ];
    for (caller, token, want) in expect {
        for (route, path, body) in [
            ("by ids", "/api/v4/users/ids", serde_json::json!(ids)),
            (
                "by usernames",
                "/api/v4/users/usernames",
                serde_json::json!(usernames),
            ),
        ] {
            let case = format!("{caller}, {route}");
            let (go_status, _, go_body) = post_json(&http, GO, path, token, &body).await;
            let (rust_status, served, rust_body) = post_json(&http, RUST, path, token, &body).await;
            assert_eq!(go_status, 200, "{case}: Go answered {go_body}");
            assert_eq!(rust_status, 200, "{case}: we answered {rust_body}");
            assert!(served, "{case}: served here");
            let go_users: Vec<serde_json::Value> = serde_json::from_str(&go_body).expect("users");
            let mut got: Vec<&str> = go_users
                .iter()
                .map(|u| u["id"].as_str().expect("an id"))
                .collect();
            got.sort_unstable();
            let mut wanted: Vec<&str> = want.iter().map(|id| id.as_str()).collect();
            wanted.sort_unstable();
            assert_eq!(got, wanted, "{case}: Go's own answer is the fixture's");
            assert_eq!(rust_body, go_body, "{case}: byte for byte");
        }
    }

    for user in everyone {
        common::delete_plain_user(&http, &admin, &user.id).await;
    }
    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(&role_id)
        .execute(&pool)
        .await
        .expect("the synthetic role is removed");
}

/// `POST /users/search` and `GET /users/autocomplete` for a restricted caller: every arm's query
/// keeps only the users it can see (`performSearch` applies the filter last), and both halves of
/// the channel arm are filtered. Every body is compared byte for byte.
#[tokio::test]
async fn a_restricted_callers_searches_are_gos() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "vrq").await;
    let away = create_team(&http, &admin, "vrqaway").await;
    let first = create_channel_typed(&http, &admin, &team, "vrq", "O").await;
    let second = create_channel_typed(&http, &admin, &team, "vrqsecond", "O").await;

    let guest = create_plain_user(&http, &admin, &team, "vrqguest").await;
    let granted = create_plain_user(&http, &admin, &team, "vrqgranted").await;
    let nobody = create_plain_user(&http, &admin, &team, "vrqnobody").await;
    // In the first channel only, and in the second only: the in-channel and out-of-channel halves.
    let inside = create_plain_user(&http, &admin, &team, "vrqinside").await;
    let outside = create_plain_user(&http, &admin, &team, "vrqoutside").await;
    // On the team, in no channel: visible only to a team grant, and then only without channels.
    let teammate = create_plain_user(&http, &admin, &team, "vrqmate").await;
    // On another team, in a DM with each caller.
    let stranger = create_plain_user(&http, &admin, &away, "vrqstranger").await;
    // In the first channel, but gone from the team: the in-channel half's team filter drops it
    // for the team-granted caller, and only its.
    let leaver = create_plain_user(&http, &admin, &team, "vrqleaver").await;
    for user in [&guest.id, &granted.id, &inside.id, &leaver.id] {
        add_user_to_channel(&http, &admin, &first, user).await;
    }
    for user in [&guest.id, &granted.id, &outside.id] {
        add_user_to_channel(&http, &admin, &second, user).await;
    }
    for caller in [&guest.id, &granted.id] {
        let (status, _, body) = post_json(
            &http,
            GO,
            "/api/v4/channels/direct",
            &admin,
            &serde_json::json!([caller, stranger.id]),
        )
        .await;
        assert_eq!(status, 201, "the admin's DM: {body}");
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis();
    let role_id = format!("mmrsvrq{stamp:019}");
    let role_name = format!("mmrs_vrq_viewer_{stamp}");
    sqlx::query(
        "INSERT INTO roles
            (id, name, displayname, description, createat, updateat, deleteat,
             permissions, schememanaged, builtin, schemeid)
         VALUES
            ($1, $2, 'mmrs team viewer', 'written straight into the table',
             1701355039000, 1701355040000, 0, ' view_members view_team', false, false, NULL)",
    )
    .bind(&role_id)
    .bind(&role_name)
    .execute(&pool)
    .await
    .expect("the synthetic role is written");
    for user in [&guest.id, &granted.id, &nobody.id] {
        make_guest(&pool, user).await;
    }
    sqlx::query("UPDATE teammembers SET roles = $2 WHERE userid = $1")
        .bind(&granted.id)
        .bind(&role_name)
        .execute(&pool)
        .await
        .expect("the custom team role is granted");
    for table in ["channelmembers", "teammembers"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE userid = $1"))
            .bind(&nobody.id)
            .execute(&pool)
            .await
            .expect("the memberships are dropped");
    }
    sqlx::query("DELETE FROM channelmembers WHERE userid = $1")
        .bind(&teammate.id)
        .execute(&pool)
        .await
        .expect("the teammate's channel memberships are dropped");
    sqlx::query("UPDATE teammembers SET deleteat = 1701355041000 WHERE userid = $1")
        .bind(&leaver.id)
        .execute(&pool)
        .await
        .expect("the departure is written");
    invalidate_go_caches(&http, &admin).await;

    let searches = [
        serde_json::json!({"term": "vrq"}),
        serde_json::json!({"term": "vrq", "team_id": team}),
    ];
    let autocompletes = [
        "/api/v4/users/autocomplete?name=vrq".to_owned(),
        format!("/api/v4/users/autocomplete?in_team={team}&name=vrq"),
        format!("/api/v4/users/autocomplete?in_team={team}&in_channel={first}&name=vrq"),
    ];
    for (caller, token) in [
        ("guest", &guest.token),
        ("team-granted", &granted.token),
        ("nobody", &nobody.token),
    ] {
        for body in &searches {
            let case = format!("{caller}, search {body}");
            let path = "/api/v4/users/search";
            let (go_status, _, go_body) = post_json(&http, GO, path, token, body).await;
            let (rust_status, served, rust_body) = post_json(&http, RUST, path, token, body).await;
            assert_eq!(
                rust_status, go_status,
                "{case}: Go {go_body}, we {rust_body}"
            );
            assert!(served, "{case}: served here");
            if go_status == 200 {
                assert_eq!(rust_body, go_body, "{case}: byte for byte");
            } else {
                assert_error_bodies_match_except_known_gaps(
                    go_body.as_bytes(),
                    rust_body.as_bytes(),
                    &case,
                );
            }
            if caller == "guest" && go_status == 200 {
                assert!(
                    go_body.contains(&inside.id),
                    "{case}: a channel mate is found"
                );
                assert!(
                    !go_body.contains(&teammate.id),
                    "{case}: the restriction bites"
                );
            }
        }
        for path in &autocompletes {
            let case = format!("{caller}, {path}");
            let (go_status, _, go_body) = get(&http, GO, path, token).await;
            let (rust_status, served, rust_body) = get(&http, RUST, path, token).await;
            assert_eq!(
                rust_status, go_status,
                "{case}: Go {go_body}, we {rust_body}"
            );
            assert!(served, "{case}: served here");
            if go_status == 200 {
                assert_eq!(rust_body, go_body, "{case}: byte for byte");
            } else {
                assert_error_bodies_match_except_known_gaps(
                    go_body.as_bytes(),
                    rust_body.as_bytes(),
                    &case,
                );
            }
        }
    }

    for user in [
        &guest.id,
        &granted.id,
        &nobody.id,
        &inside.id,
        &outside.id,
        &teammate.id,
        &stranger.id,
        &leaver.id,
    ] {
        common::delete_plain_user(&http, &admin, user).await;
    }
    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(&role_id)
        .execute(&pool)
        .await
        .expect("the synthetic role is removed");
}
