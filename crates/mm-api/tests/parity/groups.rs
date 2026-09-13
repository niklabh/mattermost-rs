//! Cross-server parity for every `GET` in `api4/group.go` — ten routes that answer one thing.
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
//!
//! # Ten routes, one answer, and the interesting part is which requests reach it
//!
//! `requireLicense` is the **first statement** of all ten handlers, so on an unlicensed server the
//! bodies are indistinguishable and a suite that only compared them would prove nothing about
//! routing. What it *can* prove is the boundary: which paths reach the gate at all
//! (`every_group_read_answers_the_same_licence_error`), which fall to gorilla's mux 404 instead
//! (`a_syncable_type_outside_the_alternation_is_forwarded` — `teams|channels` is an alternation of
//! literals, not a class), and that a licence hands every one of them back to Go.

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

/// Registering these methods must not turn the ones beside them into our 405.
///
/// **`POST /api/v4/groups` moved out of this test on 2026-09-12**: `createGroup` is now served
/// here too, and `parity::group_writes` owns it. What is left is the methods gorilla registers on
/// neither — a `PUT` and a `PATCH` on the collection — which must still reach Go for its own
/// answer rather than axum's 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for method in [reqwest::Method::PUT, reqwest::Method::PATCH] {
        let ours = client
            .request(method.clone(), format!("{RUST}{LIST}"))
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
            "{method} {LIST} must be forwarded"
        );
    }
}

/// Every `GET` in the file, each reaching the gate, each answering the same 501.
///
/// The list is spelled out rather than generated: a route that stopped being registered would
/// silently drop out of a generated one and the count below would still pass.
fn every_group_read(user_id: &str, id: &str) -> Vec<String> {
    vec![
        LIST.to_owned(),
        by_user(user_id),
        format!("/api/v4/groups/{id}"),
        format!("/api/v4/groups/{id}/members"),
        format!("/api/v4/groups/{id}/stats"),
        format!("/api/v4/groups/{id}/teams"),
        format!("/api/v4/groups/{id}/channels"),
        format!("/api/v4/groups/{id}/teams/{id}"),
        format!("/api/v4/groups/{id}/channels/{id}"),
        format!("/api/v4/channels/{id}/groups"),
        format!("/api/v4/teams/{id}/groups"),
        format!("/api/v4/teams/{id}/groups_by_channels"),
    ]
}

const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// All twelve paths, and each is served by **us** rather than forwarded — without that check the
/// comparison would be Go against Go and would pass whatever the router did.
#[tokio::test]
async fn every_group_read_answers_the_same_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let paths = every_group_read(logged_in_user_id(), NOWHERE);
    assert_eq!(paths.len(), 12, "ten handlers, twelve reachable paths");

    for p in &paths {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, p).await;
        assert_eq!(go_status, 501, "{p}: `requireLicense` is a 501");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, p);
        assert_eq!(body["id"], "api.license_error", "{p}");
        assert!(!rs.ends_with(b"\n"), "{p}: error bodies carry no newline");

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
            Some("rust"),
            "{p} was forwarded"
        );
    }
}

/// **`{syncable_type:teams|channels}` is an alternation of literals.** A third value never matched
/// gorilla, so it is Go's mux 404 — forwarded, not answered with the licence error. This is the
/// one place in the family where two paths that look alike give different statuses.
#[tokio::test]
async fn a_syncable_type_outside_the_alternation_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [
        format!("/api/v4/groups/{NOWHERE}/team"),
        format!("/api/v4/groups/{NOWHERE}/Teams"),
        format!("/api/v4/groups/{NOWHERE}/users"),
        format!("/api/v4/groups/{NOWHERE}/users/{NOWHERE}"),
    ] {
        let ours = client
            .get(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        let served_by = ours
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let status = ours.status().as_u16();
        let body = ours.bytes().await.expect("reads").to_vec();

        assert_eq!(served_by.as_deref(), Some("go"), "{p} must be forwarded");
        assert_eq!(status, 404, "{p}: gorilla's NotFoundHandler");

        let theirs = client
            .get(format!("{}{p}", common::GO))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(theirs.status().as_u16(), status, "{p}");
        assert_eq!(
            String::from_utf8_lossy(&theirs.bytes().await.expect("reads")),
            String::from_utf8_lossy(&body),
            "{p}: a forwarded body is Go's own"
        );
    }
}

/// The literal siblings win over `{syncable_type}` — `members` and `stats` are their own routes in
/// gorilla and must not be read as syncable types here. Both answer the gate; neither 404s.
#[tokio::test]
async fn members_and_stats_are_literals_not_syncable_types() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [
        format!("/api/v4/groups/{NOWHERE}/members"),
        format!("/api/v4/groups/{NOWHERE}/stats"),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 501, "{p}: a route, not a mux 404");
        assert_eq!(rs_status, go_status, "{p}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    }
}

/// The boundary, for the eight added routes: a licence row hands every one back to the proxy.
#[tokio::test]
async fn a_license_row_hands_every_group_read_back_to_go() {
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

    let paths = every_group_read(logged_in_user_id(), NOWHERE);

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let mut forwarded = Vec::new();
    for p in &paths {
        forwarded.push((p.clone(), served_by(p).await));
    }
    // Restored before any assertion, so a failure cannot leave the row set for another suite.
    set_active_licence_id(None).await;

    for (p, answer) in &forwarded {
        assert_eq!(answer.as_deref(), Some("go"), "{p} must be forwarded");
    }

    for p in &paths {
        assert_eq!(served_by(p).await.as_deref(), Some("rust"), "{p}");
    }
}
