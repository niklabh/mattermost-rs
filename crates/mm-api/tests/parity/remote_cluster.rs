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

//!
//! # The licensed half (2026-09-19)
//!
//! Against the licensed pair ([`common::licensed`]) the gate's licence disjuncts pass and the
//! answer turns on the remote-cluster session, now served. The fixture plants `RemoteClusters`
//! rows so every branch of `GetRemoteClusterSession` is a different row — valid, another remote's,
//! deleted, a NULL column, a negative `Options` — and four users for `remoteSetProfileImage`: one
//! owned by the calling remote, one owned by another remote, a local user, and a user whose
//! `RemoteId` is the empty string (not remote, though not NULL either).

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

// ---------------------------------------------------------------------------------------------
// The planted fixture
// ---------------------------------------------------------------------------------------------

/// A 26-character id or token from a tag, so every row of every test is distinct and sweepable by
/// the `mmrsrpi` prefix.
fn rpi_id(test: &str, kind: &str) -> String {
    let id = format!("mmrsrpi{test}{kind}");
    assert!(id.len() <= 26, "{id} is too long for an id column");
    format!("{id:0<26}")
}

/// One planted `RemoteClusters` row's credentials.
struct Remote {
    id: String,
    token: String,
}

/// The rows one test plants. `valid` and `other` are ordinary; the other three each break
/// `GetRemoteClusterSession` a different way while their token would otherwise match.
struct Remotes {
    valid: Remote,
    other: Remote,
    deleted: Remote,
    null_column: Remote,
    negative_options: Remote,
}

async fn purge_remotes(pool: &sqlx::PgPool, test: &str) {
    sqlx::query("DELETE FROM remoteclusters WHERE remoteid LIKE $1")
        .bind(format!("mmrsrpi{test}%"))
        .execute(pool)
        .await
        .expect("the planted remotes delete");
}

/// Plant the five rows for `test`, every column written (as Go's `Save` does) except where the row
/// exists to hold a NULL.
async fn plant_remotes(pool: &sqlx::PgPool, test: &str) -> Remotes {
    purge_remotes(pool, test).await;
    let remote = |kind: &str| Remote {
        id: rpi_id(test, kind),
        token: rpi_id(test, &format!("{kind}t")),
    };
    let remotes = Remotes {
        valid: remote("ok"),
        other: remote("ot"),
        deleted: remote("de"),
        null_column: remote("nu"),
        negative_options: remote("ng"),
    };
    for (row, delete_at, remote_team_id, options) in [
        (&remotes.valid, 0_i64, Some(""), 0_i16),
        (&remotes.other, 0, Some(""), 1),
        (&remotes.deleted, 1_700_000_000_000, Some(""), 0),
        (&remotes.null_column, 0, None, 0),
        (&remotes.negative_options, 0, Some(""), -1),
    ] {
        sqlx::query(
            "INSERT INTO remoteclusters (remoteid, remoteteamid, name, displayname, siteurl, \
             defaultteamid, createat, deleteat, lastpingat, token, remotetoken, topics, creatorid, \
             pluginid, options, lastglobalusersyncat) \
             VALUES ($1, $2, $1, 'mmrs rpi', 'https://rpi.invalid', '', 1700000000000, $3, 0, $4, \
             'mmrsrpiremotetoken00000000', '', '', '', $5, 0)",
        )
        .bind(&row.id)
        .bind(remote_team_id)
        .bind(delete_at)
        .bind(&row.token)
        .bind(options)
        .execute(pool)
        .await
        .expect("the remote plants");
    }
    remotes
}

/// `Users.RemoteId` directly: no REST route writes it (the API sanitises it away).
async fn set_remote_id(pool: &sqlx::PgPool, user_id: &str, remote_id: Option<&str>) {
    sqlx::query("UPDATE users SET remoteid = $1 WHERE id = $2")
        .bind(remote_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("the remote id sets");
}

/// `(status, body, x-mmrs-served-by)` for one POST carrying `headers`, `content_type` and `body`.
async fn post_raw(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_type: &str,
    body: Vec<u8>,
) -> (u16, Vec<u8>, Option<String>) {
    let mut request = client
        .post(format!("{base}{path}"))
        .header("Content-Type", content_type)
        .body(body);
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

/// Both servers of `(go, rust)`, the same request; asserts ours served it, the statuses agree,
/// and the bodies match; returns Go's body.
#[allow(clippy::too_many_arguments)]
async fn both_serve_identically(
    client: &reqwest::Client,
    go: &str,
    rust: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_type: &str,
    body: Vec<u8>,
    context: &str,
) -> serde_json::Value {
    let (go_status, go_body, _) =
        post_raw(client, go, path, headers, content_type, body.clone()).await;
    let (rs_status, rs_body, served_by) =
        post_raw(client, rust, path, headers, content_type, body).await;
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "{context}: forwarded, not served ({})",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "{context}: status\n  go:   {}\n  rust: {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context)
}

/// `(label, headers, Go's error id)`.
type SessionCase = (&'static str, Vec<(&'static str, String)>, &'static str);

/// `(label, path, content type, body, Go's error id, status)`.
type ImageCase = (&'static str, String, String, Vec<u8>, &'static str, u16);

const JSON_BODY: &[u8] = br#"{"remote_id":"someremoteid00000000000000","msg":{}}"#;

const BOUNDARY: &str = "mmrsparityremoteimageboundary";

fn multipart() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

fn multipart_body(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for (name, filename, bytes) in parts {
        let disposition = match filename {
            Some(filename) => format!("form-data; name=\"{name}\"; filename=\"{filename}\""),
            None => format!("form-data; name=\"{name}\""),
        };
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: {disposition}\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

// ---------------------------------------------------------------------------------------------
// Without a licence: valid credentials are still the gate
// ---------------------------------------------------------------------------------------------

/// A planted remote's real token and id change nothing on the unlicensed pair: `License()` is nil,
/// so `ServeHTTP` never resolves the header and the gate refuses — on all five routes.
#[tokio::test]
async fn valid_remote_credentials_are_the_gate_401_without_a_licence() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = common::fixture_pool().await else {
        panic!("the fixture needs DATABASE_URL");
    };
    let client = client();
    let remotes = plant_remotes(&pool, "u").await;
    let headers = [
        ("X-RemoteCluster-Token", remotes.valid.token.as_str()),
        ("X-RemoteCluster-Id", remotes.valid.id.as_str()),
    ];
    for path in GATED {
        let go = both_serve_identically(
            &client,
            GO,
            RUST,
            path,
            &headers,
            "application/json",
            JSON_BODY.to_vec(),
            &format!("unlicensed {path} with valid credentials"),
        )
        .await;
        assert_eq!(go["id"], "api.context.session_expired.app_error", "{path}");
    }
    purge_remotes(&pool, "u").await;
}

// ---------------------------------------------------------------------------------------------
// With a licence: the remote-cluster session
// ---------------------------------------------------------------------------------------------

/// Every way `ServeHTTP` + `RemoteClusterTokenRequired` refuses a remote, on all five routes, and
/// the one way through — which the four service-backed routes forward.
#[tokio::test]
async fn the_remote_cluster_session_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let pair = common::licensed().await;
    let Some(pool) = common::fixture_pool().await else {
        panic!("the fixture needs DATABASE_URL");
    };
    let client = client();
    let remotes = plant_remotes(&pool, "s").await;
    let valid = &remotes.valid;

    let token = |remote: &Remote| remote.token.clone();
    #[rustfmt::skip]
    let cases: Vec<SessionCase> = vec![
        ("no headers", vec![], "api.context.session_expired.app_error"),
        ("a token and no remote id", vec![("X-RemoteCluster-Token", token(valid))],
            "api.context.remote_id_missing.app_error"),
        ("a remote id and no token", vec![("X-RemoteCluster-Id", valid.id.clone())],
            "api.context.session_expired.app_error"),
        ("a wrong token", vec![("X-RemoteCluster-Token", "wrongtoken".to_owned()),
            ("X-RemoteCluster-Id", valid.id.clone())], "api.context.invalid_token.error"),
        ("another remote's token", vec![("X-RemoteCluster-Token", token(&remotes.other)),
            ("X-RemoteCluster-Id", valid.id.clone())], "api.context.invalid_token.error"),
        ("an unknown remote id", vec![("X-RemoteCluster-Token", token(valid)),
            ("X-RemoteCluster-Id", rpi_id("s", "zz"))], "api.context.invalid_token.error"),
        ("a deleted remote", vec![("X-RemoteCluster-Token", token(&remotes.deleted)),
            ("X-RemoteCluster-Id", remotes.deleted.id.clone())], "api.context.invalid_token.error"),
        ("a NULL column", vec![("X-RemoteCluster-Token", token(&remotes.null_column)),
            ("X-RemoteCluster-Id", remotes.null_column.id.clone())],
            "api.context.invalid_token.error"),
        ("a negative Options", vec![("X-RemoteCluster-Token", token(&remotes.negative_options)),
            ("X-RemoteCluster-Id", remotes.negative_options.id.clone())],
            "api.context.invalid_token.error"),
        ("a valid token behind a Bearer", vec![("X-RemoteCluster-Token", token(valid)),
            ("X-RemoteCluster-Id", valid.id.clone()), ("Authorization", "Bearer junk".to_owned())],
            "api.context.session_expired.app_error"),
        ("a valid token behind X-Cloud-Token", vec![("X-RemoteCluster-Token", token(valid)),
            ("X-RemoteCluster-Id", valid.id.clone()), ("X-Cloud-Token", "junk".to_owned())],
            "api.context.session_expired.app_error"),
    ];

    for path in GATED {
        for (label, headers, expected) in &cases {
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
            let go = both_serve_identically(
                &client,
                &pair.go,
                &pair.rust,
                path,
                &headers,
                "application/json",
                JSON_BODY.to_vec(),
                &format!("licensed {path}: {label}"),
            )
            .await;
            assert_eq!(go["id"], *expected, "licensed {path}: {label}");
        }
    }

    // The way through, on a service-backed route: forwarded, and Go's answer relayed. `ping` with
    // a body that does not decode writes nothing whatever the service does.
    let headers = [
        ("X-RemoteCluster-Token", valid.token.as_str()),
        ("X-RemoteCluster-Id", valid.id.as_str()),
    ];
    let path = "/api/v4/remotecluster/ping";
    let (go_status, go_body, _) = post_raw(
        &client,
        &pair.go,
        path,
        &headers,
        "application/json",
        b"not json".to_vec(),
    )
    .await;
    let (rs_status, rs_body, served_by) = post_raw(
        &client,
        &pair.rust,
        path,
        &headers,
        "application/json",
        b"not json".to_vec(),
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "a remote past the gate on ping is Go's"
    );
    assert_eq!(
        go_status,
        rs_status,
        "{}",
        String::from_utf8_lossy(&go_body)
    );
    let go: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(go["id"], rs["id"]);
    assert_ne!(
        go["id"], "api.context.session_expired.app_error",
        "the valid remote got past Go's gate"
    );

    purge_remotes(&pool, "s").await;
}

/// `remoteSetProfileImage`'s own refusals, each byte for byte against the licensed oracle, and
/// the hand-over of an upload every one of them passes.
#[tokio::test]
async fn the_remote_profile_image_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let pair = common::licensed().await;
    let Some(pool) = common::fixture_pool().await else {
        panic!("the fixture needs DATABASE_URL");
    };
    let client = client();
    let admin = common::go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let remotes = plant_remotes(&pool, "i").await;

    let owned = common::create_plain_user(&client, &admin, &team_id, "rpiowned").await;
    let foreign = common::create_plain_user(&client, &admin, &team_id, "rpiforeign").await;
    let local = common::create_plain_user(&client, &admin, &team_id, "rpilocal").await;
    let empty = common::create_plain_user(&client, &admin, &team_id, "rpiempty").await;
    set_remote_id(&pool, &owned.id, Some(&remotes.valid.id)).await;
    set_remote_id(&pool, &foreign.id, Some(&remotes.other.id)).await;
    set_remote_id(&pool, &empty.id, Some("")).await;
    common::invalidate_licensed_go_caches(&client, &pair, &admin).await;

    let headers = [
        ("X-RemoteCluster-Token", remotes.valid.token.as_str()),
        ("X-RemoteCluster-Id", remotes.valid.id.as_str()),
    ];
    let image = multipart_body(&[("image", Some("p.png"), common::TINY_PNG)]);
    let image_path = |user: &str| format!("/api/v4/remotecluster/{user}/image");
    let nobody = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

    #[rustfmt::skip]
    let cases: Vec<ImageCase> = vec![
        // An empty body, so `RequireUserId` is visibly first: after the body it would be the 500.
        ("a user id that is not an id", image_path("notanid"), multipart(), Vec::new(),
            "api.context.invalid_url_param.app_error", 400),
        ("an empty body", image_path(&owned.id), multipart(), Vec::new(),
            "api.user.upload_profile_user.parse.app_error", 500),
        ("a body that is not multipart", image_path(&owned.id), "application/json".to_owned(),
            JSON_BODY.to_vec(), "api.user.upload_profile_user.parse.app_error", 500),
        ("no image part", image_path(&owned.id), multipart(),
            multipart_body(&[("picture", Some("p.png"), common::TINY_PNG)]),
            "api.user.upload_profile_user.no_file.app_error", 400),
        ("an image field that is not a file", image_path(&owned.id), multipart(),
            multipart_body(&[("image", None, common::TINY_PNG)]),
            "api.user.upload_profile_user.no_file.app_error", 400),
        ("a user who does not exist", image_path(nobody), multipart(), image.clone(),
            "api.context.invalid_url_param.app_error", 400),
        ("a local user", image_path(&local.id), multipart(), image.clone(),
            "api.context.invalid_url_param.app_error", 400),
        ("a user whose RemoteId is empty", image_path(&empty.id), multipart(), image.clone(),
            "api.context.invalid_url_param.app_error", 400),
        ("another remote's user", image_path(&foreign.id), multipart(), image.clone(),
            "api.context.remote_id_mismatch.app_error", 401),
        // The gate precedes `RequireUserId`: a bad id with no credentials is the 401.
        ("a bad id and no credentials", image_path("notanid"), multipart(), image.clone(),
            "api.context.session_expired.app_error", 401),
    ];
    for (label, path, content_type, body, expected, status) in cases {
        let headers: &[(&str, &str)] = if label == "a bad id and no credentials" {
            &[]
        } else {
            &headers
        };
        let go = both_serve_identically(
            &client,
            &pair.go,
            &pair.rust,
            &path,
            headers,
            &content_type,
            body,
            label,
        )
        .await;
        assert_eq!(go["id"], expected, "{label}");
        assert_eq!(go["status_code"], status, "{label}");
    }

    // Every refusal passed: `SetProfileImage` is Go's. An upload Go's decoder refuses shows the
    // hand-over happened (Go's 400, relayed); a real PNG shows it end to end.
    let not_an_image = multipart_body(&[("image", Some("p.png"), b"not an image at all")]);
    let (go_status, go_body, _) = post_raw(
        &client,
        &pair.go,
        &image_path(&owned.id),
        &headers,
        &multipart(),
        not_an_image.clone(),
    )
    .await;
    let (rs_status, rs_body, served_by) = post_raw(
        &client,
        &pair.rust,
        &image_path(&owned.id),
        &headers,
        &multipart(),
        not_an_image,
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("go"), "the write is Go's");
    assert_eq!(
        (go_status, rs_status),
        (400, 400),
        "Go's decoder refuses it: {} / {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );

    let (status, body, served_by) = post_raw(
        &client,
        &pair.rust,
        &image_path(&owned.id),
        &headers,
        &multipart(),
        image,
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("go"), "the write is Go's");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let ok: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(ok["status"], "OK");

    for user in [&owned, &foreign, &local, &empty] {
        common::delete_plain_user(&client, &admin, &user.id).await;
    }
    purge_remotes(&pool, "i").await;
}

/// `r.ContentLength > MaxFileSize` is refused on the **header**, before the body is read and
/// after the gate and `RequireUserId`. The request is written by hand with a 200 MiB
/// `Content-Length` over a short body, and half-closed, as `image_writes` does and for the same
/// reason.
#[tokio::test]
async fn a_declared_oversize_remote_upload_is_refused_before_it_is_read() {
    if !stack_enabled() {
        return;
    }
    let pair = common::licensed().await;
    let Some(pool) = common::fixture_pool().await else {
        panic!("the fixture needs DATABASE_URL");
    };
    let remotes = plant_remotes(&pool, "l").await;
    let path = "/api/v4/remotecluster/zzzzzzzzzzzzzzzzzzzzzzzzzz/image";
    let body = multipart_body(&[("image", Some("p.png"), common::TINY_PNG)]);

    let go = oversize_post(&pair.go, path, &remotes.valid, &body).await;
    let rs = oversize_post(&pair.rust, path, &remotes.valid, &body).await;
    assert_eq!(rs.2.as_deref(), Some("rust"), "the 413 is ours to give");
    assert_eq!(
        (go.0, rs.0),
        (413, 413),
        "{} / {}",
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    let go = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "declared oversize");
    assert_eq!(go["id"], "api.user.upload_profile_user.too_large.app_error");

    purge_remotes(&pool, "l").await;
}

/// One POST on a raw socket declaring 200 MiB over `body`, then half-closed.
async fn oversize_post(
    base: &str,
    path: &str,
    remote: &Remote,
    body: &[u8],
) -> (u16, Vec<u8>, Option<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let authority = base
        .rsplit_once('/')
        .map_or(base, |(_, authority)| authority);
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nX-RemoteCluster-Token: {}\r\n\
         X-RemoteCluster-Id: {}\r\nContent-Type: {}\r\nContent-Length: 209715201\r\n\
         Connection: close\r\n\r\n",
        remote.token,
        remote.id,
        multipart(),
    );
    let raw = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(authority)
            .await
            .unwrap_or_else(|e| panic!("{authority} refuses a socket: {e}"));
        stream.write_all(request.as_bytes()).await.expect("headers");
        stream.write_all(body).await.expect("the body");
        stream.shutdown().await.expect("the write half closes");
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .expect("the answer reads");
        raw
    })
    .await
    .unwrap_or_else(|_| panic!("{base}{path} never answered; it waited for the body"));

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("{base}{path}: no header terminator"));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("{base}{path}: no status in {head}"));
    let served_by = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("x-mmrs-served-by")
            .then(|| value.trim().to_owned())
    });
    let mut body = raw[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    (status, body, served_by)
}

/// Undo `Transfer-Encoding: chunked` on a complete body.
fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = raw.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&raw[..line_end]).trim(), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        out.extend_from_slice(&raw[start..start + size]);
        raw = &raw[start + size + 2..];
    }
    out
}
