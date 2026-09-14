//! Cross-server parity for `PUT /api/v4/roles/{role_id}/patch`, over the port and over the
//! local socket.
//!
//! ```sh
//! scripts/parity.sh --test parity role_patch
//! ```
//!
//! # Synthetic rows, never built-in ones
//!
//! Every write here lands on a role this suite inserted straight into `Roles` — a custom,
//! scheme-less, non-built-in row with a fresh name per run — because a built-in role written
//! through **our** server is invisible to Go's 30-minute by-name role cache, and the `roles`
//! suite compares every built-in row between the two servers, `update_at` included. The
//! refusals that never reach the store (`system_admin`, `system_guest`) are exercised on the
//! built-in rows, which they do not touch. Held under `ROLE_ROWS` exclusively, as every role
//! writer is.
//!
//! # What a row written by us must look like to Go
//!
//! `GET /roles/{id}` is not cached, so after our `PUT` the row is read back through **both**
//! servers and compared with our `PUT`'s own body: three answers, one row.

use std::time::Duration;

use crate::common;

use common::local_socket::{both_maybe_forwarded, sockets_enabled};
use common::{
    BROADCAST_STREAM, GO, ROLE_ROWS, RUST, SocketProbe,
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    delete_plain_user, fetch_both_raw, fetch_licensed_pair, go_minted_token, licensed, request_raw,
    stack_enabled,
};

/// The two rows a test works on, with everything the REST API cannot produce held constant.
struct Synthetic {
    ids: Vec<String>,
    pool: sqlx::PgPool,
}

/// Insert `count` identical custom roles, each `create_post` only, after purging earlier runs'.
async fn synthetic_roles(count: usize) -> Synthetic {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    sqlx::query("DELETE FROM roles WHERE id LIKE 'mmrspatch%'")
        .execute(&pool)
        .await
        .expect("earlier synthetic rows are cleared");

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_millis();
    let stamp = format!("{stamp:013}");
    let mut ids = Vec::new();
    for n in 0..count {
        let id = format!("mmrspatch{stamp}{n:04}");
        assert_eq!(id.len(), 26);
        sqlx::query(
            "INSERT INTO roles
                (id, name, displayname, description, createat, updateat, deleteat,
                 permissions, schememanaged, builtin, schemeid)
             VALUES
                ($1, $2, 'mmrs patch role', 'written straight into the table',
                 1701355039000, 1701355040000, 0, ' create_post', false, false, NULL)",
        )
        .bind(&id)
        .bind(format!("mmrspatch{stamp}{n}"))
        .execute(&pool)
        .await
        .expect("the synthetic row is written");
        ids.push(id);
    }
    Synthetic { ids, pool }
}

impl Synthetic {
    async fn remove(self) {
        sqlx::query("DELETE FROM roles WHERE id LIKE 'mmrspatch%'")
            .execute(&self.pool)
            .await
            .expect("the synthetic rows are removed");
    }
}

async fn put(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    role_id: &str,
    body: &[u8],
) -> (u16, Vec<u8>, bool) {
    let (status, body, served_by) = request_raw(
        client,
        base,
        reqwest::Method::PUT,
        token,
        &format!("/api/v4/roles/{role_id}/patch"),
        Some(body),
    )
    .await;
    (status, body, served_by.as_deref() == Some("rust"))
}

/// A role body with the per-row fields blanked, so two rows patched the same way compare equal.
fn shape(body: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("not JSON: {e}: {}", String::from_utf8_lossy(body)));
    for key in ["id", "name", "update_at"] {
        value[key] = serde_json::Value::Null;
    }
    value
}

/// The same patch on two identical rows, one through each server: the same body but for the
/// row's own fields, the same row read back through both servers, and the same `role_updated`
/// on each server's socket. Then the order-sensitive no-op, and the `null` body that is a
/// rewrite.
#[tokio::test]
async fn a_patch_writes_the_row_and_announces_it_alike() {
    if !stack_enabled() {
        return;
    }
    let _rows = ROLE_ROWS.write().await;
    let _stream = BROADCAST_STREAM.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let rows = synthetic_roles(2).await;
    let (go_role, rs_role) = (&rows.ids[0], &rows.ids[1]);

    let mut go_probe = SocketProbe::connect(GO, &admin).await;
    let mut rs_probe = SocketProbe::connect(RUST, &admin).await;

    let body = br#"{"permissions":["edit_post","create_post","edit_post"],"unknown":1}"#;
    let (go_status, go_body, _) = put(&client, GO, Some(&admin), go_role, body).await;
    let (rs_status, rs_body, served) = put(&client, RUST, Some(&admin), rs_role, body).await;
    assert!(served, "the patch was forwarded to Go");
    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        200,
        "ours: {}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(shape(&go_body), shape(&rs_body));
    assert!(
        go_body.ends_with(b"\n") && rs_body.ends_with(b"\n"),
        "json.Encoder's newline"
    );
    let written: serde_json::Value = serde_json::from_slice(&rs_body).unwrap();
    assert_eq!(
        written["permissions"],
        serde_json::json!(["create_post", "edit_post"])
    );
    assert!(
        written["update_at"].as_i64().unwrap() > 1701355040000,
        "{written}"
    );
    assert_eq!(written["create_at"], 1_701_355_039_000_i64);
    assert_eq!(written["scheme_id"], serde_json::Value::Null);

    // The row we wrote, through both servers — `Get` by id is not cached on Go.
    let ((go_status, go_read), (rs_status, rs_read)) =
        fetch_both_raw(&client, &admin, &format!("/api/v4/roles/{rs_role}")).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(
        go_read, rs_body,
        "Go reads back something other than what we answered"
    );
    assert_eq!(rs_read, rs_body);

    // One `role_updated` on each server's socket, carrying the role as a JSON string.
    go_probe.collect_for(Duration::from_millis(1500)).await;
    rs_probe.collect_for(Duration::from_millis(1500)).await;
    let go_events = go_probe.events_named("role_updated");
    let rs_events = rs_probe.events_named("role_updated");
    assert_eq!(go_events.len(), 1, "{go_events:?}");
    assert_eq!(rs_events.len(), 1, "{rs_events:?}");
    assert_eq!(go_events[0]["broadcast"], rs_events[0]["broadcast"]);
    let carried = |event: &serde_json::Value| {
        let role = event["data"]["role"]
            .as_str()
            .unwrap_or_else(|| panic!("`role` is a string: {event}"));
        shape(role.as_bytes())
    };
    assert_eq!(carried(&go_events[0]), carried(&rs_events[0]));
    assert_eq!(carried(&rs_events[0]), shape(&rs_body));

    // The stored set, in either order, is a no-op: the handler sorts before the compare, so
    // nothing is written and the same bytes come back both times.
    for body in [
        &br#"{"permissions":["create_post","edit_post"]}"#[..],
        &br#"{"permissions":["edit_post","create_post"]}"#[..],
    ] {
        let (_, go_again, _) = put(&client, GO, Some(&admin), go_role, body).await;
        let (_, rs_again, _) = put(&client, RUST, Some(&admin), rs_role, body).await;
        assert_eq!(go_again, go_body, "Go rewrote the row on a set-equal patch");
        assert_eq!(rs_again, rs_body, "we rewrote the row on a set-equal patch");
    }

    // A different set is a write: a later `update_at`, the new permissions sorted.
    let body = br#"{"permissions":["create_post","edit_post","delete_post"]}"#;
    let (_, go_moved, _) = put(&client, GO, Some(&admin), go_role, body).await;
    let (_, rs_moved, _) = put(&client, RUST, Some(&admin), rs_role, body).await;
    assert_eq!(shape(&go_moved), shape(&rs_moved));
    let moved: serde_json::Value = serde_json::from_slice(&rs_moved).unwrap();
    assert_eq!(
        moved["permissions"],
        serde_json::json!(["create_post", "delete_post", "edit_post"])
    );
    assert!(moved["update_at"].as_i64().unwrap() > written["update_at"].as_i64().unwrap());

    // `null` decodes to a patch with no permissions — the shortcut does not fire, so the row is
    // rewritten with the same permissions and a fresh `update_at`.
    let (go_status, go_null, _) = put(&client, GO, Some(&admin), go_role, b"null").await;
    let (rs_status, rs_null, _) = put(&client, RUST, Some(&admin), rs_role, b"null").await;
    assert_eq!(
        (go_status, rs_status),
        (200, 200),
        "{}",
        String::from_utf8_lossy(&rs_null)
    );
    assert_eq!(shape(&go_null), shape(&rs_null));
    let nulled: serde_json::Value = serde_json::from_slice(&rs_null).unwrap();
    assert_eq!(nulled["permissions"], moved["permissions"]);
    assert!(nulled["update_at"].as_i64().unwrap() >= moved["update_at"].as_i64().unwrap());

    rows.remove().await;
}

/// Every refusal, on both servers. The unlicensed pair covers the bodies that do not decode,
/// the ids that do not resolve, the member's first-gate 403, the unlicensed guest-role 501 and
/// the not-allowed-permission 501. The two-gate distinction and the licensed guest path need a
/// licence, so they run on the licensed pair.
#[tokio::test]
async fn the_refusals_match() {
    if !stack_enabled() {
        return;
    }
    let _rows = ROLE_ROWS.write().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let rows = synthetic_roles(1).await;
    let custom = rows.ids[0].clone();
    let team = create_team(&client, &admin, "rpatch").await;
    let member = create_plain_user(&client, &admin, &team, "rpatch").await;

    let role_id_of = async |base: &str, name: &str| -> String {
        let (status, body, _) = request_raw(
            &client,
            base,
            reqwest::Method::GET,
            Some(&admin),
            &format!("/api/v4/roles/name/{name}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let system_admin = role_id_of(GO, "system_admin").await;
    let system_guest = role_id_of(GO, "system_guest").await;

    let perms = &br#"{"permissions":["create_post"]}"#[..];
    let check = async |token: Option<&str>,
                       role_id: &str,
                       body: &[u8],
                       status: u16,
                       id: &str,
                       context: &str| {
        let (go_status, go_body, _) = put(&client, GO, token, role_id, body).await;
        let (rs_status, rs_body, served) = put(&client, RUST, token, role_id, body).await;
        assert!(served, "{context}: forwarded to Go");
        assert_eq!(
            go_status,
            status,
            "{context}: Go: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            status,
            "{context}: ours: {}",
            String::from_utf8_lossy(&rs_body)
        );
        let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);
        assert_eq!(parsed["id"], id, "{context}");
    };

    check(
        None,
        &custom,
        perms,
        401,
        "api.context.session_expired.app_error",
        "no session",
    )
    .await;
    check(
        Some(&admin),
        &custom,
        b"[1]",
        400,
        "api.context.invalid_body_param.app_error",
        "array",
    )
    .await;
    check(
        Some(&admin),
        &custom,
        b"\"x\"",
        400,
        "api.context.invalid_body_param.app_error",
        "string",
    )
    .await;
    check(
        Some(&admin),
        &custom,
        b"",
        400,
        "api.context.invalid_body_param.app_error",
        "empty",
    )
    .await;
    check(
        Some(&admin),
        &custom,
        br#"{"permissions":"x"}"#,
        400,
        "api.context.invalid_body_param.app_error",
        "wrong type",
    )
    .await;
    check(
        Some(&admin),
        "abc",
        perms,
        400,
        "api.context.invalid_url_param.app_error",
        "short id",
    )
    .await;
    check(
        Some(&admin),
        "mmrspatchnosuchroleatall00",
        perms,
        404,
        "app.role.get.app_error",
        "unknown id",
    )
    .await;

    // The member fails the first gate on both a custom role and a protected one.
    check(
        Some(&member.token),
        &custom,
        perms,
        403,
        "api.context.permissions.app_error",
        "member, custom",
    )
    .await;
    check(
        Some(&member.token),
        &system_admin,
        perms,
        403,
        "api.context.permissions.app_error",
        "member, system_admin",
    )
    .await;

    // Unlicensed: a guest role with permissions in the patch is the 501 before the not-allowed
    // check; a not-allowed permission added to a custom role, and one removed from system_admin.
    check(
        Some(&admin),
        &system_guest,
        perms,
        501,
        "api.roles.patch_roles.license.error",
        "guest, unlicensed",
    )
    .await;
    check(
        Some(&admin),
        &custom,
        br#"{"permissions":["create_post","manage_roles"]}"#,
        501,
        "api.roles.patch_roles.not_allowed_permission.error",
        "adding manage_roles",
    )
    .await;
    check(
        Some(&admin),
        &system_admin,
        br#"{"permissions":["create_post"]}"#,
        501,
        "api.roles.patch_roles.not_allowed_permission.error",
        "removing from system_admin",
    )
    .await;

    // Licensed: the guest role gets past the licence check to the not-allowed one.
    let pair = licensed().await;
    let guest_body: &[u8] = br#"{"permissions":["manage_roles"]}"#;
    let (go_status, go_body, _) = request_raw(
        &client,
        &pair.go,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/roles/{system_guest}/patch"),
        Some(guest_body),
    )
    .await;
    let (rs_status, rs_body, served) = request_raw(
        &client,
        &pair.rust,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/roles/{system_guest}/patch"),
        Some(guest_body),
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(
        (go_status, rs_status),
        (501, 501),
        "{}",
        String::from_utf8_lossy(&go_body)
    );
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "licensed guest");
    assert_eq!(
        parsed["id"],
        "api.roles.patch_roles.not_allowed_permission.error"
    );
    // The licensed pair still reads the guest role alike, untouched.
    let (go, rs) = fetch_licensed_pair(
        &client,
        &pair,
        Some(&admin),
        &format!("/api/v4/roles/{system_guest}"),
    )
    .await;
    assert_eq!(go, rs);

    // The second permission gate, which only an actor with `write_permissions` but not
    // `write_system_roles` can tell apart: `system_manager`. Assigning that role needs a
    // licence, so the promotion and the checks both run on the licensed pair.
    let manager = create_plain_user(&client, &admin, &team, "rpatchm").await;
    let (status, body, _) = request_raw(
        &client,
        &pair.go,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/users/{}/roles", manager.id),
        Some(br#"{"roles":"system_user system_manager"}"#),
    )
    .await;
    assert_eq!(
        status,
        200,
        "promoting the manager: {}",
        String::from_utf8_lossy(&body)
    );
    // Both licensed servers must re-read the manager's new roles from the shared DB.
    let _ = request_raw(
        &client,
        &pair.go,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/caches/invalidate",
        None,
    )
    .await;
    let run_member = role_id_of(&pair.go, "run_member").await;
    let manager_check = async |role_id: &str, body: &[u8], status: u16, id: &str, context: &str| {
        let (go_status, go_body, _) = request_raw(
            &client,
            &pair.go,
            reqwest::Method::PUT,
            Some(&manager.token),
            &format!("/api/v4/roles/{role_id}/patch"),
            Some(body),
        )
        .await;
        let (rs_status, rs_body, served) = request_raw(
            &client,
            &pair.rust,
            reqwest::Method::PUT,
            Some(&manager.token),
            &format!("/api/v4/roles/{role_id}/patch"),
            Some(body),
        )
        .await;
        assert_eq!(served.as_deref(), Some("rust"), "{context}");
        assert_eq!(
            go_status,
            status,
            "{context}: Go: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            status,
            "{context}: ours: {}",
            String::from_utf8_lossy(&rs_body)
        );
        let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);
        assert_eq!(parsed["id"], id, "{context}");
    };
    // Custom role: past the first gate, refused by the second.
    manager_check(
        &custom,
        perms,
        403,
        "api.context.permissions.app_error",
        "manager, custom",
    )
    .await;
    // Protected role: refused by the first gate (manage_system).
    manager_check(
        &system_admin,
        perms,
        403,
        "api.context.permissions.app_error",
        "manager, system_admin",
    )
    .await;
    // Scheme-default role: past both gates, refused by the not-allowed check, writing nothing.
    manager_check(
        &run_member,
        br#"{"permissions":["manage_system"]}"#,
        501,
        "api.roles.patch_roles.not_allowed_permission.error",
        "manager, run_member",
    )
    .await;

    delete_plain_user(&client, &admin, &manager.id).await;
    delete_plain_user(&client, &admin, &member.id).await;
    rows.remove().await;
}

/// The same patch over both sockets, on two identical rows; a body that does not decode; and a
/// role id outside the mux charset, forwarded over the socket.
#[tokio::test]
async fn the_patch_matches_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _rows = ROLE_ROWS.write().await;
    let rows = synthetic_roles(2).await;
    let go_socket = common::local_socket::go_socket().expect("checked by sockets_enabled");
    let rust_socket = common::local_socket::rust_socket().expect("checked by sockets_enabled");

    let put_over = async |socket: &std::path::Path, role_id: &str, body: &'static str| {
        let request = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/api/v4/roles/{role_id}/patch"))
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len().to_string())
            .body(axum::body::Body::from(body))
            .expect("request builds");
        let response = mm_api::local::send_over_unix(socket, request)
            .await
            .unwrap_or_else(|e| panic!("PUT over {}: {e}", socket.display()));
        let status = response.status().as_u16();
        let served = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            == Some("rust");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads")
            .to_vec();
        (status, served, body)
    };

    let body = r#"{"permissions":["delete_post","create_post","delete_post"]}"#;
    let (go_status, _, go_body) = put_over(&go_socket, &rows.ids[0], body).await;
    let (rs_status, served, rs_body) = put_over(&rust_socket, &rows.ids[1], body).await;
    assert!(served, "the socket patch was forwarded to Go");
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(shape(&go_body), shape(&rs_body));
    let written: serde_json::Value = serde_json::from_slice(&rs_body).unwrap();
    assert_eq!(
        written["permissions"],
        serde_json::json!(["create_post", "delete_post"])
    );

    let (go_status, _, go_body) = put_over(&go_socket, &rows.ids[0], "[]").await;
    let (rs_status, _, rs_body) = put_over(&rust_socket, &rows.ids[1], "[]").await;
    assert_eq!((go_status, rs_status), (400, 400));
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "socket array");
    assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");

    let ((go_status, _), (rs_status, _), served_here) =
        both_maybe_forwarded("PUT", "/api/v4/roles/not-an-id/patch").await;
    assert!(
        !served_here,
        "a segment outside the mux charset must be forwarded"
    );
    assert_eq!((go_status, rs_status), (404, 404));

    rows.remove().await;
}
