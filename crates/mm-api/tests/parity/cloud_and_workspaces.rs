//! Cross-server parity for the twelve `/cloud` routes and the thirteen connected-workspace ones.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity cloud_and_workspaces
//! ```
//!
//! # Three neighbouring families, three shapes
//!
//! `/cloud` refuses with **400** `api.server.cws.needs_enterprise_edition`; `/remotecluster` and
//! `/sharedchannels` refuse with **501** `api.remote_cluster.service_not_enabled.app_error`; the
//! content-flagging family beside them refuses with 501 and a third id. The status is what a
//! client branches on, so the two are asserted separately rather than through one helper.
//!
//! # The orderings are the point
//!
//! `/cloud` consults nothing before its gate. `/remotecluster` checks a permission first — two
//! different ones, and one of them is an *either* — and sometimes an id after that. And **two**
//! shared-channel routes put the gate *before* their permission check, so an unauthorised caller
//! gets 501 rather than 403, the opposite of every other route here.

use crate::common;

use common::{ACTIVE_LICENCE_ROW, GO, RUST, client, go_minted_token, stack_enabled};

const CLOUD_ERROR: &str = "api.server.cws.needs_enterprise_edition";
const WORKSPACES_ERROR: &str = "api.remote_cluster.service_not_enabled.app_error";
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

fn m(s: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(s.as_bytes()).expect("a method")
}

/// The twelve `/cloud` routes.
fn cloud_routes() -> Vec<(reqwest::Method, String)> {
    vec![
        (m("GET"), "/api/v4/cloud/products".into()),
        (m("GET"), "/api/v4/cloud/limits".into()),
        (m("GET"), "/api/v4/cloud/installation".into()),
        (m("GET"), "/api/v4/cloud/check-cws-connection".into()),
        (m("GET"), "/api/v4/cloud/customer".into()),
        (m("PUT"), "/api/v4/cloud/customer".into()),
        (m("PUT"), "/api/v4/cloud/customer/address".into()),
        (m("GET"), "/api/v4/cloud/subscription".into()),
        (m("GET"), "/api/v4/cloud/subscription/invoices".into()),
        (
            m("GET"),
            "/api/v4/cloud/subscription/invoices/inv_123/pdf".into(),
        ),
        (m("POST"), "/api/v4/cloud/validate-business-email".into()),
        (
            m("POST"),
            "/api/v4/cloud/validate-workspace-business-email".into(),
        ),
    ]
}

/// The thirteen connected-workspace routes.
fn workspace_routes() -> Vec<(reqwest::Method, String)> {
    vec![
        (m("GET"), "/api/v4/remotecluster".into()),
        (m("POST"), "/api/v4/remotecluster".into()),
        (m("POST"), "/api/v4/remotecluster/accept_invite".into()),
        (m("GET"), format!("/api/v4/remotecluster/{NOWHERE}")),
        (m("PATCH"), format!("/api/v4/remotecluster/{NOWHERE}")),
        (m("DELETE"), format!("/api/v4/remotecluster/{NOWHERE}")),
        (
            m("POST"),
            format!("/api/v4/remotecluster/{NOWHERE}/generate_invite"),
        ),
        (
            m("GET"),
            format!("/api/v4/remotecluster/{NOWHERE}/sharedchannelremotes"),
        ),
        (
            m("POST"),
            format!("/api/v4/remotecluster/{NOWHERE}/channels/{NOWHERE}/invite"),
        ),
        (
            m("POST"),
            format!("/api/v4/remotecluster/{NOWHERE}/channels/{NOWHERE}/uninvite"),
        ),
        (
            m("GET"),
            format!("/api/v4/sharedchannels/remote_info/{NOWHERE}"),
        ),
        (m("GET"), format!("/api/v4/sharedchannels/{NOWHERE}")),
        (
            m("GET"),
            format!("/api/v4/sharedchannels/{NOWHERE}/remotes"),
        ),
    ]
}

async fn both(
    client: &reqwest::Client,
    token: &str,
    method: &reqwest::Method,
    path: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let call = async |base: &str| {
        let response = client
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .expect("reachable");
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (
            status,
            served_by,
            response.bytes().await.expect("reads").to_vec(),
        )
    };

    let (go_status, _, go) = call(GO).await;
    let (rs_status, served_by, rs) = call(RUST).await;
    assert_eq!(served_by.as_deref(), Some("rust"), "{method} {path}");
    assert_eq!(rs_status, go_status, "{method} {path}");
    let parsed =
        common::assert_error_bodies_match_except_known_gaps(&go, &rs, &format!("{method} {path}"));
    (go_status, parsed)
}

/// **The cloud family is a 400**, not the 501 every neighbour uses.
#[tokio::test]
async fn every_cloud_route_is_a_400_needing_enterprise() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let all = cloud_routes();
    assert_eq!(all.len(), 12);

    for (method, path) in &all {
        let (status, parsed) = both(&client, &token, method, path, b"{}").await;
        assert_eq!(
            status, 400,
            "the cloud gate is a Bad Request, unlike every other refusal in this router: \
             {method} {path}"
        );
        assert_eq!(parsed["id"], CLOUD_ERROR, "{method} {path}");
    }
}

/// **The connected-workspace family is a 501**, and its id names the *service*, not a licence.
#[tokio::test]
async fn every_workspace_route_is_a_501_service_not_enabled() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let all = workspace_routes();
    assert_eq!(all.len(), 13);

    for (method, path) in &all {
        let (status, parsed) = both(&client, &token, method, path, b"{}").await;
        assert_eq!(status, 501, "{method} {path}");
        assert_eq!(parsed["id"], WORKSPACES_ERROR, "{method} {path}");
    }

    // The two families really do differ, so neither loop is asserting one constant.
    assert_ne!(CLOUD_ERROR, WORKSPACES_ERROR);
}

/// **The permission comes before the gate on `/remotecluster`** — a caller without it gets 403,
/// not 501 — and **after** it on two `/sharedchannels` routes, where the same caller gets 501.
///
/// That inversion is the whole reason these thirteen are ported rather than proxied, and it is
/// invisible to any fixture that only asks as an administrator.
#[tokio::test]
async fn the_gate_sits_on_different_sides_of_the_permission_check() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "workspaces").await;
    let plain = common::create_plain_user(&client, &admin, &team, "workspaces").await;

    // Permission first: a plain user is refused before the service is consulted.
    for (method, path) in [
        (m("GET"), "/api/v4/remotecluster".to_owned()),
        (m("POST"), "/api/v4/remotecluster".to_owned()),
        (m("POST"), "/api/v4/remotecluster/accept_invite".to_owned()),
        (m("GET"), format!("/api/v4/remotecluster/{NOWHERE}")),
        (m("PATCH"), format!("/api/v4/remotecluster/{NOWHERE}")),
        (m("DELETE"), format!("/api/v4/remotecluster/{NOWHERE}")),
        (
            m("POST"),
            format!("/api/v4/remotecluster/{NOWHERE}/generate_invite"),
        ),
        (
            m("GET"),
            format!("/api/v4/remotecluster/{NOWHERE}/sharedchannelremotes"),
        ),
        (
            m("POST"),
            format!("/api/v4/remotecluster/{NOWHERE}/channels/{NOWHERE}/invite"),
        ),
        (
            m("POST"),
            format!("/api/v4/remotecluster/{NOWHERE}/channels/{NOWHERE}/uninvite"),
        ),
    ] {
        let (status, parsed) = both(&client, &plain.token, &method, &path, b"{}").await;
        assert_eq!(
            status, 403,
            "the permission check precedes the service gate: {method} {path}"
        );
        assert_eq!(parsed["id"], "api.context.permissions.app_error");
    }

    // Gate first: the **same** user gets the 501 on these two, because the service is consulted
    // before the team or channel permission.
    for path in [
        format!("/api/v4/sharedchannels/{NOWHERE}"),
        format!("/api/v4/sharedchannels/{NOWHERE}/remotes"),
    ] {
        let (status, parsed) = both(&client, &plain.token, &m("GET"), &path, b"").await;
        assert_eq!(
            status, 501,
            "the service gate precedes the permission check here: {path}"
        );
        assert_eq!(parsed["id"], WORKSPACES_ERROR);
    }

    // And `getRemoteClusterInfo` has **no** permission check at all.
    let (status, parsed) = both(
        &client,
        &plain.token,
        &m("GET"),
        &format!("/api/v4/sharedchannels/remote_info/{NOWHERE}"),
        b"",
    )
    .await;
    assert_eq!(status, 501, "any session may ask about a remote cluster");
    assert_eq!(parsed["id"], WORKSPACES_ERROR);

    // The cloud family checks nothing either: a plain user gets the same 400 an admin does.
    for (method, path) in cloud_routes() {
        let (status, parsed) = both(&client, &plain.token, &method, &path, b"{}").await;
        assert_eq!(status, 400, "no permission is checked: {method} {path}");
        assert_eq!(parsed["id"], CLOUD_ERROR);
    }

    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// **`manage_secure_connections` and `manage_shared_channels` are two permissions**, and one route
/// accepts *either*.
///
/// No stock role separates them — `system_admin` holds both and a plain user holds neither — so a
/// mutation swapping one for the other, or collapsing the "either" check into a single-permission
/// one, is invisible to any fixture built from stock roles. Two of them survived the first run.
/// This uses planted single-permission roles, the same technique the schemes suite needed.
#[tokio::test]
async fn the_two_connection_permissions_are_distinguishable() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    common::purge_api_fixtures().await;
    let team = common::create_team(&client, &admin, "connperm").await;

    let reader = async |tag: &str, permission: &str| -> Option<String> {
        let role = common::plant_role(tag, permission).await?;
        let user = common::create_plain_user(&client, &admin, &team, tag).await;
        common::set_user_roles(&user.id, &format!("system_user {role}")).await;
        // A fresh token: `session.Roles` is copied at login and never re-read.
        Some(common::login_plain_user(&client, tag).await)
    };

    let Some(secure) = reader("connsecure", "manage_secure_connections").await else {
        return; // no DATABASE_URL
    };
    let Some(shared) = reader("connshared", "manage_shared_channels").await else {
        return;
    };

    // `createRemoteCluster` wants `manage_secure_connections` **only**.
    let secure_only = "/api/v4/remotecluster";
    let (status, _) = both(&client, &secure, &m("POST"), secure_only, b"{}").await;
    assert_eq!(status, 501, "secure connections admits: POST {secure_only}");
    let (status, parsed) = both(&client, &shared, &m("POST"), secure_only, b"{}").await;
    assert_eq!(status, 403, "shared channels does not: POST {secure_only}");
    assert_eq!(parsed["id"], "api.context.permissions.app_error");

    // `inviteRemoteClusterToChannel` wants `manage_shared_channels` **only** — the mirror image.
    let shared_only = format!("/api/v4/remotecluster/{NOWHERE}/channels/{NOWHERE}/invite");
    let (status, _) = both(&client, &shared, &m("POST"), &shared_only, b"{}").await;
    assert_eq!(status, 501, "shared channels admits: {shared_only}");
    let (status, _) = both(&client, &secure, &m("POST"), &shared_only, b"{}").await;
    assert_eq!(status, 403, "secure connections does not: {shared_only}");

    // `getRemoteClusters` accepts **either**, so both readers get through — which is what
    // distinguishes `SessionHasPermissionToAny` from a single-permission check.
    for token in [&secure, &shared] {
        let (status, _) = both(&client, token, &m("GET"), "/api/v4/remotecluster", b"").await;
        assert_eq!(
            status, 501,
            "either permission opens the listing, which is why it is `...ToAny`"
        );
    }

    common::delete_plain_user(&client, &admin, &secure).await;
    common::delete_plain_user(&client, &admin, &shared).await;
}

/// **The two `Require*Id` conventions in adjacent files.**
///
/// `RequireRemoteId` tests emptiness alone, so a malformed remote id reaches the service gate and
/// answers 501; `RequireTeamId` and `RequireChannelId` are `IsValidId` and answer 400 on paths
/// that look the same. The port applied the second convention to four routes that use the first,
/// and this is what caught it.
#[tokio::test]
async fn the_id_checks_are_where_go_puts_them() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // **`RequireRemoteId` is not `IsValidId`.** It tests emptiness alone (web/context.go:745),
    // so a five-letter word passes it and the service gate answers — while `RequireTeamId` and
    // `RequireChannelId` on the two `/sharedchannels` routes really do validate. Two conventions
    // in adjacent files, and the port applied the wrong one to four routes before this test.
    for (method, path) in [
        (m("PATCH"), "/api/v4/remotecluster/short".to_owned()),
        (m("DELETE"), "/api/v4/remotecluster/short".to_owned()),
        (m("GET"), "/api/v4/remotecluster/short".to_owned()),
        (
            m("POST"),
            "/api/v4/remotecluster/short/generate_invite".to_owned(),
        ),
        (
            m("GET"),
            "/api/v4/remotecluster/short/sharedchannelremotes".to_owned(),
        ),
        (
            m("GET"),
            "/api/v4/sharedchannels/remote_info/short".to_owned(),
        ),
    ] {
        let (status, parsed) = both(&client, &token, &method, &path, b"{}").await;
        assert_eq!(
            status, 501,
            "a remote id is only checked for emptiness, which the router cannot produce: \
             {method} {path}"
        );
        assert_eq!(parsed["id"], WORKSPACES_ERROR, "{method} {path}");
    }

    // The two that **do** validate, on paths that look identical.
    for path in [
        "/api/v4/sharedchannels/short".to_owned(),
        "/api/v4/sharedchannels/short/remotes".to_owned(),
    ] {
        let (status, parsed) = both(&client, &token, &m("GET"), &path, b"").await;
        assert_eq!(
            status, 400,
            "`RequireTeamId`/`RequireChannelId` are `IsValidId`: {path}"
        );
        assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
    }
}

/// A licence hands all twenty-five back to Go.
#[tokio::test]
async fn a_licence_row_hands_every_route_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let served_by = async |method: &reqwest::Method, path: &str| -> Option<String> {
        client
            .request(method.clone(), format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(b"{}".to_vec())
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    let all: Vec<_> = cloud_routes()
        .into_iter()
        .chain(workspace_routes())
        .collect();
    assert_eq!(all.len(), 25);

    common::set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let mut forwarded = Vec::new();
    for (method, path) in &all {
        forwarded.push((format!("{method} {path}"), served_by(method, path).await));
    }

    common::set_active_licence_id(None).await;
    let mut cleared = Vec::new();
    for (method, path) in &all {
        cleared.push((format!("{method} {path}"), served_by(method, path).await));
    }

    for (route, served) in &forwarded {
        assert_eq!(
            served.as_deref(),
            Some("go"),
            "licensed, {route} must be forwarded — neither the cloud interface nor the remote \
             cluster service exists on this side"
        );
    }
    for (route, served) in &cleared {
        assert_eq!(
            served.as_deref(),
            Some("rust"),
            "and it comes back: {route}"
        );
    }
}

/// The routes in these files that are deliberately **not** migrated must still be forwarded.
#[tokio::test]
async fn the_unmigrated_neighbours_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let served_by = async |method: reqwest::Method, path: &str| -> Option<String> {
        client
            .request(method, format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    for (method, path, why) in [
        (
            m("GET"),
            "/api/v4/cloud/preview/modal_data",
            "no cloud gate at all",
        ),
        (
            m("POST"),
            "/api/v4/cloud/webhook",
            "a different auth wrapper",
        ),
        (
            m("POST"),
            "/api/v4/remotecluster/ping",
            "RemoteClusterTokenRequired",
        ),
        (
            m("POST"),
            "/api/v4/remotecluster/msg",
            "RemoteClusterTokenRequired",
        ),
        (
            m("POST"),
            "/api/v4/remotecluster/confirm_invite",
            "RemoteClusterTokenRequired",
        ),
        (
            m("GET"),
            "/api/v4/sharedchannels/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/can_dm/zzzzzzzzzzzzzzzzzzzzzzzzzy",
            "an ordinary read that answers 200",
        ),
        (
            m("DELETE"),
            "/api/v4/cloud/limits",
            "an unregistered method",
        ),
        (m("PUT"), "/api/v4/remotecluster", "an unregistered method"),
    ] {
        assert_eq!(
            served_by(method.clone(), path).await.as_deref(),
            Some("go"),
            "{method} {path} must be forwarded — {why}"
        );
    }
}
