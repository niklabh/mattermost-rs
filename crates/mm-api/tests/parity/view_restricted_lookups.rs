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
