//! The `DisableWhenBusy` gate on the five served search routes — `APISessionRequiredDisableWhenBusy`
//! (api4/handlers.go:179), refused by `web.Handler.ServeHTTP` with
//! `api.context.server_busy.app_error` / 503 while the server is busy (web/handlers.go:349).
//!
//! ```sh
//! scripts/parity.sh --test parity busy_gates
//! ```
//!
//! **Measured on this server only**, as `typing`'s busy test is: marking the Go server busy would
//! refuse every `DisableWhenBusy` route — `createPost` among them — for every suite running
//! beside this one, and the busy flag is per process ([D-320]). The Go half is the source. Every
//! test here takes the **write** guard on `common::BUSY_STATE`; the suites that call these routes
//! hold read guards, so the 503 window waits for them.
//!
//! The gate runs after authentication and before the body is read: an unauthenticated caller
//! gets a 401 on a busy server, and a valid one gets the 503 for a body that would otherwise be
//! a 400.

use crate::common;

use common::{BUSY_STATE, RUST, client, create_team, go_minted_token, stack_enabled};

/// `POST /api/v4/server_busy?seconds=30` on this server, then `f`, then the `DELETE` — cleared
/// before any assertion so a failure does not leave the server busy for its neighbours.
async fn while_busy<F, Fut, T>(client: &reqwest::Client, token: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let set = client
        .post(format!("{RUST}/api/v4/server_busy?seconds=30"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        set.status().as_u16(),
        200,
        "{}",
        set.text().await.unwrap_or_default()
    );
    let out = f().await;
    let clear = client
        .delete(format!("{RUST}/api/v4/server_busy"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(clear.status().as_u16(), 200, "clearing the busy flag");
    out
}

async fn post(
    client: &reqwest::Client,
    token: Option<&str>,
    path: &str,
    body: &str,
) -> (u16, bool, serde_json::Value) {
    let mut request = client
        .post(format!("{RUST}{path}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned());
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.expect("we answer");
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response.json().await.unwrap_or(serde_json::Value::Null),
    )
}

fn assert_busy(status: u16, served: bool, body: &serde_json::Value, path: &str) {
    assert_eq!(status, 503, "{path}: {body}");
    assert!(served, "{path}: the gate is this server's");
    assert_eq!(
        body["id"], "api.context.server_busy.app_error",
        "{path}: {body}"
    );
    assert_eq!(body["status_code"], 503, "{path}");
}

/// The five routes, each a 503 while busy and a success once the flag is cleared.
#[tokio::test]
async fn the_five_searches_are_refused_while_busy_and_answer_after() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "busyg").await;

    let routes: [(String, &str); 5] = [
        ("/api/v4/channels/search".to_owned(), r#"{"term":"busyg"}"#),
        (
            "/api/v4/channels/group/search".to_owned(),
            r#"{"term":"busyg"}"#,
        ),
        ("/api/v4/teams/search".to_owned(), r#"{"term":"busyg"}"#),
        (
            format!("/api/v4/teams/{team_id}/channels/search"),
            r#"{"term":"busyg"}"#,
        ),
        ("/api/v4/users/search".to_owned(), r#"{"term":"busyg"}"#),
    ];

    for (path, body) in &routes {
        let (status, served, answer) =
            while_busy(&client, &token, || post(&client, Some(&token), path, body)).await;
        assert_busy(status, served, &answer, path);

        // Cleared: the same request is answered here, and not with a 503.
        let (status, served, answer) = post(&client, Some(&token), path, body).await;
        assert_eq!(status, 200, "{path} after clearing: {answer}");
        assert!(served, "{path}: served once the flag is cleared");
    }
}

/// Authentication comes first: no session is a 401 even while busy. The body comes after: a
/// malformed one is the 503, not its 400.
#[tokio::test]
async fn the_gate_sits_between_authentication_and_the_body() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let (unauthenticated, malformed) = while_busy(&client, &token, || async {
        let unauthenticated = post(&client, None, "/api/v4/users/search", r#"{"term":"x"}"#).await;
        let malformed = post(&client, Some(&token), "/api/v4/teams/search", "{").await;
        (unauthenticated, malformed)
    })
    .await;
    assert_eq!(unauthenticated.0, 401, "{}", unauthenticated.2);
    assert!(unauthenticated.1, "the 401 is this server's");
    assert_busy(
        malformed.0,
        malformed.1,
        &malformed.2,
        "/api/v4/teams/search with a malformed body",
    );
}
