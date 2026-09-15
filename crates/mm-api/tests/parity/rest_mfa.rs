//! Cross-server parity for MFA on REST routes: `ServeHTTP` calls `MfaRequired` for every handler
//! registered with `RequireMfa` (web/handlers.go:345) — `APISessionRequired`, its `TrustRequester`
//! and `DisableWhenBusy` variants — and not for `APISessionRequiredMfa`, the two routes that set MFA
//! up.
//!
//! ```sh
//! MMRS_LICENSED_VARIANT=mfa scripts/go-licensed.sh start
//! scripts/parity.sh --test parity rest_mfa
//! ```
//!
//! Runs on the licensed **MFA** pair. Every refusal on the Rust side must carry
//! `x-mmrs-served-by: rust`: a forwarded request would be refused by the Go half of the pair and
//! pass for the wrong reason.

use crate::common;

use common::{
    PlainUser, client, create_plain_user, create_team, go_minted_token, licensed_mfa, request_raw,
    stack_enabled,
};

struct Fixture {
    /// Has not set MFA up.
    owes: PlainUser,
    /// Enrolled.
    enrolled: PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(http: &reqwest::Client, admin: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(http, admin, "rmf").await;
            let owes = create_plain_user(http, admin, &team, "rmfo").await;
            let enrolled = create_plain_user(http, admin, &team, "rmfe").await;
            let pool = common::fixture_pool().await.expect("the fixture database");
            sqlx::query(
                "UPDATE users SET mfaactive = true, mfasecret = 'MMRSMFAFIXTURE' WHERE id = $1",
            )
            .bind(&enrolled.id)
            .execute(&pool)
            .await
            .expect("the enrolment fixture is written");
            Fixture { owes, enrolled }
        })
        .await
}

/// `(status, error id)` — the id is empty for a success or a body that is not an error.
fn outcome(status: u16, body: &[u8]) -> (u16, String) {
    let id = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_owned))
        .filter(|_| status >= 400)
        .unwrap_or_default();
    (status, id)
}

/// `(what, method, path, body, as whom)`.
type Case<'a> = (
    &'a str,
    reqwest::Method,
    String,
    Option<&'a [u8]>,
    &'a PlainUser,
);

#[tokio::test]
async fn rest_routes_refuse_a_user_who_owes_mfa_except_where_go_exempts_them() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let pair = licensed_mfa().await;
    let f = fixture(&http, &admin).await;

    let owes_id = f.owes.id.clone();
    let cases: Vec<Case> = vec![
        (
            "an ordinary route",
            reqwest::Method::GET,
            "/api/v4/users/me/teams".to_owned(),
            None,
            &f.owes,
        ),
        (
            "the user by id, not the exempt spelling",
            reqwest::Method::GET,
            format!("/api/v4/users/{owes_id}"),
            None,
            &f.owes,
        ),
        (
            "/users/me, exempt",
            reqwest::Method::GET,
            "/api/v4/users/me".to_owned(),
            None,
            &f.owes,
        ),
        (
            "/users/me with a query, still exempt",
            reqwest::Method::GET,
            "/api/v4/users/me?since=0".to_owned(),
            None,
            &f.owes,
        ),
        (
            "the MFA update route, exempt",
            reqwest::Method::PUT,
            format!("/api/v4/users/{owes_id}/mfa"),
            Some(br#"{"activate":false}"#),
            &f.owes,
        ),
        (
            "the enrolled user",
            reqwest::Method::GET,
            "/api/v4/users/me/teams".to_owned(),
            None,
            &f.enrolled,
        ),
    ];

    for (what, method, path, body, user) in cases {
        let (go_status, go_body, _) = request_raw(
            &http,
            &pair.go,
            method.clone(),
            Some(&user.token),
            &path,
            body,
        )
        .await;
        let (rust_status, rust_body, served_by) =
            request_raw(&http, &pair.rust, method, Some(&user.token), &path, body).await;
        let go = outcome(go_status, &go_body);
        let rust = outcome(rust_status, &rust_body);

        assert_eq!(
            go,
            rust,
            "{what}: {path}\n  go: {}\nrust: {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rust_body)
        );
        if go.1 == "api.context.mfa_required.app_error" {
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "{what}: our refusal must be our own, not Go's through the proxy"
            );
        }
        match what {
            "an ordinary route" | "the user by id, not the exempt spelling" => assert_eq!(
                go,
                (403, "api.context.mfa_required.app_error".to_owned()),
                "{what}: Go is expected to refuse"
            ),
            "/users/me, exempt" | "/users/me with a query, still exempt" | "the enrolled user" => {
                assert_eq!(go.0, 200, "{what}: Go is expected to serve")
            }
            _ => assert_ne!(
                go.1, "api.context.mfa_required.app_error",
                "{what}: Go is expected to exempt the route"
            ),
        }
    }
}
