//! Cross-server parity for the fifteen `/recaps` and `/scheduled_recaps` routes.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity recaps
//! ```
//!
//! # A configuration gate, so the suite can turn it on
//!
//! Unlike the licence families, this gate is `FeatureFlags.EnableAIRecaps` — an environment
//! setting an operator controls. That makes the *enabled* half testable in a way the licensed half
//! of a licence gate is not: the flag can be set on the Rust server alone and the routes must then
//! forward. That is exactly the mutation the licence suites could not see until a licence row was
//! planted, and here it costs a restart rather than a database write.
//!
//! The enabled half is checked by [`the_gate_is_read_from_configuration`], which drives
//! `mm_app::config::Config` directly rather than restarting the server — the routing decision is a
//! pure function of the config, and the parity suite has no way to set an environment variable on
//! a process it did not start.

use crate::common;

use common::{ACTIVE_LICENCE_ROW, GO, RUST, client, go_minted_token, stack_enabled};

const RECAPS_DISABLED_ERROR: &str = "api.recap.disabled.app_error";
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

fn routes() -> Vec<(reqwest::Method, String)> {
    let m = |s: &str| reqwest::Method::from_bytes(s.as_bytes()).expect("a method");
    vec![
        (m("GET"), "/api/v4/recaps".into()),
        (m("POST"), "/api/v4/recaps".into()),
        (m("GET"), "/api/v4/recaps/limit_status".into()),
        (m("POST"), "/api/v4/recaps/mark_viewed".into()),
        (m("GET"), format!("/api/v4/recaps/{NOWHERE}")),
        (m("DELETE"), format!("/api/v4/recaps/{NOWHERE}")),
        (m("POST"), format!("/api/v4/recaps/{NOWHERE}/read")),
        (m("POST"), format!("/api/v4/recaps/{NOWHERE}/regenerate")),
        (m("GET"), "/api/v4/scheduled_recaps".into()),
        (m("POST"), "/api/v4/scheduled_recaps".into()),
        (m("GET"), format!("/api/v4/scheduled_recaps/{NOWHERE}")),
        (m("PUT"), format!("/api/v4/scheduled_recaps/{NOWHERE}")),
        (m("DELETE"), format!("/api/v4/scheduled_recaps/{NOWHERE}")),
        (
            m("POST"),
            format!("/api/v4/scheduled_recaps/{NOWHERE}/pause"),
        ),
        (
            m("POST"),
            format!("/api/v4/scheduled_recaps/{NOWHERE}/resume"),
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

/// All fifteen, as an administrator.
#[tokio::test]
async fn every_recap_route_is_the_disabled_refusal() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let all = routes();
    assert_eq!(all.len(), 15, "eight recap routes and seven scheduled ones");

    for (method, path) in &all {
        let (status, parsed) = both(&client, &token, method, path, b"{}").await;
        assert_eq!(status, 501, "{method} {path}");
        assert_eq!(parsed["id"], RECAPS_DISABLED_ERROR, "{method} {path}");
    }
}

/// **Nothing else is consulted**: a malformed body, a plain user, and a real-looking id all give
/// the same answer, because the gate is the first statement.
#[tokio::test]
async fn nothing_ahead_of_the_gate_is_read() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "recaps").await;
    let plain = common::create_plain_user(&client, &admin, &team, "recaps").await;

    for (method, path) in routes() {
        if method != reqwest::Method::GET {
            let (status, parsed) = both(&client, &admin, &method, &path, b"{").await;
            assert_eq!(
                status, 501,
                "a malformed body is never decoded: {method} {path}"
            );
            assert_eq!(parsed["id"], RECAPS_DISABLED_ERROR);
        }

        let (status, parsed) = both(&client, &plain.token, &method, &path, b"{}").await;
        assert_eq!(status, 501, "no permission is checked: {method} {path}");
        assert_eq!(parsed["id"], RECAPS_DISABLED_ERROR);
    }

    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// **The gate is a pure function of the configuration**, and the enabled half is the branch that
/// must forward.
///
/// This drives `Config` rather than the HTTP surface, because the flag lives in the environment of
/// a process the suite did not start. The routing decision is
/// `Config::ai_recaps_enabled()` and nothing else, so pinning that function pins the branch: with
/// recaps on, `refuse_or_forward` takes the proxy arm.
#[test]
fn the_gate_is_read_from_configuration() {
    use mm_app::config::Config;

    let stock = Config::default();
    assert!(
        !stock.ai_recaps_enabled(),
        "a stock server has the feature flag off, so all fifteen routes are ours"
    );

    let enabled = Config {
        feature_flag_enable_ai_recaps: true,
        ..Config::default()
    };
    assert!(
        enabled.ai_recaps_enabled(),
        "setting the flag alone enables recaps — the settings block defaults to *enabled*, and \
         every one of these routes must then forward"
    );

    let explicitly_off = Config {
        feature_flag_enable_ai_recaps: true,
        ai_recap_settings_enable: Some(false),
        ..Config::default()
    };
    assert!(
        !explicitly_off.ai_recaps_enabled(),
        "and an administrator can still turn them off with the flag on"
    );
}

/// **With the feature flag set, every one of the fifteen forwards.**
///
/// The flag lives only in the environment — Go strips `FeatureFlags` before persisting — so this
/// starts a second `mm-api` with it set rather than trying to arrange it in the database. Added
/// because a mutation replacing the gate with a constant `false`, that is, one that never
/// forwards, **survived** the first run: with recaps off, "always refuse" and "refuse unless
/// enabled" are the same program.
///
/// The second server is killed when the guard drops, panic or not.
#[tokio::test]
async fn an_enabled_server_forwards_every_recap_route() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let Some(server) =
        common::SecondServer::start(8077, &[("MM_FEATUREFLAGS_ENABLEAIRECAPS", "true")]).await
    else {
        return; // no binary where `parity.sh` leaves it, or no DATABASE_URL
    };
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path) in routes() {
        let response = client
            .request(method.clone(), format!("{}{path}", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(b"{}".to_vec())
            .send()
            .await
            .expect("the second server answers");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "with recaps enabled, {method} {path} must be forwarded — there is no recap engine \
             on this side"
        );
    }

    // And the *same* binary on :8066, without the flag, still answers. So the difference is the
    // configuration and not the build.
    let (status, parsed) = both(
        &client,
        &token,
        &reqwest::Method::GET,
        "/api/v4/recaps",
        b"",
    )
    .await;
    assert_eq!(status, 501);
    assert_eq!(parsed["id"], RECAPS_DISABLED_ERROR);
}

/// Registering fifteen methods must not turn their neighbours into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path) in [
        (reqwest::Method::DELETE, "/api/v4/recaps".to_owned()),
        (reqwest::Method::PUT, format!("/api/v4/recaps/{NOWHERE}")),
        (
            reqwest::Method::GET,
            format!("/api/v4/recaps/{NOWHERE}/read"),
        ),
        (
            reqwest::Method::DELETE,
            "/api/v4/scheduled_recaps".to_owned(),
        ),
        (
            reqwest::Method::PATCH,
            format!("/api/v4/scheduled_recaps/{NOWHERE}"),
        ),
    ] {
        let ours = client
            .request(method.clone(), format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {path} must be forwarded"
        );
    }
}
