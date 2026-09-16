//! Cross-server parity for a **configuration write reaching this server's projection** — D-701.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity config_reload
//! ```
//!
//! `mm_app::App::config` used to be loaded once at startup, so after a write the Go server and
//! the full-document reads moved at once and every gate that consults the projection kept the
//! old value until a restart. The subject is therefore not a route but the **gap between a write
//! and the next gated read**, measured on both of the paths that close it:
//!
//! - **A write through this server** is followed by `refresh_config_after_write`, so the very
//!   next request must see it. The test reads immediately — no sleep, no retry — and toggles
//!   twice, so the periodic check cannot rescue a broken middleware by landing in both
//!   few-millisecond windows.
//! - **A write made straight to Go** passes through nothing here; only the periodic check can
//!   see it. That test sleeps past the check, because the wait *is* the behaviour, and probes
//!   once per write so the probe's own reload cannot stand in for the timer.
//!
//! # The gate is `ServiceSettings.EnableFileSearch`
//!
//! Projected (`Config::enable_file_search`), read live by Go, consulted on a served route
//! (`POST /api/v4/files/search` → `SearchFilesInTeamForUser`'s 501) and by no other suite except
//! `file_search`, which holds [`common::FILE_SEARCH_SETTING`]'s read guard for every search. The
//! write guard is held here from the first patch to the restore.
//!
//! **Restored at the start as well as the end**: an assertion panics past the trailing restore,
//! and a stack left with file search off fails `file_search` on the next run for a reason that
//! suite never mentions.

use std::time::Duration;

use futures_util::FutureExt;

use crate::common;

use common::{FILE_SEARCH_SETTING, GO, RUST, client, go_minted_token, stack_enabled};

const SEARCH: &str = "/api/v4/files/search";
const DISABLED: &str = "store.sql_file_info.search.disabled";

/// `PUT {base}/api/v4/config/patch` with `EnableFileSearch` only, asserting Go's 200 — the save
/// is Go's whichever server received it (see `mm_api::config_writes`).
async fn set_file_search(client: &reqwest::Client, token: &str, base: &str, enabled: bool) {
    let response = client
        .put(format!("{base}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "ServiceSettings": { "EnableFileSearch": enabled } }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "patching EnableFileSearch={enabled} on {base}: {body}"
    );
}

/// One all-teams file search on one server: `(status, error id)`, the id empty on a 200.
async fn search(client: &reqwest::Client, token: &str, base: &str) -> (u16, String) {
    let response = client
        .post(format!("{base}{SEARCH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "terms": "mmrsconfigreload", "is_or_search": false }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{SEARCH} is unreachable: {e}"));
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    let id = if status == 200 {
        String::new()
    } else {
        body["id"].as_str().unwrap_or_default().to_owned()
    };
    (status, id)
}

/// Run `body` with `EnableFileSearch` restored to `true` afterwards, **panic or not**, while the
/// caller still holds [`FILE_SEARCH_SETTING`].
///
/// The setting is persisted in the shared configuration; the lock only serialises. A panic after a
/// `false` write would otherwise release the lock with file search off, and every `file_search`
/// test after it would fail its 200. Restored through this server, so its own projection reloads
/// before the lock goes.
async fn restoring_file_search(
    client: &reqwest::Client,
    token: &str,
    body: impl std::future::Future<Output = ()>,
) {
    let outcome = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    set_file_search(client, token, RUST, true).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Both servers, Go first, asserting each answers `enabled` — a 200, or the disabled 501.
async fn assert_both(client: &reqwest::Client, token: &str, enabled: bool, context: &str) {
    let expected = if enabled {
        (200, String::new())
    } else {
        (501, DISABLED.to_owned())
    };
    assert_eq!(search(client, token, GO).await, expected, "{context}: Go");
    assert_eq!(
        search(client, token, RUST).await,
        expected,
        "{context}: Rust"
    );
}

#[tokio::test]
async fn a_configuration_write_through_this_server_gates_the_next_request() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _setting = FILE_SEARCH_SETTING.write().await;
    set_file_search(&client, &admin, GO, true).await;

    restoring_file_search(&client, &admin, async {
        set_file_search(&client, &admin, RUST, false).await;
        assert_both(
            &client,
            &admin,
            false,
            "right after turning file search off here",
        )
        .await;

        set_file_search(&client, &admin, RUST, true).await;
        assert_both(&client, &admin, true, "right after turning it back on here").await;
    })
    .await;
}

/// `localPatchConfig` over this server's socket forwards over Go's, so the socket router needs the
/// reload as much as the HTTP one — and nothing else in the binary would notice it missing.
#[tokio::test]
async fn a_configuration_write_over_the_local_socket_gates_the_next_request() {
    if !common::local_socket::sockets_enabled() {
        return;
    }
    let socket = common::local_socket::rust_socket().expect("checked by sockets_enabled");
    let client = client();
    let admin = go_minted_token(&client).await;
    let _setting = FILE_SEARCH_SETTING.write().await;
    set_file_search(&client, &admin, GO, true).await;

    restoring_file_search(&client, &admin, async {
        for enabled in [false, true] {
            let body = format!(r#"{{"ServiceSettings":{{"EnableFileSearch":{enabled}}}}}"#);
            let request = axum::http::Request::builder()
                .method("PUT")
                .uri("/api/v4/config/patch")
                .header("Host", "localhost")
                .header("Content-Type", "application/json")
                .header("Content-Length", body.len().to_string())
                .body(axum::body::Body::from(body))
                .expect("request builds");
            let response = mm_api::local::send_over_unix(&socket, request)
                .await
                .expect("the patch reaches this server's socket");
            assert_eq!(
                response.status().as_u16(),
                200,
                "patching EnableFileSearch={enabled} over the socket"
            );
            let expected = if enabled { 200 } else { 501 };
            assert_eq!(
                search(&client, &admin, RUST).await.0,
                expected,
                "the first request after a socket patch to EnableFileSearch={enabled}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_configuration_write_made_to_go_directly_is_picked_up_without_a_request() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _setting = FILE_SEARCH_SETTING.write().await;

    set_file_search(&client, &admin, GO, true).await;
    assert_eq!(
        rust_after_the_timer(&client, &admin).await,
        200,
        "before the write"
    );

    restoring_file_search(&client, &admin, async {
        set_file_search(&client, &admin, GO, false).await;
        assert_eq!(
            search(&client, &admin, GO).await,
            (501, DISABLED.to_owned()),
            "Go swaps its own copy on its own save"
        );
        assert_eq!(
            rust_after_the_timer(&client, &admin).await,
            501,
            "a write made to Go reaches this server with no request through it"
        );

        set_file_search(&client, &admin, GO, true).await;
        assert_eq!(
            rust_after_the_timer(&client, &admin).await,
            200,
            "and so does its restore"
        );
    })
    .await;
}

/// This server's search status, asked **once**, after long enough for the periodic check to
/// have run at least twice.
///
/// Once, because the search is a POST: its own `refresh_config_after_write` reloads *after* it is
/// answered, so a second probe would pass on the middleware alone and prove nothing about the
/// timer. The last request this server saw before the write was the previous single probe, whose
/// reload ran against the previous document. So the only thing that can move this answer is the
/// timer — `MM_API_CONFIG_POLL_MS=200` on the stack (`scripts/mm-api-env.sh`), hence the 600ms.
async fn rust_after_the_timer(client: &reqwest::Client, token: &str) -> u16 {
    tokio::time::sleep(Duration::from_millis(600)).await;
    search(client, token, RUST).await.0
}
