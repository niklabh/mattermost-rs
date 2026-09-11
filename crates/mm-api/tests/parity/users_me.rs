//! Cross-server parity for `GET /api/v4/users/me`.
//!
//! This is the slice's oracle, and it is the only kind of test that can prove what the slice
//! claims: that a client cannot tell which server answered. A Rust-only round-trip would assert
//! that we agree with ourselves.
//!
//! # Running it
//!
//! Needs the development stack up and both servers running:
//!
//! ```sh
//! docker compose up -d
//! cargo run -p mm-api                       # :8066, forwards to :8065
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity users_me
//! ```
//!
//! Without `MM_PARITY_STACK=1` every test here returns early. That is deliberate: `cargo test`
//! on a laptop with no Docker must stay green, and a test that silently passes because it could
//! not reach anything is worse than one that is explicitly skipped.

use crate::common;

use common::{GO, LOGIN_ID, RUST, client, go_minted_token, stack_enabled};

/// `users.UpdateAt` for the fixture user, straight from the shared database.
async fn fixture_user_update_at() -> i64 {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set to check our value against the row");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&database_url)
        .await
        .expect("the shared Postgres is reachable");
    sqlx::query_scalar("SELECT updateat FROM users WHERE email = $1")
        .bind(LOGIN_ID)
        .fetch_one(&pool)
        .await
        .expect("the fixture user exists")
}

/// The claim the whole slice rests on: same token, same bytes.
#[tokio::test]
async fn users_me_is_byte_identical_across_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let fetch = async |base: &str| {
        let response = client
            .get(format!("{base}/api/v4/users/me"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
        let status = response.status();
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = response.bytes().await.expect("body reads").to_vec();
        (status, etag, body)
    };

    // The row's value **before** the requests. `users.UpdateAt` moves under this test — Go bumps
    // it whenever the fixture user is joined to a team, which several other suites in this
    // binary do while this one runs — and the check at the bottom compares our answer against
    // the row. Reading the row only afterwards turned a concurrent write into a failure on most
    // full-suite runs; bracketing the requests turns it back into what it is, a value that moved
    // between two reads. See [D-160].
    let row_update_at_before = fixture_user_update_at().await;

    let (go_status, go_etag, go_body) = fetch(GO).await;
    let (rs_status, rs_etag, rs_body) = fetch(RUST).await;

    assert_eq!(go_status, 200, "Go should serve /users/me");
    assert_eq!(rs_status, 200, "Rust should serve /users/me");

    // # `update_at` is exempted, and the exemption is the finding
    //
    // The Go server answers `/users/me` from an in-memory **user cache**. The login above bumps
    // `UpdateAt` in the database and does not invalidate that cache, so Go keeps serving the
    // pre-login value — measured at six seconds stale and still stale after fifteen seconds of
    // polling, i.e. not a race but a cache that will not converge on its own.
    //
    // So the two servers really do disagree on this field, and **we are the correct one**: our
    // value matches the row. Asserting byte-identity against a stale cache would assert the
    // wrong thing, and quietly dropping the field would hide it. Instead the field is normalised
    // out of the byte comparison — which still covers every other field, the key order and the
    // trailing newline — and then checked against the database directly below.
    //
    // Recorded as D-087. It is a property of two servers with independent caches over one
    // database, not of this port, and it will apply to every cached read the migration touches.
    let normalise = |body: &[u8]| {
        let text = String::from_utf8_lossy(body).into_owned();
        let Some(start) = text.find("\"update_at\":") else {
            return text;
        };
        let value_start = start + "\"update_at\":".len();
        let value_end = text[value_start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|offset| value_start + offset)
            .unwrap_or(text.len());
        format!(
            "{}\"update_at\":<normalised>{}",
            &text[..start],
            &text[value_end..]
        )
    };

    assert_eq!(
        normalise(&rs_body),
        normalise(&go_body),
        "apart from `update_at` (D-087, Go's stale user cache) the two servers must return \
         byte-identical JSON — same fields, same key order, same trailing newline"
    );

    // Both really did carry an `update_at`; a typo in `normalise` must not make the assertion
    // above vacuous by silently matching two bodies that never had the field.
    assert!(
        normalise(&rs_body).contains("\"update_at\":<normalised>"),
        "the normalisation should have found and replaced the field"
    );

    // And ours is the value that is actually in the database. This is what turns "different from
    // Go" into "correct, where Go is stale".
    //
    // The claim is bracketed rather than exact: `UpdateAt` only ever moves forward, so "our
    // answer lies between the row before the request and the row after it" says precisely that
    // we read the row at request time and never a cached older value — while a concurrent
    // writer, which is the normal state of this binary, cannot fail it. An equality against a
    // single later read said the same thing only when nothing else was running.
    let row_update_at_after = fixture_user_update_at().await;

    let rs_json: serde_json::Value = serde_json::from_slice(&rs_body).expect("our body decodes");
    let ours = rs_json["update_at"]
        .as_i64()
        .expect("we serialise update_at");
    assert!(
        (row_update_at_before..=row_update_at_after).contains(&ours),
        "our update_at ({ours}) must be the row's value at request time, not a cached one — \
         the row held {row_update_at_before} before the request and {row_update_at_after} after"
    );

    // And Go's really is outside that window on the stale side, which is the divergence D-087
    // records. Asserted rather than assumed: if Go's cache ever starts converging, this test
    // should say so rather than keep normalising a field the two servers now agree on.
    let go_json: serde_json::Value = serde_json::from_slice(&go_body).expect("Go's body decodes");
    let theirs = go_json["update_at"]
        .as_i64()
        .expect("Go serialises update_at");
    assert!(
        theirs <= ours,
        "Go's cached update_at ({theirs}) can be stale but never ahead of the row ({ours})"
    );

    // The etag is `{version}.{id}.{update_at}.{tos_id}.{tos_create_at}.{full_name}.{email}.
    // {bot_icon}` — and the version is itself three dot-separated numbers, so "strip the version"
    // means dropping three components, not one. Getting that wrong compares `11.0.…` against
    // `10.0.…` and reports a mismatch that is really an off-by-two in the test.
    //
    // Two components are exempt:
    //   * the version — ours is the pinned SHA's CURRENT_VERSION (11.11.0), the container image's
    //     is its own release (11.10.0). See D-080; they agree when both are built from the pin.
    //   * `update_at` — the etag embeds it, so it inherits D-087 above.
    let comparable = |etag: &str| {
        let parts: Vec<&str> = etag.split('.').collect();
        assert!(
            parts.len() > 4,
            "an etag should have a version, an id and the user fields: {etag}"
        );
        let mut rest: Vec<String> = parts[3..].iter().map(|s| (*s).to_owned()).collect();
        rest[1] = "<normalised>".to_owned(); // update_at
        rest.join(".")
    };

    assert_eq!(
        comparable(&rs_etag),
        comparable(&go_etag),
        "etags must agree on everything but the server version (D-080) and update_at (D-087)"
    );
    assert!(
        rs_etag.starts_with(mm_model::version::CURRENT_VERSION),
        "our etag should carry the pinned CURRENT_VERSION, got {rs_etag}"
    );
}

/// Every rejection path must produce the status Go produces. A port that is more permissive than
/// the original is a security bug; one that is stricter breaks working clients.
#[tokio::test]
async fn rejected_credentials_match_gos_status_exactly() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();

    // A session **id** is not a credential. `SessionStore.Get` matches `Token = $1 OR Id = $1`,
    // so the row comes back either way and only the `session.Token != token` check rejects it.
    // If this ever returns 200, session ids have become bearer tokens.
    let token = go_minted_token(&client).await;
    let session_id = {
        let response = client
            .get(format!("{GO}/api/v4/users/me/sessions"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("sessions list is reachable");
        let sessions: serde_json::Value = response.json().await.expect("sessions decode");
        sessions[0]["id"]
            .as_str()
            .expect("a session has an id")
            .to_owned()
    };

    let cases: Vec<(&str, Option<String>)> = vec![
        ("no credentials at all", None),
        (
            "a token that matches no row",
            Some("deadbeefdeadbeefdeadbeefxx".to_owned()),
        ),
        ("a session id used as a token", Some(session_id)),
    ];

    for (name, token) in cases {
        let send = async |base: &str| {
            let mut request = client.get(format!("{base}/api/v4/users/me"));
            if let Some(token) = &token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            request
                .send()
                .await
                .unwrap_or_else(|e| panic!("{base} unreachable: {e}"))
                .status()
        };

        let go_status = send(GO).await;
        let rs_status = send(RUST).await;
        assert_eq!(
            rs_status, go_status,
            "status mismatch for {name}: rust={rs_status} go={go_status}"
        );
        assert_eq!(rs_status, 401, "{name} should be unauthorised");
    }
}

/// An unmigrated route must be forwarded, and the answer must be Go's.
///
/// The canary was `GET /api/v4/system/ping` until 2026-09-07 and
/// `GET /api/v4/config/client?format=old` until 2026-09-11, each moved on the day its route was
/// migrated. It is now `GET /api/v4/plugins/webapp`, chosen for the same two properties: it is
/// `api.APIHandler`, so it needs no session and the test needs no fixture, and it is behind the
/// plugin host, which is not ported at all.
///
/// **When this route is migrated, move the canary rather than deleting the test.** It is the only
/// assertion in the suite that the fallback still exists at all.
#[tokio::test]
async fn an_unmigrated_route_is_forwarded_to_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let response = client
        .get(format!("{RUST}/api/v4/plugins/webapp"))
        .send()
        .await
        .expect("the proxy is reachable");

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "the webapp plugin list is not migrated, so the proxy should have forwarded it"
    );

    let body: serde_json::Value = response.json().await.expect("the plugin list decodes");
    assert!(
        body.is_array(),
        "and the body is Go's `[]*model.Manifest`, not something we synthesised: {body}"
    );
}

/// The migrated route must announce itself as the other case, or an operator watching the cutover
/// cannot tell the two apart.
#[tokio::test]
async fn the_migrated_route_reports_itself_as_rust() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let response = client
        .get(format!("{RUST}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");

    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust")
    );
}
