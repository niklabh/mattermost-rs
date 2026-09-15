//! Cross-server parity for `api4/agents.go` — `GET /api/v4/agents`, `GET /api/v4/agents/status`
//! and `GET /api/v4/llmservices`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity agents
//! ```
//!
//! No `mattermost-ai` plugin is installed on the stack's Go, and this server hosts none, so both
//! answer the bridge's "not active" status and empty lists — compared byte for byte, since all
//! three are `json.Marshal` + `w.Write`.

use crate::common;

use common::{RUST, client, fetch_both_raw, go_minted_token, stack_enabled};

#[tokio::test]
async fn the_three_reads_are_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;

    for (path, expected) in [
        (
            "/api/v4/agents/status",
            r#"{"available":false,"reason":"app.agents.bridge.not_available.plugin_not_active"}"#,
        ),
        ("/api/v4/agents", "[]"),
        ("/api/v4/llmservices", "[]"),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, path).await;
        assert_eq!(go_status, 200, "{path}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, 200, "{path}: {}", String::from_utf8_lossy(&rs));
        assert_eq!(String::from_utf8_lossy(&go), expected, "{path}: Go");
        assert_eq!(rs, go, "{path}");
    }
}

/// `APISessionRequired` on all three: no token is a 401 before anything else.
#[tokio::test]
async fn the_session_comes_first() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    for path in [
        "/api/v4/agents",
        "/api/v4/agents/status",
        "/api/v4/llmservices",
    ] {
        let response = client
            .get(format!("{RUST}{path}"))
            .send()
            .await
            .expect("Rust answers");
        assert_eq!(response.status(), 401, "{path}");
    }
}
