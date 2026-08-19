//! Shared plumbing for the cross-server parity tests.
//!
//! Compiled separately into *each* integration-test binary, which has two consequences: the
//! `OnceCell` below is per-binary — one login per test file, not one per test — and anything a
//! given test file does not use looks unused from that binary's point of view, hence the
//! `dead_code` allowance.
#![allow(dead_code)]

use std::time::Duration;

pub const GO: &str = "http://localhost:8065";
pub const RUST: &str = "http://127.0.0.1:8066";
pub const LOGIN_ID: &str = "slice@example.com";
pub const PASSWORD: &str = "Slice-Test-1234";

/// True when the caller asked for the stack-backed tests.
///
/// Without this, every parity test returns early. Deliberate: `cargo test` on a machine with no
/// Docker must stay green, and a test that silently passes because it could not reach anything is
/// worse than one that is explicitly skipped.
pub fn stack_enabled() -> bool {
    std::env::var("MM_PARITY_STACK").is_ok_and(|v| v == "1")
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("client builds")
}

/// One login per test binary.
///
/// This is not an optimisation. **A login mutates the user row** — it bumps `UpdateAt`, which
/// appears in `/users/me`'s body and in its etag. With a login per test, tests running in
/// parallel move `UpdateAt` underneath each other and a byte comparison fails against a
/// seconds-old value while both servers were in fact perfectly agreed. One login removes the only
/// writer, so a diff means a real divergence.
static TOKEN: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// Log in against the **Go** server and return the token it mints.
///
/// The token has to come from Go: the whole point is that a credential this port never issued is
/// nonetheless accepted by it, against a row it never wrote.
pub async fn go_minted_token(client: &reqwest::Client) -> String {
    TOKEN
        .get_or_init(|| async {
            let response = client
                .post(format!("{GO}/api/v4/users/login"))
                .json(&serde_json::json!({ "login_id": LOGIN_ID, "password": PASSWORD }))
                .send()
                .await
                .expect("the Go server is reachable — is `docker compose up -d` running?");

            assert_eq!(
                response.status(),
                200,
                "login against Go failed; the fixture user may not exist yet"
            );

            let token = response
                .headers()
                .get("token")
                .expect("Go returns the session token in a `Token` header")
                .to_str()
                .expect("the token is ASCII")
                .to_owned();

            // The login body is the user, so the id comes from the same round trip. Tests used to
            // hardcode it, which broke the moment [D-130] required recreating the volume: ids are
            // minted per database, and a stale one fails as "permission denied" rather than as
            // "that user does not exist".
            let user: serde_json::Value = response.json().await.expect("login returns the user");
            let id = user["id"]
                .as_str()
                .expect("the user carries an id")
                .to_owned();
            USER_ID.set(id).expect("set once");

            token
        })
        .await
        .clone()
}

static USER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The logged-in user's id. Panics unless [`go_minted_token`] has run, which every caller does
/// first because it is how they get a token at all.
pub fn logged_in_user_id() -> &'static str {
    USER_ID
        .get()
        .expect("call go_minted_token first — the id comes from the login response")
}

/// Fetch a path from both servers with the same token, returning `(go_body, rust_body)`.
pub async fn fetch_both(client: &reqwest::Client, token: &str, path: &str) -> (Vec<u8>, Vec<u8>) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        assert_eq!(response.status(), 200, "{base}{path} should return 200");
        response.bytes().await.expect("body reads").to_vec()
    };

    (get(GO).await, get(RUST).await)
}
