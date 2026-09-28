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

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, fixture_pool, go_minted_token,
    invalidate_go_caches, stack_enabled,
};

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
