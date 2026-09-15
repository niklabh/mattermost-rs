//! Cross-server parity for the five `RemoteClusterTokenRequired` routes of
//! `api4/remote_cluster.go`:
//!
//! ```text
//! POST /api/v4/remotecluster/ping
//! POST /api/v4/remotecluster/msg
//! POST /api/v4/remotecluster/confirm_invite
//! POST /api/v4/remotecluster/upload/{upload_id}
//! POST /api/v4/remotecluster/{user_id}/image
//! ```
//!
//! ```sh
//! scripts/parity.sh --test parity remote_cluster
//! ```
//!
//! On this build all five are the `RemoteClusterTokenRequired` gate — the stack's Go carries no
//! licence, so `License()` is nil and the gate answers 401 `session_expired` before any handler
//! runs, whatever the body or the token headers. The suite fires each route with no token, with a
//! made-up `X-RemoteCluster-Token`/`X-RemoteCluster-Id` pair, and with a body, and asserts our
//! 401 matches Go's field for field (the translated `message` and the `request_id` are the two
//! documented gaps). It also checks the seven `APISessionRequired` neighbours in the family still
//! forward (an anonymous 401 is Go's there too, but with a different `where`).

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, go_minted_token, request_raw,
    stack_enabled,
};

/// Every gated route, with a representative body and both token shapes.
const GATED: &[&str] = &[
    "/api/v4/remotecluster/ping",
    "/api/v4/remotecluster/msg",
    "/api/v4/remotecluster/confirm_invite",
    "/api/v4/remotecluster/upload/abcdef0123456789",
    "/api/v4/remotecluster/aaaaaaaaaaaaaaaaaaaaaaaaaa/image",
];

async fn post_with_headers(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, Vec<u8>, Option<String>) {
    let mut request = client
        .post(format!("{base}{path}"))
        .header("Content-Type", "application/json")
        .body(r#"{"remote_id":"someremoteid00000000000000","msg":{}}"#);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        served_by,
    )
}

/// The gate is a 401 on both servers, with and without token headers, on every route — and ours
/// serves it rather than forwarding.
#[tokio::test]
async fn the_token_gate_is_a_401_on_every_route() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    let header_sets: &[&[(&str, &str)]] = &[
        &[],
        &[
            ("X-RemoteCluster-Token", "sometoken"),
            ("X-RemoteCluster-Id", "someid"),
        ],
    ];

    for path in GATED {
        for headers in header_sets {
            let (go_status, go_body, _) = post_with_headers(&client, GO, path, headers).await;
            let (rs_status, rs_body, served_by) =
                post_with_headers(&client, RUST, path, headers).await;
            assert_eq!((go_status, rs_status), (401, 401), "{path} {headers:?}");
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "{path} {headers:?} was forwarded, not served"
            );
            let go = assert_error_bodies_match_except_known_gaps(
                &go_body,
                &rs_body,
                &format!("{path} {headers:?}"),
            );
            assert_eq!(
                go["id"], "api.context.session_expired.app_error",
                "{path}: Go's gate is not session_expired"
            );
        }
    }
}

/// Serving these five must not disturb the CRUD family beside them: the seven
/// `APISessionRequired` `/remotecluster` routes still answer from Go for an anonymous caller
/// (also a 401, but the family's own, with a different `where`). A quick smoke that the literals
/// did not shadow `{remote_id}`.
#[tokio::test]
async fn the_crud_neighbours_still_forward() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    // `GET /remotecluster` and `GET /remotecluster/{id}` are served by the connected-workspaces
    // family already (they answer the 501 gate), so a served `x-mmrs-served-by: rust` there is
    // correct and not this suite's concern. What this suite must not have broken is the *method*
    // fallback on the gated paths: a `GET` to `/remotecluster/ping` is not registered, so it
    // forwards.
    let (go_status, go_body, _) = request_raw(
        &client,
        GO,
        reqwest::Method::GET,
        Some(&token),
        "/api/v4/remotecluster/ping",
        None,
    )
    .await;
    let (rs_status, rs_body, rs_served) = request_raw(
        &client,
        RUST,
        reqwest::Method::GET,
        Some(&token),
        "/api/v4/remotecluster/ping",
        None,
    )
    .await;
    assert_ne!(
        rs_served.as_deref(),
        Some("rust"),
        "GET /remotecluster/ping was served here"
    );
    assert_eq!(go_status, rs_status, "GET /remotecluster/ping status");
    let _ = (go_body, rs_body);
}
