//! Cross-server parity for `GET /api/v4/groups` (`getGroups`) and
//! `GET /api/v4/users/{user_id}/groups` (`getGroupsByUserId`).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::groups
//! ```
//!
//! # A fourth licence error, shared between them
//!
//! Both open with `requireLicense` (api4/handlers.go:237), which returns the **generic**
//! `api.license_error` at **501** with a blank `where`. The three channel gates in
//! `parity/licence_gated_channels.rs` each have an id of their own and two of them answer 403;
//! this pair shares one id and one status. Four gates, four conventions — which is why each is
//! asserted rather than assumed to match its neighbour.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, assert_error_bodies_match_except_known_gaps, client, fetch_both_raw,
    go_minted_token, logged_in_user_id, set_active_licence_id, stack_enabled,
};

const LIST: &str = "/api/v4/groups";

fn by_user(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/groups")
}

/// Both routes, the same id, the same status, for the admin.
#[tokio::test]
async fn both_routes_answer_the_generic_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [LIST.to_owned(), by_user(logged_in_user_id())] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 501, "{p}: `requireLicense` is a 501, not a 403");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            body["id"], "api.license_error",
            "{p}: the generic id, shared by every group route"
        );
        assert!(!rs.ends_with(b"\n"), "{p}: error bodies carry no newline");
    }
}

/// **The licence check precedes `RequireUserId`.** `abc` could never be a user id and still gets
/// the licence error, because the gate is the first statement.
#[tokio::test]
async fn an_invalid_user_id_still_gets_the_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let p = by_user("abc");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 501, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_ne!(
        body["id"], "api.context.invalid_url_param.app_error",
        "{p}: not the id error"
    );
}

/// And before the self-or-admin check: asking about **someone else** is still the licence error,
/// where a licensed server would refuse without `manage_system`.
#[tokio::test]
async fn asking_about_another_user_is_still_the_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let p = by_user("mmrsnosuchuser000000000001");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 501, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
}

/// Query parameters change nothing — the gate runs before any of them is read.
#[tokio::test]
async fn the_query_string_is_not_consulted() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [
        format!("{LIST}?only_syncable_sources=true"),
        format!("{LIST}?page=3&per_page=7&filter_allow_reference=true"),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 501, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    }
}

/// **The boundary.** A valid `Systems.ActiveLicenseId` sends both routes back to the proxy.
#[tokio::test]
async fn a_license_row_hands_both_routes_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let served_by = async |p: &str| {
        client
            .get(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    set_active_licence_id(None).await;
    for p in [LIST.to_owned(), by_user(logged_in_user_id())] {
        assert_eq!(served_by(&p).await.as_deref(), Some("rust"), "{p}");
    }

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let forwarded = [
        served_by(LIST).await,
        served_by(&by_user(logged_in_user_id())).await,
    ];

    set_active_licence_id(None).await;

    assert_eq!(forwarded[0].as_deref(), Some("go"), "{LIST}");
    assert_eq!(
        forwarded[1].as_deref(),
        Some("go"),
        "{}",
        by_user(logged_in_user_id())
    );
}

/// A segment outside the mux charset is Go's own 404, before the handler and therefore before the
/// gate.
#[tokio::test]
async fn a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let p = by_user("not-an-id");
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

/// Registering the `GET`s must not turn the `POST` beside them into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let ours = client
        .post(format!("{RUST}{LIST}"))
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
        "POST {LIST} must be forwarded"
    );
}
