//! Cross-server parity for the two message-channel creates asked by a **view-restricted** caller
//! ([D-240]): `POST /api/v4/channels/direct` and `POST /api/v4/channels/group` both run
//! `UserCanSeeOtherUser` before anything is looked up or written.
//!
//! ```sh
//! scripts/parity.sh --test parity view_restricted_creates
//! ```
//!
//! The caller is made a guest by SQL, as `parity::channel_member_removal` makes one: the three
//! role columns. Its token was minted while it was a plain user, so the session still holds
//! `create_direct_channel` and `create_group_channel`; `GetViewUsersRestrictions` reads the
//! **user's** roles, which is the branch under test. A guest lacks `view_members`, so it sees
//! only the members of its own channels.
//!
//! The visibility check runs before the existing-channel lookup, so the two servers can share one
//! fixture: the second server to be asked for a created pair answers the first one's channel,
//! but only after the same check.

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
async fn post(
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

/// A guest may open a DM or group with the members of its channels, and is refused — `403`
/// naming `view_members` — for a user on a team it is not on and in none of its channels. A
/// teammate outside its channels is answered as Go answers it.
#[tokio::test]
async fn a_guest_creates_only_with_users_it_can_see() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "vrc").await;
    let channel = create_channel_typed(&http, &admin, &team, "vrc", "O").await;
    let guest = create_plain_user(&http, &admin, &team, "vrcguest").await;
    let seen = create_plain_user(&http, &admin, &team, "vrcseen").await;
    let seen2 = create_plain_user(&http, &admin, &team, "vrcseen2").await;
    // A teammate outside the guest's channels: visible or not by the team's `view_members`.
    let teammate = create_plain_user(&http, &admin, &team, "vrcmate").await;
    // Somebody on another team and in none of the guest's channels: never visible.
    let elsewhere = create_team(&http, &admin, "vrcaway").await;
    let unseen = create_plain_user(&http, &admin, &elsewhere, "vrcunseen").await;
    // Somebody on that other team who shares a **group channel** with the guest — made by the
    // admin, before the guest is a guest — and so is visible through the channel half alone.
    let grouped = create_plain_user(&http, &admin, &elsewhere, "vrcgrouped").await;
    let (status, _, body) = post(
        &http,
        GO,
        "/api/v4/channels/group",
        &admin,
        &serde_json::json!([guest.id, grouped.id]),
    )
    .await;
    assert_eq!(status, 201, "the admin's group channel: {body}");
    for user in [&guest.id, &seen.id, &seen2.id] {
        add_user_to_channel(&http, &admin, &channel, user).await;
    }
    make_guest(&pool, &guest.id).await;
    invalidate_go_caches(&http, &admin).await;

    // `None`: whatever Go answers — the teammate's visibility is the team role's to decide.
    let cases: [(&str, &str, serde_json::Value, Option<u16>); 6] = [
        (
            "dm seen",
            "/api/v4/channels/direct",
            serde_json::json!([guest.id, seen.id]),
            Some(201),
        ),
        (
            "dm teammate",
            "/api/v4/channels/direct",
            serde_json::json!([guest.id, teammate.id]),
            None,
        ),
        (
            "dm via a shared group",
            "/api/v4/channels/direct",
            serde_json::json!([guest.id, grouped.id]),
            Some(201),
        ),
        (
            "dm unseen",
            "/api/v4/channels/direct",
            serde_json::json!([guest.id, unseen.id]),
            Some(403),
        ),
        (
            "group seen",
            "/api/v4/channels/group",
            serde_json::json!([seen.id, seen2.id]),
            Some(201),
        ),
        (
            "group with one unseen",
            "/api/v4/channels/group",
            serde_json::json!([seen.id, unseen.id]),
            Some(403),
        ),
    ];
    for (case, path, body, want) in cases {
        let (go_status, _, go_body) = post(&http, GO, path, &guest.token, &body).await;
        let (rust_status, served, rust_body) = post(&http, RUST, path, &guest.token, &body).await;
        if let Some(want) = want {
            assert_eq!(go_status, want, "{case}: Go answered {go_body}");
        }
        assert_eq!(rust_status, go_status, "{case}: we answered {rust_body}");
        assert!(
            served,
            "{case}: a restricted caller is served here: {rust_body}"
        );
        if go_status == 403 {
            let error: serde_json::Value = serde_json::from_str(&go_body).expect("an error");
            assert_eq!(error["id"], "api.context.permissions.app_error", "{case}");
            assert_error_bodies_match_except_known_gaps(
                go_body.as_bytes(),
                rust_body.as_bytes(),
                case,
            );
        } else {
            let go_channel: serde_json::Value = serde_json::from_str(&go_body).expect("a channel");
            let rust_channel: serde_json::Value =
                serde_json::from_str(&rust_body).expect("a channel");
            assert_eq!(
                go_channel["id"], rust_channel["id"],
                "{case}: the same channel, found again"
            );
        }
    }

    for user in [
        &guest.id,
        &seen.id,
        &seen2.id,
        &teammate.id,
        &unseen.id,
        &grouped.id,
    ] {
        common::delete_plain_user(&http, &admin, user).await;
    }
}
