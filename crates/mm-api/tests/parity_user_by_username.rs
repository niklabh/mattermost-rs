//! Cross-server parity for `GET /api/v4/users/username/{username}`.
//!
//! The interesting deltas from `getUser` (whose shared tail this route reuses): the fetch runs
//! **before** the visibility question, the miss wears `app.user.get_by_username.app_error` at
//! both statuses (no `MissingAccountError` here), `RequireUsername` answers the **body**-param
//! 400 for a path segment, and the mux charset is the wider `[A-Za-z0-9\_\-\.]+` — of which the
//! validator accepts only the lowercase half, so `SliceUser` routes and then 400s.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_user_by_username
//! ```

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures,
    stack_enabled,
};

/// D-087 again: `update_at` is normalised out of user-body byte comparisons, as in
/// `parity_user_get` and `parity_users_me`.
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

/// The plain success, and it must match the by-id route byte for byte — same user, same viewer,
/// same shared tail, different lookup.
#[tokio::test]
async fn a_username_lookup_matches_both_servers_and_the_by_id_route() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let target = create_plain_user(&client, &token, &team_id, "unamemain").await;

    let path = "/api/v4/users/username/mmrsplainunamemain";
    let (go_body, rs_body) = fetch_both_stable(&client, &token, path).await;
    let by_id = format!("/api/v4/users/{}", target.id);
    let (_, by_id_body) = fetch_both_stable(&client, &token, &by_id).await;
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
    assert_eq!(
        normalise_update_at(&rs_body),
        normalise_update_at(&by_id_body),
        "the username and id lookups share the whole tail"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["id"].as_str(), Some(target.id.as_str()));
}

/// A miss is a 404 whose id is the **shared** `get_by_username` one — not `MissingAccountError`
/// — so a client cannot correlate a missing id with a missing username by error id.
#[tokio::test]
async fn a_missing_username_is_404_with_the_shared_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/users/username/mmrs-no-such-username";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;

    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing username");
    assert_eq!(
        go["id"].as_str(),
        Some("app.user.get_by_username.app_error"),
        "not MissingAccountError — the by-username miss has its own id (user.go:573)"
    );
}

/// The validator is narrower than the mux: `SliceUser` (uppercase) and `all` (restricted) both
/// route — the charset admits them — and both fail `IsValidUsername` with the **body**-param
/// 400, from both servers.
#[tokio::test]
async fn segments_the_mux_routes_but_the_validator_rejects_are_400_invalid_param() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    for segment in ["SliceUser", "all"] {
        let path = format!("/api/v4/users/username/{segment}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;

        assert_eq!(go_status, 400, "{segment}");
        assert_eq!(rs_status, 400, "{segment}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, segment);
        assert_eq!(
            go["id"].as_str(),
            Some("api.context.invalid_body_param.app_error"),
            "{segment}: SetInvalidParam — the body id, for a path segment"
        );
    }
}

/// A segment outside the username mux class never matches Go's route: the mux 404, reached
/// through the forward. `%20` decodes to a space, which the class rejects.
#[tokio::test]
async fn a_segment_outside_the_mux_class_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    for path in ["/api/v4/users/username/a%20b", "/api/v4/users/username/a@b"] {
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
            Some("go"),
            "{path}: Go would not have routed this, so we must not handle it"
        );
        let status = response.status().as_u16();
        let body = response.bytes().await.expect("body reads").to_vec();
        assert_eq!(status, 404, "{path}");

        let direct = client
            .get(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(direct.status().as_u16(), status, "{path}");
        assert_eq!(
            String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
            String::from_utf8_lossy(&body),
            "{path}"
        );
    }
}

/// Only GET is ours; a POST takes Go's own answer through the method fallback.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/users/username/sliceuser";

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
