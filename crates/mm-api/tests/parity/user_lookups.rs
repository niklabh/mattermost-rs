//! Cross-server parity for `api4/user.go`'s two literal-path reads:
//! `GET /api/v4/users/auth_data` and `GET /api/v4/users/invalid_emails`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity user_lookups
//! ```
//!
//! # `/users/invalid_emails` can only be compared in its refusal
//!
//! The handler answers **400** whenever `TeamSettings.EnableOpenServer` is on, and
//! `scripts/go-server.sh` pins that variable on through the environment. An environment override
//! never reaches the configuration document, so there is no way to tell the Go server on :8065
//! otherwise without restarting it — and restarting it would move the ground under every other
//! suite in this binary.
//!
//! So the 400, and the fact that it is checked **before** the permission, are compared against Go
//! here; the 200 path is exercised against a [`common::SecondServer`] with the variable off, which
//! is our answer only, and the *query* underneath it is tested in `mm-store`'s
//! `db_users_invalid_emails`. That is the whole of the coverage and none of it is a cross-server
//! comparison of a successful response. See MIGRATION.md and [D-213].

use crate::common;

use common::{
    GO, RUST, SecondServer, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    create_team, fetch_both_raw, fetch_both_stable, go_minted_token, plant_bot, purge_api_fixtures,
    set_user_roles, stack_enabled,
};

const AUTH_PATH: &str = "/api/v4/users/auth_data";
const INVALID_PATH: &str = "/api/v4/users/invalid_emails";

/// The `AuthData` planted on the fixture user. No REST route sets this column.
const AUTH_DATA: &str = "mmrs-parity-auth-data-value";

struct Fixture {
    subject_id: String,
    /// A `system_user`, for the permission checks.
    plain_token: String,
    planted: bool,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "ulook").await;
            let subject = create_plain_user(client, token, &team_id, "ulooksubject").await;
            let plain = create_plain_user(client, token, &team_id, "ulookplain").await;

            let planted = set_auth_data(&subject.id, AUTH_DATA).await
                && plant_bot("ulookbot", common::logged_in_user_id(), 0)
                    .await
                    .is_some()
                && set_user_roles(&plain.id, "system_user").await;

            Fixture {
                subject_id: subject.id,
                plain_token: plain.token,
                planted,
            }
        })
        .await
}

/// Write `Users.AuthData`, which no API can set on an existing account.
async fn set_auth_data(user_id: &str, auth_data: &str) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return false;
    };
    sqlx::query("UPDATE users SET authdata = $2 WHERE id = $1")
        .bind(user_id)
        .bind(auth_data)
        .execute(&pool)
        .await
        .is_ok()
}

/// The planted account comes back byte for byte, under the etag Go computed.
#[tokio::test]
async fn the_planted_auth_data_resolves_to_its_user() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.planted {
        return;
    }

    let path = format!("{AUTH_PATH}?value={AUTH_DATA}");
    let (go, rs) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert_eq!(go.last(), Some(&b'\n'), "the encoder's newline");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["id"], f.subject_id.as_str());
    assert_eq!(
        parsed["auth_data"], AUTH_DATA,
        "`SanitizeProfile(user, true)` keeps `auth_data` for an admin viewer — which is the only \
         kind of viewer this route has"
    );
}

/// The `ETag` header matches, and sending it back is a 304 on both servers.
#[tokio::test]
async fn the_etag_round_trips_to_a_304() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.planted {
        return;
    }

    let path = format!("{AUTH_PATH}?value={AUTH_DATA}");
    let etag_of = async |base: &str| -> String {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("a response");
        assert_eq!(response.status(), 200, "{base}{path}");
        response
            .headers()
            .get("ETag")
            .expect("an ETag header")
            .to_str()
            .expect("ASCII")
            .to_owned()
    };
    let go_etag = etag_of(GO).await;
    let rs_etag = etag_of(RUST).await;
    assert_eq!(go_etag, rs_etag, "the etags must agree");

    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", &go_etag)
            .send()
            .await
            .expect("a response");
        assert_eq!(
            response.status(),
            304,
            "{base}{path} with the matching etag"
        );
        assert_eq!(
            response.headers().get("ETag").and_then(|v| v.to_str().ok()),
            Some(go_etag.as_str()),
            "`HandleEtag` sets the header on the 304 too"
        );
        assert!(
            response.bytes().await.expect("a body").is_empty(),
            "a 304 has no body"
        );
    }

    // A different etag is a 200 again — the comparison is equality, not presence.
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!((go_status, rs_status), (200, 200));
}

/// Every way the `value` parameter can be refused, and the 404 that is not one of them.
#[tokio::test]
async fn the_value_parameter_is_refused_the_same_way() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let too_long = "x".repeat(129);
    let at_the_limit = "y".repeat(128);
    let cases: [(String, u16, &str); 5] = [
        // Absent and empty are the same branch: `r.URL.Query().Get` returns `""` for both.
        (
            AUTH_PATH.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            format!("{AUTH_PATH}?value="),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            format!("{AUTH_PATH}?value={too_long}"),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        // 128 is `UserAuthDataMaxLength` and the check is `>`, so the bound itself is allowed
        // through — and then simply misses.
        (
            format!("{AUTH_PATH}?value={at_the_limit}"),
            404,
            "app.user.missing_account.const",
        ),
        (
            format!("{AUTH_PATH}?value=mmrs-no-such-auth-data"),
            404,
            "app.user.missing_account.const",
        ),
    ];

    for (path, status, id) in cases {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, status, "{}", &path[..path.len().min(60)]);
        assert_eq!(
            rs_status,
            go_status,
            "{}: statuses must match",
            &path[..path.len().min(60)]
        );
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], id);
    }

    // The parameter's *name* in the error is `value`, and Go reports it through
    // `NewInvalidParamError`, whose id says "body param" for something that is in the query
    // string. Reproduced; a client branching on the id would branch the same way.
    let ((_, go_body), (_, rs_body)) = fetch_both_raw(&client, &token, AUTH_PATH).await;
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, AUTH_PATH);
    assert_eq!(go["id"], "api.context.invalid_body_param.app_error");
}

/// `IsSystemAdmin`, and it is checked before the parameter.
#[tokio::test]
async fn a_plain_caller_is_refused_before_the_parameter_is_read() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for path in [
        AUTH_PATH.to_owned(),
        format!("{AUTH_PATH}?value={AUTH_DATA}"),
        format!("{AUTH_PATH}?value={}", "z".repeat(200)),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &f.plain_token, &path).await;
        assert_eq!(
            go_status, 403,
            "the permission is checked first, so even a bad parameter is a 403"
        );
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.permissions.app_error");
    }
}

/// `/users/invalid_emails` refuses, identically, because the stack's server is an open server.
///
/// The id is `model.NoTranslation` — the literal `<untranslated>` — which is the first ported
/// route to use it, and it is the reason error bodies are now written with Go's HTML escaping:
/// `encoding/json` renders those angle brackets as `<` and `>`, and `serde_json` does
/// not. See `mm_api::error::ApiError::into_wire`.
#[tokio::test]
async fn the_open_server_refusal_matches_and_precedes_the_permission() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, INVALID_PATH).await;
    assert_eq!(go_status, 400, "{INVALID_PATH}");
    assert_eq!(rs_status, go_status, "statuses must match");
    // `request_id` is per-request, so the bodies go through the helper that drops it; the
    // escaping is asserted on the raw bytes below, which is the part that was wrong.
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, INVALID_PATH);
    assert_eq!(go["id"], "<untranslated>");
    assert_eq!(go["message"], "<untranslated>");
    assert_eq!(
        go["detailed_error"], "",
        "the detail that says *why* is wiped with every other detailed_error"
    );
    for (label, body) in [("Go", &go_body), ("ours", &rs_body)] {
        let text = String::from_utf8_lossy(body);
        assert!(
            text.contains("\\u003cuntranslated\\u003e"),
            "{label} must HTML-escape the angle brackets `encoding/json` escapes: {text}"
        );
        assert!(
            !text.contains("<untranslated>"),
            "{label} must not write them raw: {text}"
        );
    }

    // **Order.** The configuration is checked before the permission, so an unprivileged caller
    // gets the 400 and learns a setting they may not read. Swapping the two is a one-line
    // mutation and this is the only test that sees it.
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, INVALID_PATH).await;
    assert_eq!(go_status, 400, "not the 403 a reader would predict");
    assert_eq!(rs_status, go_status, "statuses must match");
    let refused = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, INVALID_PATH);
    assert_eq!(
        refused["id"], "<untranslated>",
        "the gate, not the permission"
    );

    // Pagination is read after both, so a garbage page is still the 400.
    let path = format!("{INVALID_PATH}?page=nonsense&per_page=-4");
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!((go_status, rs_status), (400, 400));
}

/// The 200 path, **against our server only**.
///
/// There is no Go counterpart: :8065 is an open server and cannot be told otherwise without a
/// restart. What this can still prove is that the route serves at all once the gate is open, that
/// the gate is a gate rather than an unconditional refusal, that the permission check behind it is
/// reachable, and that the four store exclusions reach the wire. The query itself is tested
/// against planted rows in `mm-store`'s `db_users_invalid_emails`.
#[tokio::test]
async fn with_the_open_server_off_the_route_serves_a_page() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.planted {
        return;
    }

    let Some(server) =
        SecondServer::start(8071, &[("MM_TEAMSETTINGS_ENABLEOPENSERVER", "false")]).await
    else {
        return;
    };

    let response = client
        .get(format!("{}{INVALID_PATH}?per_page=200", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the second server answers");
    assert_eq!(
        response.status(),
        200,
        "with the gate open the route serves — so the 400 above is the gate and not the handler"
    );
    let body = response.bytes().await.expect("a body").to_vec();
    assert_eq!(body.last(), Some(&b'\n'), "the encoder's newline");
    let users: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let ids: Vec<&str> = users
        .as_array()
        .expect("an array")
        .iter()
        .map(|user| user["id"].as_str().expect("an id"))
        .collect();

    assert!(
        ids.contains(&f.subject_id.as_str()),
        "an ordinary active account with no auth service is reported"
    );
    assert!(
        !ids.iter().any(|id| id.starts_with("mmrsbot")),
        "the planted bot is not: {ids:?}"
    );
    for user in users.as_array().expect("an array") {
        assert!(
            user.get("password").is_none() || user["password"] == "",
            "every row is sanitised: {user}"
        );
        assert!(
            user.get("email")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "and every row keeps its email, which is the point: {user}"
        );
    }

    // The permission check is behind the gate, so it is only reachable here.
    let refused = client
        .get(format!("{}{INVALID_PATH}", server.base))
        .header("Authorization", format!("Bearer {}", f.plain_token))
        .send()
        .await
        .expect("the second server answers");
    assert_eq!(
        refused.status(),
        403,
        "`sysconsole_read_user_management_users`, reachable only with the gate open"
    );
}
