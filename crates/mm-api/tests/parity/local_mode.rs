//! Cross-server parity for the **local-mode** API: the same routes over two unix sockets.
//!
//! Every other module in this suite compares two TCP ports. This one compares
//! `reference/.build/mmroot-N/local.socket` (the Go server's, `ServiceSettings
//! .LocalModeSocketLocation`) against `mmrs.socket` (mm-api's own), which is what makes the
//! 171-pair local API measurable at all — before this there was no oracle for it, because
//! `scripts/go-server.sh` shipped with `MM_SERVICESETTINGS_ENABLELOCALMODE=false`.
//!
//! # Why there is no reqwest here
//!
//! It has no unix-socket transport. The client is `mm_api::local::send_over_unix` — the same code
//! that serves as mm-api's forward leg to Go, used here as a test client. That is deliberate
//! rather than convenient: a bug in the forward leg would otherwise be invisible to a suite that
//! dialled the sockets some other way.
//!
//! # The authentication model under test
//!
//! There is no token anywhere in this file. A local request carries no credential at all and gets
//! `model.Session{Local: true}`, which is unrestricted — so `GET /api/v4/server_busy`, an
//! `manage_system` route that a plain HTTP caller is refused, answers 200 here. The tests assert
//! that pair of facts together, since either alone is consistent with a broken port.

use super::super::common::{self, stack_enabled};

/// The Go server's local socket, or `None` when the stack has not exported one.
fn go_socket() -> Option<std::path::PathBuf> {
    std::env::var("MMRS_GO_LOCAL_SOCKET").ok().map(Into::into)
}

/// mm-api's local socket.
fn rust_socket() -> Option<std::path::PathBuf> {
    std::env::var("MMRS_LOCAL_SOCKET").ok().map(Into::into)
}

/// True when both sockets are named *and* present.
///
/// Named-but-absent is the ordinary state of a checkout whose `mm-api` predates this work, and it
/// must skip rather than fail — but the skip is logged, because a silently skipped parity test is
/// the failure mode `stack_enabled` exists to avoid.
fn sockets_enabled() -> bool {
    if !stack_enabled() {
        return false;
    }
    match (go_socket(), rust_socket()) {
        (Some(go), Some(rust)) if go.exists() && rust.exists() => true,
        (Some(go), Some(rust)) => {
            eprintln!(
                "skipping: local sockets not both present ({} exists: {}, {} exists: {})",
                go.display(),
                go.exists(),
                rust.display(),
                rust.exists()
            );
            false
        }
        _ => {
            eprintln!("skipping: MMRS_GO_LOCAL_SOCKET / MMRS_LOCAL_SOCKET are not set");
            false
        }
    }
}

/// One request over one socket, with a JSON body.
async fn post_over_socket(
    socket: &std::path::Path,
    path: &str,
    body: &'static str,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len().to_string())
        .body(axum::body::Body::from(body))
        .expect("request builds");

    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("POST {path} over {}: {e}", socket.display()));

    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

/// One request over one socket, returning `(status, headers, body)`.
async fn over_socket(
    socket: &std::path::Path,
    method: &str,
    path: &str,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        // `mmctl` sends a Host; curl's `--unix-socket` sends whatever the URL says. Neither server
        // routes on it, and sending one keeps this an ordinary HTTP/1.1 request.
        .header("Host", "localhost")
        .body(axum::body::Body::empty())
        .expect("request builds");

    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("{} {path} over {}: {e}", method, socket.display()));

    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

/// The same request to both sockets.
async fn both(method: &str, path: &str) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = go_socket().expect("checked by sockets_enabled");
    let rust = rust_socket().expect("checked by sockets_enabled");

    let (go_status, _, go_body) = over_socket(&go, method, path).await;
    let (rust_status, rust_headers, rust_body) = over_socket(&rust, method, path).await;

    assert_eq!(
        rust_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{method} {path} was forwarded to Go over the socket, so this comparison proves nothing \
         about the Rust handler. Is an older mm-api still holding the socket?"
    );

    ((go_status, go_body), (rust_status, rust_body))
}

/// Assert a **forwarded** body is Go's own, byte for byte apart from the per-request id.
///
/// [`common::assert_error_bodies_match_except_known_gaps`] is the wrong instrument here: it pins
/// our `message` to the raw error id, which is the D-092 i18n gap in errors *this server renders*.
/// A forwarded response was rendered by Go, so it carries Go's translated message — and a helper
/// that tolerated both would no longer be able to tell a forward from a local answer.
///
/// `request_id` is minted per request and the two calls are two requests, so it is dropped rather
/// than compared.
fn assert_forwarded_body_is_gos(go_body: &[u8], rust_body: &[u8], context: &str) {
    let strip = |body: &[u8]| {
        let mut value: serde_json::Value = serde_json::from_slice(body)
            .unwrap_or_else(|e| panic!("{context}: body is not JSON: {e}"));
        if let Some(object) = value.as_object_mut() {
            object.remove("request_id");
        }
        value
    };
    assert_eq!(
        strip(go_body),
        strip(rust_body),
        "{context}: a forwarded body must be Go's, message included"
    );
}

/// Serialises the tests that touch the busy flag.
///
/// `ServerBusy` is one global per process — that is what it is in Go too — so a test that marks
/// this server busy and a test that asserts it is idle cannot both run at once. The alternative
/// was a test that passes alone and fails in the suite, which this project already has enough of.
static BUSY_STATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The cheapest possible end-to-end proof that the socket works: `getSystemPing` over both.
///
/// Its HTTP twin has been served for months, so a difference here is the *transport*, not the
/// handler — which is exactly why this is the first route on the socket.
#[tokio::test]
async fn the_local_ping_matches_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let ((go_status, go_body), (rust_status, rust_body)) = both("GET", "/api/v4/system/ping").await;

    assert_eq!(go_status, 200);
    assert_eq!(rust_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rust_body)
    );
}

/// `web.Handler.ServeHTTP` sets the same headers on the socket as on the port.
///
/// The local router is a second `Router` in this process and could easily have been built without
/// the middleware the HTTP one carries — a difference no body comparison can see. `Expires` is the
/// discriminating one: Go sets it on `GET` only.
#[tokio::test]
async fn the_socket_carries_gos_security_headers() {
    if !sockets_enabled() {
        return;
    }
    // It issues a DELETE on both sockets, which is a write to the busy flag.
    let _guard = BUSY_STATE.lock().await;
    let go = go_socket().expect("present");
    let rust = rust_socket().expect("present");

    for (method, path, expect_expires) in [
        ("GET", "/api/v4/server_busy", true),
        ("DELETE", "/api/v4/server_busy", false),
    ] {
        let (_, go_headers, _) = over_socket(&go, method, path).await;
        let (_, rust_headers, _) = over_socket(&rust, method, path).await;

        for header in [
            "content-type",
            "referrer-policy",
            "x-content-type-options",
            "permissions-policy",
            "vary",
        ] {
            assert_eq!(
                rust_headers.get(header).map(|v| v.as_bytes()),
                go_headers.get(header).map(|v| v.as_bytes()),
                "{method} {path}: {header}"
            );
        }
        assert_eq!(
            rust_headers.contains_key("expires"),
            expect_expires,
            "{method} {path}: Expires is a GET-only header"
        );
        assert_eq!(
            go_headers.contains_key("expires"),
            expect_expires,
            "{method} {path}: and Go agrees"
        );
    }
}

/// `clearServerBusy`'s body is `{"status":"OK"}`, with no trailing newline, on both sockets.
///
/// A `DELETE` on an idle server is a no-op on either side, so this is safe to run beside
/// everything else — and it is the only comparison that sees `web.ReturnStatusOK` on the local
/// router at all. A body assertion, not a status one: an encoder instead of a bare write adds a
/// newline that nothing else here would notice.
#[tokio::test]
async fn the_clear_body_is_status_ok_on_both_sockets() {
    if !sockets_enabled() {
        return;
    }
    let _guard = BUSY_STATE.lock().await;
    let ((go_status, go_body), (rust_status, rust_body)) =
        both("DELETE", "/api/v4/server_busy").await;

    assert_eq!((go_status, rust_status), (200, 200));
    assert_eq!(String::from_utf8_lossy(&rust_body), r#"{"status":"OK"}"#);
    assert_eq!(go_body, rust_body, "no trailing newline on either side");
}

/// `getAppliedSchemaMigrations` over the socket, where the permission check is satisfied by the
/// local session rather than by a role.
///
/// The list comes from the **shared** database, so the two servers are reading the same rows; a
/// difference is an ordering or a rendering difference, not a data one.
#[tokio::test]
async fn the_local_schema_version_matches() {
    if !sockets_enabled() {
        return;
    }
    let ((go_status, go_body), (rust_status, rust_body)) =
        both("GET", "/api/v4/system/schema/version").await;

    assert_eq!(go_status, 200);
    assert_eq!(rust_status, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&go_body).expect("go json"),
        serde_json::from_slice::<serde_json::Value>(&rust_body).expect("rust json")
    );
    assert_eq!(
        go_body, rust_body,
        "byte for byte, trailing newline included"
    );
}

/// `localGetClientLicense`, both 400s and the unlicensed success.
///
/// The empty-`format` case is the one clients hit by accident and the one whose id they branch
/// on, so it is compared as bytes rather than as a status.
#[tokio::test]
async fn the_local_client_licence_matches_including_its_refusals() {
    if !sockets_enabled() {
        return;
    }
    let ((go_status, go_body), (rust_status, rust_body)) =
        both("GET", "/api/v4/license/client?format=old").await;
    assert_eq!((go_status, rust_status), (200, 200));
    assert_eq!(go_body, rust_body, "{{\"IsLicensed\":\"false\"}}");

    for query in ["", "?format=", "?format=new", "?format=OLD"] {
        let path = format!("/api/v4/license/client{query}");
        let ((go_status, go_body), (rust_status, rust_body)) = both("GET", &path).await;
        assert_eq!(go_status, rust_status, "{path}");
        assert_eq!(go_status, 400, "{path}");
        common::assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
    }
}

/// `GET /api/v4/server_busy` on an idle server, over both sockets.
///
/// The answer is the zero `time.Time` rendered twice — `-62135596800` and
/// `Mon Jan 1 00:00:00 +0000 UTC 0001` — which is the part of this route a port invents rather
/// than measures. Compared as bytes.
///
/// **This test never marks either server busy.** `Busy` gates the `DisableWhenBusy` search routes,
/// and five other modules in this suite exercise them concurrently; a one-second busy window here
/// would make them flake for a reason they could not diagnose. The busy branch is held down by
/// `system::tests` instead, and by [`the_rust_busy_state_is_this_processs_own`] below, which only
/// ever marks *our* process busy.
#[tokio::test]
async fn the_idle_busy_state_matches_byte_for_byte() {
    if !sockets_enabled() {
        return;
    }
    let _guard = BUSY_STATE.lock().await;
    let ((go_status, go_body), (rust_status, rust_body)) = both("GET", "/api/v4/server_busy").await;

    assert_eq!((go_status, rust_status), (200, 200));
    assert_eq!(
        String::from_utf8_lossy(&rust_body),
        r#"{"busy":false,"expires":-62135596800,"expires_ts":"Mon Jan 1 00:00:00 +0000 UTC 0001"}"#
    );
    assert_eq!(go_body, rust_body);
}

/// The three `?seconds=` rejections, over both sockets.
///
/// A rejected `POST` changes no state on either side, so this is safe to run beside everything
/// else — and it is the only comparison that exercises `setServerBusy`'s parameter handling
/// against Go at all.
#[tokio::test]
async fn the_busy_seconds_rejections_match() {
    if !sockets_enabled() {
        return;
    }
    for query in [
        "?seconds=0",
        "?seconds=-1",
        "?seconds=86401",
        "?seconds=abc",
    ] {
        let path = format!("/api/v4/server_busy{query}");
        let ((go_status, go_body), (rust_status, rust_body)) = both("POST", &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rust_status, 400, "{path}");
        common::assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
    }
}

/// The busy state is **in-memory and per process**, which is [D-320].
///
/// Marking *our* server busy through its own socket must be visible to our own `GET` and
/// invisible to Go's. Recording the divergence in a ledger is not the same as proving it, and a
/// future session that wires the two together should see this test fail rather than discover the
/// coupling by accident.
///
/// Only mm-api is marked busy, so no Go search route is affected. Ours is cleared at the end;
/// mm-api serves no `DisableWhenBusy` route today in any case.
#[tokio::test]
async fn the_rust_busy_state_is_this_processs_own() {
    if !sockets_enabled() {
        return;
    }
    let _guard = BUSY_STATE.lock().await;
    let go = go_socket().expect("present");
    let rust = rust_socket().expect("present");

    let (status, _, _) = over_socket(&rust, "POST", "/api/v4/server_busy?seconds=60").await;
    assert_eq!(status, 200);

    let (_, _, ours) = over_socket(&rust, "GET", "/api/v4/server_busy").await;
    let ours: serde_json::Value = serde_json::from_slice(&ours).expect("json");
    assert_eq!(ours["busy"], serde_json::Value::Bool(true));
    assert!(
        ours["expires"].as_i64().unwrap_or_default() > 0,
        "a real expiry, not the zero time"
    );

    let (_, _, theirs) = over_socket(&go, "GET", "/api/v4/server_busy").await;
    let theirs: serde_json::Value = serde_json::from_slice(&theirs).expect("json");
    assert_eq!(
        theirs["busy"],
        serde_json::Value::Bool(false),
        "the Go process cannot see a busy state set in ours — D-320"
    );

    let (status, _, _) = over_socket(&rust, "DELETE", "/api/v4/server_busy").await;
    assert_eq!(status, 200);
    let (_, _, cleared) = over_socket(&rust, "GET", "/api/v4/server_busy").await;
    let cleared: serde_json::Value = serde_json::from_slice(&cleared).expect("json");
    assert_eq!(cleared["busy"], serde_json::Value::Bool(false));
}

/// An unmigrated local route is **forwarded**, not 404'd.
///
/// This is the whole reason the socket has a proxy leg. `GET /api/v4/users/{user_id}` is
/// registered on Go's local router and not on ours, and `me` is not a valid id to a session with
/// no user — so Go answers a 400 about `user_id`, and our socket must return *that*, byte for
/// byte, rather than a 404 of its own.
#[tokio::test]
async fn an_unmigrated_local_route_is_forwarded_to_go() {
    if !sockets_enabled() {
        return;
    }
    let go = go_socket().expect("present");
    let rust = rust_socket().expect("present");

    let (go_status, _, go_body) = over_socket(&go, "GET", "/api/v4/users/me").await;
    let (rust_status, rust_headers, rust_body) =
        over_socket(&rust, "GET", "/api/v4/users/me").await;

    assert_eq!(go_status, 400);
    assert_eq!(rust_status, 400);
    assert!(
        rust_headers.get("x-mmrs-served-by").is_none(),
        "a forwarded response carries Go's headers, not our cutover marker"
    );
    assert_forwarded_body_is_gos(&go_body, &rust_body, "/api/v4/users/me");
}

/// A forwarded **POST with a body** reaches Go intact.
///
/// Every other forward in this file is a bodyless `GET`, under which the hop-by-hop filter is
/// indistinguishable from no filter at all: there is no inbound `Content-Length` to carry, so
/// carrying it changes nothing. A mutation that stopped dropping it survived the whole suite
/// until this existed.
///
/// `cel/check` is chosen because it is an enterprise route on an unlicensed server: it reads the
/// body, refuses with a deterministic 501 and touches nothing. A forwarded write that *worked*
/// would be a fixture this suite has to clean up.
#[tokio::test]
async fn a_forwarded_post_carries_its_body() {
    if !sockets_enabled() {
        return;
    }
    const PATH: &str = "/api/v4/access_control_policies/cel/check";
    const BODY: &str = r#"{"expression":"1 == 1"}"#;

    let (go_status, _, go_body) =
        post_over_socket(&go_socket().expect("present"), PATH, BODY).await;
    let (rust_status, rust_headers, rust_body) =
        post_over_socket(&rust_socket().expect("present"), PATH, BODY).await;

    assert_eq!(go_status, 501, "unlicensed, so the policy engine refuses");
    assert_eq!(rust_status, go_status);
    assert!(
        rust_headers.get("x-mmrs-served-by").is_none(),
        "this route is not migrated; it must arrive as Go's own answer"
    );
    assert_forwarded_body_is_gos(&go_body, &rust_body, PATH);
}

/// A path neither router registers reaches Go's own 404 through the forward leg.
#[tokio::test]
async fn an_unknown_local_path_gets_gos_404() {
    if !sockets_enabled() {
        return;
    }
    let go = go_socket().expect("present");
    let rust = rust_socket().expect("present");
    let (go_status, _, go_body) = over_socket(&go, "GET", "/api/v4/nope").await;
    let (rust_status, _, rust_body) = over_socket(&rust, "GET", "/api/v4/nope").await;

    assert_eq!(go_status, 404);
    assert_eq!(rust_status, 404);
    assert_forwarded_body_is_gos(&go_body, &rust_body, "/api/v4/nope");
}

/// The HTTP router refuses what the socket grants.
///
/// The local session is unrestricted, and the danger of that model is a route that accidentally
/// inherits it over TCP. `GET /api/v4/server_busy` on the **port**, with no token, must be a 401
/// — proving that the unrestricted session is reachable only through the socket.
#[tokio::test]
async fn the_same_route_over_tcp_still_demands_a_session() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    for base in [common::GO, common::RUST] {
        let response = client
            .get(format!("{base}/api/v4/server_busy"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(
            response.status().as_u16(),
            401,
            "{base}: an anonymous HTTP caller gets no local session"
        );
    }
}

/// And an ordinary (non-admin) token is refused by `manage_system`, on both servers.
///
/// The local shortcut bypasses this check entirely; if the port had implemented "local" as
/// "always allow" on the shared handler rather than as a property of the session, this is the
/// test that would catch it.
#[tokio::test]
async fn a_plain_user_is_refused_the_busy_routes_over_tcp() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let admin = common::go_minted_token(&client).await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = common::create_plain_user(&client, &admin, &team, "busy").await;

    for base in [common::GO, common::RUST] {
        let response = client
            .get(format!("{base}/api/v4/server_busy"))
            .header("Authorization", format!("Bearer {}", user.token))
            .send()
            .await
            .expect("reachable");
        assert_eq!(
            response.status().as_u16(),
            403,
            "{base}: manage_system is not granted to an ordinary user"
        );

        // And the *order* of the two checks: a request that is both unauthorised and malformed
        // is refused for the permission, not for the parameter. Checking `?seconds=` first would
        // tell an unauthorised caller whether their value would have been accepted.
        let response = client
            .post(format!("{base}/api/v4/server_busy?seconds=0"))
            .header("Authorization", format!("Bearer {}", user.token))
            .send()
            .await
            .expect("reachable");
        assert_eq!(
            response.status().as_u16(),
            403,
            "{base}: the permission check runs before the parameter is read"
        );
    }

    common::delete_plain_user(&client, &admin, &user.id).await;
}
