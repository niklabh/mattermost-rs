//! Cross-server parity for the **user family on the local socket** — `api4/user_local.go`.
//!
//! The reads compare shared rows byte for byte over the two sockets; the writes create a user
//! *per socket* (`mmrsnewuserloc…`, which `purge_api_fixtures`'s `mmrsnewuser%` sweep also
//! recognises) so that each server performs the same operation on an equivalent row rather than
//! a second write on one. Every write test holds `USER_COUNT` for the reason `user_creates`
//! does, and scrubs both accounts hard at the end.
//!
//! What this file is careful to show, because the wrappers could be wrong without it:
//!
//! - **the local session is not an ordinary one** — `?in_channel=<no such channel>` is `[]`,
//!   where the port gives a 403, and the profile is sanitised with the admin flags;
//! - **`me` is nobody** on every route that goes through `RequireUserId`, and *is* a user id
//!   of three bytes on the one that does not (`/uploads`);
//! - **a forward goes over the socket**: the permanent delete and the MFA deactivation are
//!   handed to Go and come back as Go's local answer, not a port 401.

use super::super::common;
use super::super::common::local_socket::{
    assert_forwarded_body_is_gos, both, both_maybe_forwarded, both_with_body, go_socket,
    rust_socket, sockets_enabled,
};

const PASSWORD: &str = "Mmrs-Local-1234";

fn username(tag: &str) -> String {
    format!("mmrsnewuserloc{tag}")
}

fn email(tag: &str) -> String {
    format!("{}@mmrs.invalid", username(tag))
}

/// Remove every trace of an account this suite created, on both servers' behalf.
///
/// Hard, like `user_creates::scrub`, plus the three tables the writes here can add rows to: a
/// `Bots` row from the conversion, a `Status` row from the activation, and any token.
async fn scrub(tag: &str) {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    let name = username(tag);
    for statement in [
        "DELETE FROM useraccesstokens WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM preferences WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM sessions WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM status WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM bots WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM users WHERE username = $1",
    ] {
        let _ = sqlx::query(statement).bind(&name).execute(&pool).await;
    }
}

/// One request with an owned body over one socket.
async fn send(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len().to_string())
        .body(axum::body::Body::from(body.to_owned()))
        .expect("request builds");
    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

/// The same request, with a body that is not `'static`, to both sockets; asserts ours served it.
async fn both_json(method: &str, path: &str, body: &str) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");
    let (go_status, _, go_body) = send(&go, method, path, body).await;
    let (rs_status, rs_headers, rs_body) = send(&rust, method, path, body).await;
    assert_eq!(
        rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{method} {path} was forwarded to Go over the socket"
    );
    ((go_status, go_body), (rs_status, rs_body))
}

/// Ours over our socket, theirs over Go's, each on its **own** path — for a write whose target
/// differs per server. Returns `(go, rust, served_here)`.
async fn each_on_its_own(
    method: &str,
    go_path: &str,
    rs_path: &str,
    body: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>), bool) {
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");
    let (go_status, _, go_body) = send(&go, method, go_path, body).await;
    let (rs_status, rs_headers, rs_body) = send(&rust, method, rs_path, body).await;
    let served_here = rs_headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    ((go_status, go_body), (rs_status, rs_body), served_here)
}

/// A list read that other suites' fixtures can move between the two calls: compare, and retry
/// a few times before calling a difference a difference.
async fn both_stable(path: &str) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let mut last = both("GET", path).await;
    for _ in 0..4 {
        if last.0 == last.1 {
            break;
        }
        last = both("GET", path).await;
    }
    last
}

/// [`both_stable`] for a read sent with a body (`POST /users/ids`). Only for requests that write
/// nothing: it repeats the request until the two servers agree.
async fn both_json_stable(
    method: &str,
    path: &str,
    body: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let mut last = both_json(method, path, body).await;
    for _ in 0..4 {
        if last.0 == last.1 {
            break;
        }
        last = both_json(method, path, body).await;
    }
    last
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("not JSON: {e}: {}", String::from_utf8_lossy(body)))
}

/// A created user's body minus what is minted per account or per clock.
fn comparable(user: &serde_json::Value) -> serde_json::Value {
    let mut map = user.as_object().cloned().unwrap_or_default();
    for volatile in [
        "id",
        "username",
        "email",
        "create_at",
        "update_at",
        "delete_at",
        "last_password_update",
        "auth_data",
    ] {
        map.remove(volatile);
    }
    serde_json::Value::Object(map)
}

/// Create one account through each socket and return `(go_id, rust_id)`, asserting the two
/// creations agree field for field. `email_verified: true` in the body is what shows the
/// `CreateUserAsAdmin` branch: a signup would have it sanitised back to `false`.
async fn create_pair(go_tag: &str, rs_tag: &str) -> (String, String) {
    scrub(go_tag).await;
    scrub(rs_tag).await;
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");
    let body_of = |tag: &str| {
        serde_json::json!({
            "email": email(tag),
            "username": username(tag),
            "password": PASSWORD,
            "nickname": "local",
            "position": "socket",
            "email_verified": true,
        })
        .to_string()
    };
    let (go_status, _, go_body) = send(&go, "POST", "/api/v4/users", &body_of(go_tag)).await;
    let (rs_status, rs_headers, rs_body) =
        send(&rust, "POST", "/api/v4/users", &body_of(rs_tag)).await;
    assert_eq!(
        rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "POST /users was forwarded"
    );
    assert_eq!(
        go_status,
        201,
        "Go creates: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        201,
        "we create: {}",
        String::from_utf8_lossy(&rs_body)
    );
    let (go_user, rs_user) = (json(&go_body), json(&rs_body));
    assert_eq!(
        go_user["email_verified"], true,
        "the local session is an admin to createUser"
    );
    assert_eq!(comparable(&go_user), comparable(&rs_user), "POST /users");
    (
        go_user["id"].as_str().expect("id").to_owned(),
        rs_user["id"].as_str().expect("id").to_owned(),
    )
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// The five single-user reads and their refusals, on the administrator's own row.
///
/// `GET /users/{id}` here is `localGetUser`, not `getUser`: the admin reading *themselves* over
/// the port gets the `Sanitize` self view, and over the socket the `SanitizeProfile(user, true)`
/// stranger-as-admin view — the same bytes on both sockets, which is what is asserted.
#[tokio::test]
async fn the_local_user_reads_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();
    // The login above moved `Users.UpdateAt`; Go's profile cache would serve the old row.
    common::invalidate_go_caches(&client, &token).await;
    let admin_name = common::username_of(&client, &token, admin).await;

    for path in [
        format!("/api/v4/users/{admin}"),
        format!("/api/v4/users/username/{admin_name}"),
        format!("/api/v4/users/email/{}", common::LOGIN_ID),
        format!("/api/v4/users/{admin}/uploads"),
        // No `RequireUserId` on `localGetUploadsForUser`: `me` is a three-byte user id here.
        "/api/v4/users/me/uploads".to_owned(),
        "/api/v4/users/zz/uploads".to_owned(),
    ] {
        // Bracketed: the admin's row is written by other suites (`Users.UpdateAt`), and a Go read
        // before such a write and ours after it differ in `update_at` alone (a sharded run,
        // 2026-09-15). The refusal loop below compares error bodies, which carry no user row.
        let ((go_status, go_body), (rs_status, rs_body)) = both_stable(&path).await;
        assert_eq!(
            go_status,
            200,
            "{path}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            200,
            "{path}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{path}"
        );
    }
    let ((_, go_body), _) = both("GET", "/api/v4/users/me/uploads").await;
    assert_eq!(go_body, b"[]", "nobody's uploads");

    let too_long = "a".repeat(mm_model::user::USER_AUTH_DATA_MAX_LENGTH + 1);
    let at_limit = "a".repeat(mm_model::user::USER_AUTH_DATA_MAX_LENGTH);
    for (path, status, id) in [
        // `me` is the empty string, which is not an id.
        (
            "/api/v4/users/me".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        // A literal Go's local router does not own is a `{user_id}` and fails the same way.
        (
            "/api/v4/users/stats".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "/api/v4/users/aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            404,
            "app.user.missing_account.const",
        ),
        // `RequireUsername` answers the *body*-param id; `SliceUser` is outside `IsValidUsername`.
        (
            "/api/v4/users/username/SliceUser".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "/api/v4/users/username/nobody.here-zz".to_owned(),
            404,
            "app.user.get_by_username.app_error",
        ),
        (
            "/api/v4/users/email/not-an-email".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "/api/v4/users/email/nobody@mmrs.invalid".to_owned(),
            404,
            "app.user.missing_account.const",
        ),
        (
            "/api/v4/users/auth_data?value=".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            format!("/api/v4/users/auth_data?value={too_long}"),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            format!("/api/v4/users/auth_data?value={at_limit}"),
            404,
            "app.user.missing_account.const",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!(go_status, status, "{path}: Go's status");
        assert_eq!(rs_status, go_status, "{path}: ours");
        let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], id, "{path}");
    }
}

/// `localGetUsers`: the served arms byte for byte, its own 400s served here, and the arms it
/// hands to Go coming back as Go's local answer.
#[tokio::test]
async fn the_local_user_list_matches_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    common::invalidate_go_caches(&client, &token).await;
    let (team, channel) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    const NOWHERE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";

    for path in [
        "/api/v4/users?per_page=3".to_owned(),
        format!("/api/v4/users?in_team={team}&per_page=200"),
        format!("/api/v4/users?in_channel={channel}&per_page=200"),
        format!("/api/v4/users?not_in_channel={channel}&in_team={team}&per_page=200"),
        format!("/api/v4/users?not_in_team={team}&per_page=200"),
        // The HTTP handler would 403 here: no channel, no permission. The local function has
        // no gate and the store answers `[]`.
        format!("/api/v4/users?in_channel={NOWHERE}"),
        // Not a parameter of the local function, so this is the whole list.
        "/api/v4/users?in_group=abc&per_page=3".to_owned(),
        // Not a 400 on the socket: the `inactive && active` check is the HTTP handler's.
        "/api/v4/users?inactive=true&active=true&per_page=3".to_owned(),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both_stable(&path).await;
        assert_eq!(
            go_status,
            200,
            "{path}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            200,
            "{path}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{path}"
        );
    }
    let ((_, go_body), _) = both("GET", &format!("/api/v4/users?in_channel={NOWHERE}")).await;
    assert_eq!(go_body, b"[]");

    for (path, parameter) in [
        (format!("/api/v4/users?not_in_channel={channel}"), "team_id"),
        // Accepted by `getUsers`, refused by `localGetUsers`.
        (
            format!("/api/v4/users?sort=admin&in_channel={channel}"),
            "sort",
        ),
        (
            format!("/api/v4/users?sort=display_name&in_group={NOWHERE}"),
            "sort",
        ),
        ("/api/v4/users?sort=status".to_owned(), "sort"),
        ("/api/v4/users?sort=create_at".to_owned(), "sort"),
        (
            format!("/api/v4/users?sort=create_at&in_team={team}&without_team=no"),
            "sort",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!(go_status, 400, "{path}: Go's status");
        assert_eq!(rs_status, 400, "{path}: ours");
        let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"], "api.context.invalid_url_param.app_error",
            "{path}"
        );
        assert!(
            go["message"].as_str().unwrap_or("").contains(parameter),
            "{path}: Go names {parameter}: {}",
            go["message"]
        );
    }

    // The unported arms: handed to Go over the socket, so the answer is Go's local one.
    for path in [
        format!("/api/v4/users?sort=create_at&in_team={team}&per_page=3"),
        format!("/api/v4/users?sort=status&in_channel={channel}&per_page=3"),
        "/api/v4/users?role=system_admin&per_page=3".to_owned(),
        "/api/v4/users?roles=system_admin,nope&per_page=3".to_owned(),
        "/api/v4/users?without_team=true&per_page=3".to_owned(),
        format!("/api/v4/users?not_in_team={team}&group_constrained=true&per_page=3"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body), served_here) =
            both_maybe_forwarded("GET", &path).await;
        assert!(!served_here, "{path} is forwarded");
        assert_eq!(rs_status, go_status, "{path}");
        if go_status == 200 {
            assert_eq!(go_body, rs_body, "{path}");
        } else {
            assert_forwarded_body_is_gos(&go_body, &rs_body, &path);
        }
    }
}

/// `localGetUsersByIds`, and its three 400s.
#[tokio::test]
async fn the_local_users_by_ids_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();
    common::invalidate_go_caches(&client, &token).await;

    let body = format!(r#"["{admin}","aaaaaaaaaaaaaaaaaaaaaaaaaa"]"#);
    let ((go_status, go_body), (rs_status, rs_body)) =
        both_json_stable("POST", "/api/v4/users/ids", &body).await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    assert!(json(&go_body).as_array().is_some_and(|a| a.len() == 1));

    for (path, body, id) in [
        (
            "/api/v4/users/ids",
            "[]",
            "api.context.invalid_body_param.app_error",
        ),
        ("/api/v4/users/ids", "nonsense", "api.payload.parse.error"),
        (
            "/api/v4/users/ids?since=abc",
            r#"["aaaaaaaaaaaaaaaaaaaaaaaaaa"]"#,
            "api.context.invalid_body_param.app_error",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both_with_body("POST", path, body).await;
        assert_eq!(go_status, 400, "{path} {body}");
        assert_eq!(rs_status, 400, "{path} {body}");
        let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(go["id"], id, "{path} {body}");
    }
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Create, update, roles, active, password, the soft delete and the reactivation — one account
/// per socket, every step compared.
#[tokio::test]
async fn a_local_account_lifecycle_matches_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    common::purge_api_fixtures().await;
    let (go_tag, rs_tag) = ("lifego", "lifers");
    let (go_id, rs_id) = create_pair(go_tag, rs_tag).await;

    // `updateUser`: the whole user, `id` matching the path. The e-mail-change password check is
    // the self branch, which the local session cannot take.
    //
    // `timezone` is sent explicitly. Left out, both servers store a null timezone — Go as the
    // JSON text `null`, this port as SQL NULL — and from there the rows diverge under later
    // writes and Go's reads; that is the HTTP `updateUser` port's finding ([D-601]), not this
    // family's, and a present map keeps it out of the comparison.
    let update = |tag: &str, id: &str| {
        serde_json::json!({
            "id": id,
            "email": email(tag),
            "username": username(tag),
            "nickname": "renamed",
            "position": "moved",
            "timezone": {"automaticTimezone": "", "manualTimezone": "", "useAutomaticTimezone": "true"},
            // `props` is sent for the same reason as `timezone`: our HTTP `updateUser` writes an
            // omitted `props`/`timezone` as **SQL NULL**, discarding the `{}`/default the create
            // stored, and Go's row scanner then 500s reading that row — reachable here because
            // the socket lets Go read a row our handler wrote (the forwarded permanent delete).
            // A present value keeps that pre-existing divergence out of this family's comparison;
            // it is [D-601], on `updateUser`.
            "props": {"mmrs_local": "1"},
        })
        .to_string()
    };
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");
    let (go_status, _, go_body) = send(
        &go,
        "PUT",
        &format!("/api/v4/users/{go_id}"),
        &update(go_tag, &go_id),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "PUT",
        &format!("/api/v4/users/{rs_id}"),
        &update(rs_tag, &rs_id),
    )
    .await;
    assert_eq!(
        rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "PUT /users/{{id}} was forwarded"
    );
    assert_eq!(
        go_status,
        200,
        "Go updates: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        200,
        "we update: {}",
        String::from_utf8_lossy(&rs_body)
    );
    let (go_user, rs_user) = (json(&go_body), json(&rs_body));
    assert_eq!(go_user["nickname"], "renamed");
    assert_eq!(
        comparable(&go_user),
        comparable(&rs_user),
        "PUT /users/{{id}}"
    );

    // `me` is a 400 on all three methods of `/users/{user_id}`.
    for method in ["GET", "PUT", "DELETE"] {
        let ((go_status, go_body), (rs_status, rs_body)) = both(method, "/api/v4/users/me").await;
        assert_eq!((go_status, rs_status), (400, 400), "{method} /users/me");
        let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, method);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }

    for (path, body, expect) in [
        ("roles", r#"{"roles":"system_user system_admin"}"#, 200),
        ("roles", r#"{"roles":"not_a_role"}"#, 400),
        ("active", r#"{"active":false}"#, 200),
        ("active", r#"{"active":true}"#, 200),
        ("active", r#"{"active":"no"}"#, 400),
        ("password", r#"{"new_password":"Mmrs-Local-5678"}"#, 200),
        ("password", r#"{"new_password":"short"}"#, 400),
        ("email/verify/member", "", 200),
    ] {
        let method = if path == "email/verify/member" {
            "POST"
        } else {
            "PUT"
        };
        let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
            method,
            &format!("/api/v4/users/{go_id}/{path}"),
            &format!("/api/v4/users/{rs_id}/{path}"),
            body,
        )
        .await;
        assert!(served_here, "{method} /users/{{id}}/{path} {body}");
        assert_eq!(
            go_status,
            expect,
            "{path} {body}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            go_status,
            "{path} {body}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        if go_status != 200 {
            common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        } else if path == "email/verify/member" {
            // The body is the sanitised user, encoder newline included — two accounts, so
            // everything but the minted fields.
            assert!(go_body.ends_with(b"}\n"), "Encode's newline");
            assert!(rs_body.ends_with(b"}\n"), "and ours");
            assert_eq!(
                comparable(&json(&go_body)),
                comparable(&json(&rs_body)),
                "{path}"
            );
        } else {
            assert_eq!(go_body, rs_body, "{path} {body}");
        }
    }

    // The roles write above made both accounts system admins; the reads agree on that, and on
    // the admin-flag sanitisation of a system admin's row.
    let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
        "GET",
        &format!("/api/v4/users/{go_id}"),
        &format!("/api/v4/users/{rs_id}"),
        "",
    )
    .await;
    assert!(served_here);
    assert_eq!((go_status, rs_status), (200, 200));
    let (go_user, rs_user) = (json(&go_body), json(&rs_body));
    assert_eq!(go_user["roles"], "system_user system_admin");
    assert_eq!(
        comparable(&go_user),
        comparable(&rs_user),
        "GET after the writes"
    );

    // `localDeleteUser`: no permission gate, no self guard, the soft arm served here.
    let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
        "DELETE",
        &format!("/api/v4/users/{go_id}"),
        &format!("/api/v4/users/{rs_id}"),
        "",
    )
    .await;
    assert!(served_here, "DELETE /users/{{id}}");
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(
        go_body, rs_body,
        "the soft delete is `{{\"status\":\"OK\"}}`"
    );
    let ((_, go_body), (_, rs_body), _) = each_on_its_own(
        "GET",
        &format!("/api/v4/users/{go_id}"),
        &format!("/api/v4/users/{rs_id}"),
        "",
    )
    .await;
    assert_ne!(json(&go_body)["delete_at"], 0, "Go deactivated its account");
    assert_ne!(json(&rs_body)["delete_at"], 0, "we deactivated ours");

    let ((go_status, _), (rs_status, _)) =
        both("DELETE", "/api/v4/users/aaaaaaaaaaaaaaaaaaaaaaaaaa").await;
    assert_eq!(
        (go_status, rs_status),
        (404, 404),
        "an unknown id is the GetUser 404"
    );

    // The permanent delete: no `EnableAPIUserDeletion` check on the socket (the flag is off on
    // this stack, and the HTTP route refuses), and the erase is Go's — forwarded over the
    // socket after our `GetUser`.
    let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
        "DELETE",
        &format!("/api/v4/users/{go_id}?permanent=true"),
        &format!("/api/v4/users/{rs_id}?permanent=true"),
        "",
    )
    .await;
    assert!(!served_here, "the permanent delete is forwarded");
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(go_body, rs_body);
    let ((go_status, _), (rs_status, _), _) = each_on_its_own(
        "GET",
        &format!("/api/v4/users/{go_id}"),
        &format!("/api/v4/users/{rs_id}"),
        "",
    )
    .await;
    assert_eq!((go_status, rs_status), (404, 404), "both rows are gone");

    scrub(go_tag).await;
    scrub(rs_tag).await;
}

/// The authentication-data, token, guest and bot routes, and the two forwards — the MFA
/// deactivation and the permanent delete — coming back as Go's **local** answers.
#[tokio::test]
async fn the_local_auth_token_and_conversion_routes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    common::purge_api_fixtures().await;
    let (go_tag, rs_tag) = ("authgo", "authrs");
    let (go_id, rs_id) = create_pair(go_tag, rs_tag).await;

    // Tokens: listing is `[]`; minting is refused while `EnableUserAccessTokens` is off, on
    // both, after the description check; revoking an unknown token is the store's 404.
    for (method, go_path, rs_path, body) in [
        (
            "GET",
            format!("/api/v4/users/{go_id}/tokens"),
            format!("/api/v4/users/{rs_id}/tokens"),
            String::new(),
        ),
        (
            "POST",
            format!("/api/v4/users/{go_id}/tokens"),
            format!("/api/v4/users/{rs_id}/tokens"),
            r#"{"description":""}"#.to_owned(),
        ),
        (
            "POST",
            format!("/api/v4/users/{go_id}/tokens"),
            format!("/api/v4/users/{rs_id}/tokens"),
            r#"{"description":"over the socket"}"#.to_owned(),
        ),
        (
            "POST",
            "/api/v4/users/tokens/revoke".to_owned(),
            "/api/v4/users/tokens/revoke".to_owned(),
            r#"{"token_id":"aaaaaaaaaaaaaaaaaaaaaaaaaa"}"#.to_owned(),
        ),
        // `me` is nobody on the token list too.
        (
            "GET",
            "/api/v4/users/me/tokens".to_owned(),
            "/api/v4/users/me/tokens".to_owned(),
            String::new(),
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body), served_here) =
            each_on_its_own(method, &go_path, &rs_path, &body).await;
        assert!(served_here, "{method} {rs_path}");
        assert_eq!(
            rs_status,
            go_status,
            "{method} {rs_path}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        if go_status == 200 {
            assert_eq!(go_body, rs_body, "{method} {rs_path}");
        } else {
            common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &rs_path);
        }
    }

    // Guests: neither account is one, and guest accounts are off on the stack.
    for action in ["promote", "demote"] {
        let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
            "POST",
            &format!("/api/v4/users/{go_id}/{action}"),
            &format!("/api/v4/users/{rs_id}/{action}"),
            "",
        )
        .await;
        assert!(served_here, "{action}");
        assert_ne!(go_status, 200, "{action} is refused for a non-guest");
        assert_eq!(
            rs_status,
            go_status,
            "{action}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, action);
    }

    // The MFA deactivation: the `GetUser` is ours, the two `UPDATE`s and the e-mail are Go's —
    // and Go's *local* handler's, reached over the socket, not a port 401.
    let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
        "PUT",
        &format!("/api/v4/users/{go_id}/mfa"),
        &format!("/api/v4/users/{rs_id}/mfa"),
        r#"{"activate":false}"#,
    )
    .await;
    assert!(!served_here, "the MFA deactivation is forwarded");
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(go_body, rs_body);

    // The conversion: `{user_id}` becomes a bot owned by itself.
    let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
        "POST",
        &format!("/api/v4/users/{go_id}/convert_to_bot"),
        &format!("/api/v4/users/{rs_id}/convert_to_bot"),
        "",
    )
    .await;
    assert!(served_here, "convert_to_bot");
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let (go_bot, rs_bot) = (json(&go_body), json(&rs_body));
    assert_eq!(
        go_bot.as_object().map(|o| o.keys().collect::<Vec<_>>()),
        rs_bot.as_object().map(|o| o.keys().collect::<Vec<_>>()),
        "the same fields on both sides"
    );
    assert_eq!(go_bot["user_id"], go_id.as_str());
    assert_eq!(rs_bot["user_id"], rs_id.as_str());

    // `updateUserAuth`, then the row found again by its auth data through `localGetUserByAuthData`
    // — which has neither the `IsSystemAdmin` gate nor `UserCanSeeOtherUser`. After the
    // conversion, because a federated account is one `convertUserToBot` hands to Go.
    for (tag, id) in [(go_tag, &go_id), (rs_tag, &rs_id)] {
        let body = format!(r#"{{"auth_data":"local-{tag}","auth_service":"ldap"}}"#);
        let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
            "PUT",
            &format!("/api/v4/users/{id}/auth"),
            &format!("/api/v4/users/{id}/auth"),
            &body,
        )
        .await;
        // Both servers write the same row twice; the echo is the submitted `UserAuth`.
        assert!(served_here);
        assert_eq!(
            (go_status, rs_status),
            (200, 200),
            "{}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(go_body, rs_body, "PUT /users/{{id}}/auth");
    }
    let ((go_status, go_body), (rs_status, rs_body), served_here) = each_on_its_own(
        "GET",
        &format!("/api/v4/users/auth_data?value=local-{go_tag}"),
        &format!("/api/v4/users/auth_data?value=local-{rs_tag}"),
        "",
    )
    .await;
    assert!(served_here);
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let (go_user, rs_user) = (json(&go_body), json(&rs_body));
    assert_eq!(go_user["id"], go_id.as_str());
    assert_eq!(rs_user["id"], rs_id.as_str());
    assert_eq!(
        comparable(&go_user),
        comparable(&rs_user),
        "GET /users/auth_data"
    );

    // No deactivation and no permanent delete of these two: `BotFromUser` makes a converted
    // account a bot **owned by itself**, and Go's `userDeactivated` → `disableUserBots` →
    // `UpdateBotActive` → `UpdateActive` then recurses on it without end (measured 2026-09-14:
    // the request never returned and `Users.UpdateAt` kept moving until the `Bots` row was
    // marked deleted by hand). This port's `owns_bots` forward hands exactly that request to
    // Go. The rows are scrubbed by SQL instead; the permanent delete is exercised on the
    // lifecycle pair.
    scrub(go_tag).await;
    scrub(rs_tag).await;
}

/// The refusals of the two routes that only send e-mail or need a directory, and the routes
/// beside this family that must still answer after its registrations.
#[tokio::test]
async fn the_local_user_family_neighbours_and_refusals_match() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();
    common::invalidate_go_caches(&client, &token).await;

    // `sendPasswordReset`'s two served outcomes: a missing address, and an unmatched one.
    for (body, status) in [
        (r#"{"email":""}"#, 400),
        (r#"{"email":"nobody@mmrs.invalid"}"#, 200),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            both_with_body("POST", "/api/v4/users/password/reset/send", body).await;
        assert_eq!((go_status, rs_status), (status, status), "{body}");
        if status == 200 {
            assert_eq!(go_body, rs_body, "{body}");
        } else {
            common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, body);
        }
    }

    // `migrateAuthToLDAP` / `migrateAuthToSaml`: served up to the first refusal, forwarded past
    // it, exactly as on the port — either way the two answers agree.
    for (path, body) in [
        ("/api/v4/users/migrate_auth/ldap", "{}"),
        (
            "/api/v4/users/migrate_auth/ldap",
            r#"{"from":"email","match_field":"email"}"#,
        ),
        ("/api/v4/users/migrate_auth/saml", "{}"),
        (
            "/api/v4/users/migrate_auth/saml",
            r#"{"from":"email","matches":{},"auto":true}"#,
        ),
    ] {
        let go = go_socket().expect("checked");
        let rust = rust_socket().expect("checked");
        let (go_status, _, go_body) = send(&go, "POST", path, body).await;
        let (rs_status, rs_headers, rs_body) = send(&rust, "POST", path, body).await;
        let served_here = rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            == Some("rust");
        assert_eq!(
            rs_status,
            go_status,
            "{path} {body}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_ne!(go_status, 200, "{path} {body} is refused on this stack");
        if served_here {
            common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        } else {
            assert_forwarded_body_is_gos(&go_body, &rs_body, path);
        }
    }

    // A literal Go's local router owns for POST only: the GET falls to the method fallback and
    // Go's `{user_id}` route answers it with the 400 our own `{user_id}` route would also give.
    let ((go_status, go_body), (rs_status, rs_body), served_here) =
        both_maybe_forwarded("GET", "/api/v4/users/ids").await;
    assert!(!served_here, "GET /users/ids is the method fallback's");
    assert_eq!((go_status, rs_status), (400, 400));
    assert_forwarded_body_is_gos(&go_body, &rs_body, "GET /users/ids");

    // The neighbours: `status_local.go`'s read one segment deeper, still served, and the
    // TCP router still demanding a session for the family.
    let path = format!("/api/v4/users/{admin}/status");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!((go_status, rs_status), (200, 200), "{path}");
    assert_eq!(json(&go_body)["user_id"], json(&rs_body)["user_id"]);
    let ((go_status, _), (rs_status, _)) = both("GET", "/api/v4/users/me/status").await;
    assert_eq!((go_status, rs_status), (400, 400));

    let response = client
        .get(format!("{}/api/v4/users/{admin}", common::RUST))
        .send()
        .await
        .expect("the TCP server answers");
    assert_eq!(
        response.status().as_u16(),
        401,
        "the local session must not leak onto the port"
    );
}
