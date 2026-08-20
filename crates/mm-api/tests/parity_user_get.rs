//! Cross-server parity for `GET /api/v4/users/{user_id}`.
//!
//! The route's three centres of gravity: the **sanitize split** (self gets the lax empty-map
//! `Sanitize`, everyone else the strict `SanitizeProfile`, admins with four flags forced on);
//! the **terms-of-service branch**, which runs only for a self-or-admin viewer and feeds the
//! etag; and the **serve-only-exact-ids rule** — the `/users/*` namespace is full of alphanumeric
//! GET literals (`stats`, `known`, `autocomplete`, `tokens`) that Go routes to their own
//! handlers, so anything that is not a valid 26-character id is forwarded whole.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_user_get
//! ```

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures,
    stack_enabled,
};

/// Plant a `UserTermsOfService` row straight into the shared database — Team Edition cannot
/// author a terms of service over REST, so this is the only way to make the branch's found case
/// reachable. Both servers read the same row; `purge_api_fixtures` clears it with its user.
async fn plant_terms_of_service_row(user_id: &str, tos_id: &str) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return false;
    };
    sqlx::query(
        "INSERT INTO usertermsofservice (userid, termsofserviceid, createat)
         VALUES ($1, $2, 1700000000000)
         ON CONFLICT (userid) DO UPDATE SET termsofserviceid = $2, createat = 1700000000000",
    )
    .bind(user_id)
    .bind(tos_id)
    .execute(&pool)
    .await
    .expect("plants the terms-of-service row");
    true
}

/// D-087, applied to this suite: Go answers user bodies from a cache that a login's `UpdateAt`
/// bump does not refresh, so the two servers *stably* disagree on that one field whenever a
/// fixture user was just created or logged in — and we are the correct side. The field is
/// normalised out of every cross-server byte comparison here, exactly as `parity_users_me` does;
/// everything else, key order and newline included, stays byte-compared.
fn normalise_update_at(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body).into_owned();
    let Some(start) = text.find("\"update_at\":") else {
        return text;
    };
    let value_start = start + "\"update_at\":".len();
    let value_end = text[value_start..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|d| value_start + d)
        .unwrap_or(text.len());
    format!("{}0{}", &text[..value_start], &text[value_end..])
}

/// An admin views another user: `SanitizeProfile(asAdmin)` keeps all four flagged fields, and
/// the body is byte-identical, newline included.
#[tokio::test]
async fn an_admin_view_of_another_user_is_byte_identical() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let target = create_plain_user(&client, &token, &team_id, "usergettarget").await;

    let path = format!("/api/v4/users/{}", target.id);
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_plain_user(&client, &token, &target.id).await;

    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&go_body),
        "the two servers must agree byte for byte, update_at excepted (D-087)"
    );
    assert_eq!(
        rs_body.last(),
        Some(&b'\n'),
        "the encoder's newline ([D-086])"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["id"].as_str(), Some(target.id.as_str()));
    assert_ne!(parsed["email"].as_str(), Some(""), "an admin keeps email");
    assert_eq!(parsed["password"].as_str(), None, "never on the wire");
}

/// A plain user views the admin: the strict `SanitizeProfile` path without the admin override.
/// Email and full name survive only because the privacy defaults are on; the auth fields and
/// notify props do not survive at all.
#[tokio::test]
async fn a_plain_users_view_of_another_user_is_byte_identical() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let viewer = create_plain_user(&client, &token, &team_id, "usergetviewer").await;

    let path = format!("/api/v4/users/{}", common::logged_in_user_id());
    let (go_body, rs_body) = fetch_both_stable(&client, &viewer.token, &path).await;
    delete_plain_user(&client, &token, &viewer.id).await;

    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&go_body),
        "the two servers must agree byte for byte, update_at excepted (D-087)"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_ne!(
        parsed["email"].as_str(),
        Some(""),
        "ShowEmailAddress defaults true, so the flag keeps it"
    );
    assert_eq!(
        parsed.get("notify_props"),
        None,
        "ClearNonProfileFields blanks notify props for a non-admin viewer, and the emptied \
         map is omitted entirely — the field carries omitempty"
    );
}

/// Self through the explicit id takes the lax empty-map path — and must byte-match both servers
/// *and* the `/users/me` alias, which shares the whole tail.
#[tokio::test]
async fn self_by_id_matches_both_servers_and_the_me_alias() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = format!("/api/v4/users/{}", common::logged_in_user_id());
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    let (_, me_body) = fetch_both_stable(&client, &token, "/api/v4/users/me").await;

    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&go_body),
        "the two servers must agree byte for byte, update_at excepted (D-087)"
    );
    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&me_body),
        "the alias and the explicit id are the same handler in Go"
    );
}

/// The 304 flow, **per server**: each server's own ETag round-trips to its own 304 with no
/// body. The two etags are *not* compared across servers — `User.Etag` interpolates
/// `update_at`, and D-087 makes that field stably disagree while Go's user cache is stale, so
/// the etags legitimately differ exactly when the bodies do. A client talking through the
/// Strangler proxy only ever revalidates against whoever gave it the etag, which is this shape.
#[tokio::test]
async fn each_servers_etag_round_trips_to_its_own_304() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!("/api/v4/users/{}", common::logged_in_user_id());

    for base in [GO, RUST] {
        let first = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("answers");
        assert_eq!(first.status().as_u16(), 200, "{base}");
        let etag = first
            .headers()
            .get("etag")
            .expect("a 200 carries the etag")
            .to_str()
            .expect("ASCII")
            .to_owned();

        let revalidated = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", &etag)
            .send()
            .await
            .expect("answers");
        assert_eq!(revalidated.status().as_u16(), 304, "{base}");
        assert_eq!(
            revalidated
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok()),
            Some(etag.as_str()),
            "{base}: the 304 repeats the etag"
        );
        let body = revalidated.bytes().await.expect("body reads");
        assert!(body.is_empty(), "{base}: a 304 has no body");
    }
}

/// The terms-of-service gate, all three cells with one planted row: self sees the fields, an
/// admin sees them, and a third user does not — the branch is self-or-admin, not
/// anyone-with-view-members.
#[tokio::test]
async fn the_terms_of_service_fields_show_only_to_self_or_admin() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let accepted = create_plain_user(&client, &token, &team_id, "usergettos").await;
    let bystander = create_plain_user(&client, &token, &team_id, "usergettosby").await;
    let tos_id = "mmrsutosparity000000000001";
    if !plant_terms_of_service_row(&accepted.id, tos_id).await {
        eprintln!("skipping: DATABASE_URL is needed to plant the ToS row");
        return;
    }

    let path = format!("/api/v4/users/{}", accepted.id);

    // Self: the branch runs, the fields land, and both servers agree byte for byte.
    let (go_body, rs_body) = fetch_both_stable(&client, &accepted.token, &path).await;
    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&go_body),
        "self view must agree (update_at excepted, D-087)"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["terms_of_service_id"].as_str(), Some(tos_id));
    assert_eq!(
        parsed["terms_of_service_create_at"].as_i64(),
        Some(1_700_000_000_000)
    );

    // Admin: same fields, different gate half.
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&go_body),
        "admin view must agree (update_at excepted, D-087)"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["terms_of_service_id"].as_str(), Some(tos_id));

    // A third plain user: visible (view_members), but the branch is gated off, so the fields
    // stay zero — from both servers.
    let (go_body, rs_body) = fetch_both_stable(&client, &bystander.token, &path).await;
    delete_plain_user(&client, &token, &accepted.id).await;
    delete_plain_user(&client, &token, &bystander.id).await;
    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&go_body),
        "bystander view must agree (update_at excepted, D-087)"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed.get("terms_of_service_id"),
        None,
        "the branch is self-or-admin only, and the zero value is omitted (omitempty)"
    );
}

/// A well-formed id that matches nothing is a 404 with `MissingAccountError`'s id.
#[tokio::test]
async fn a_missing_user_is_404_from_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;

    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing user");
    assert_eq!(go["id"].as_str(), Some("app.user.missing_account.const"));
}

/// The crowded-namespace rule: Go's alphanumeric GET literals under `/users/` must keep reaching
/// their own handlers through the forward, and a non-id segment that is *not* a literal must get
/// Go's own answer for it (a 400 from Go's `RequireUserId`).
#[tokio::test]
async fn literal_siblings_and_non_id_segments_are_forwarded_to_gos_own_answers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    // `stats` and `tokens` are real Go handlers; `abc` is Go's own invalid-id 400. All three are
    // alphanumeric, so the charset middleware passes them; the serve-only-exact-ids rule is what
    // forwards them.
    for segment in ["stats", "tokens", "abc"] {
        let path = format!("/api/v4/users/{segment}");
        let ours = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the Rust server answers");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{segment}: not an exact id, so not ours to answer"
        );
        let ours_status = ours.status().as_u16();
        let ours_body = ours.bytes().await.expect("body reads").to_vec();

        let direct = client
            .get(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(direct.status().as_u16(), ours_status, "{segment}");
        let direct_body = direct.bytes().await.expect("body reads").to_vec();
        // `stats` is stable; `autocomplete`-style bodies and error bodies carry request ids, so
        // compare only where deterministic.
        if segment == "stats" || segment == "tokens" {
            assert_eq!(
                String::from_utf8_lossy(&direct_body),
                String::from_utf8_lossy(&ours_body),
                "{segment}"
            );
        } else {
            let ours_json: serde_json::Value = serde_json::from_slice(&ours_body).expect("decodes");
            let direct_json: serde_json::Value =
                serde_json::from_slice(&direct_body).expect("decodes");
            assert_eq!(ours_json["id"], direct_json["id"], "{segment}");
            assert_eq!(ours_status, 400, "{segment}: Go's RequireUserId refuses it");
        }
    }
}

/// [D-150]: a segment outside `[A-Za-z0-9]+` never matches Go's route — the charset middleware
/// forwards it before the handler's id rule even runs.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/users/no-pe";

    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go")
    );
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body reads").to_vec();

    let direct = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status);
    assert_eq!(
        String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
        String::from_utf8_lossy(&body)
    );
}

/// Only GET is ours: a POST to `/users/{id}` falls through the method fallback and gets Go's
/// own answer for an unregistered method.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!("/api/v4/users/{}", common::logged_in_user_id());

    let response = client
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "POST is not migrated, so it must be forwarded"
    );
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body reads").to_vec();

    let direct = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status, "Go's own method answer");
    assert_eq!(
        String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
        String::from_utf8_lossy(&body)
    );
}
