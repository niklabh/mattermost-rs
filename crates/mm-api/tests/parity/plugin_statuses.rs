//! Cross-server parity for `GET /api/v4/plugins/statuses` (api4/plugin.go:198) with this server
//! hosting plugins (`MMRS_PLUGIN_HOST=rust`).
//!
//! ```sh
//! scripts/parity.sh --test parity plugin_statuses
//! ```
//!
//! The main Rust server leaves plugins to Go and forwards the route (`marketplace_visit` checks
//! that). Here a second Rust server hosts them over its own plugin directory, and the Go server
//! hosts the same three webapp-only bundles over its own:
//!
//! - `playbooks`, enabled by `SetDefaults`' `PluginStates`, runs;
//! - `com.mattermost.calls`, enabled the same way, needs Mattermost 99 and fails to start;
//! - `mmrsstatus.off`, which no state enables, stays off.
//!
//! Webapp-only on purpose: nothing is launched, so two hosts at once cannot step on each other
//! (docs/PLUGIN_PLAN.md, D6), and the ids are the defaulted ones so the shared configuration gains
//! no key. Go only looks at its plugin directory when its configuration changes, so the suite
//! toggles the NPS plugin's state — nothing installs NPS — and restores it. At the end `playbooks`
//! is disabled on Go before its bundle goes, so Go's environment keeps no running registration for
//! a bundle that is no longer there, and every state is put back.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::common;

use common::{
    GO, SecondServer, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, go_minted_token, request_raw, stack_enabled,
};

const PATH: &str = "/api/v4/plugins/statuses";
/// The hosting server; see `second_server_ports`.
const HOSTING_PORT: u16 = 8075;
/// A hosting server with plugins switched off.
const DISABLED_PORT: u16 = 8076;

const BUNDLES: [(&str, &str); 3] = [
    (
        "playbooks",
        r#"{"id":"playbooks","name":"MMRS status probe","description":"runs","version":"9.9.9","webapp":{"bundle_path":"dist/main.js"}}"#,
    ),
    (
        "com.mattermost.calls",
        r#"{"id":"com.mattermost.calls","name":"MMRS too new","version":"1.0.0","min_server_version":"99.0.0","webapp":{"bundle_path":"dist/main.js"}}"#,
    ),
    (
        "mmrsstatus.off",
        r#"{"id":"mmrsstatus.off","name":"MMRS off","version":"0.0.1","webapp":{"bundle_path":"dist/main.js"}}"#,
    ),
];

/// The Go server's run directory for this stack, where its `./plugins` resolves.
fn go_root() -> PathBuf {
    let offset: u16 = std::env::var("MMRS_PORT_OFFSET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let suffix = if offset == 0 {
        String::new()
    } else {
        format!("-{}", offset / 100)
    };
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../reference/.build/mmroot{suffix}"))
}

fn install(plugins: &Path) {
    for (id, manifest) in BUNDLES {
        let dir = plugins.join(id);
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("plugin.json"), manifest).unwrap();
        std::fs::write(dir.join("dist/main.js"), format!("// {id}\n")).unwrap();
    }
}

fn uninstall(plugins: &Path, client_plugins: &Path) {
    for (id, _) in BUNDLES {
        let _ = std::fs::remove_dir_all(plugins.join(id));
        let _ = std::fs::remove_dir_all(client_plugins.join(id));
    }
}

/// `PUT /config/patch` on Go, setting one plugin's state.
async fn set_state(client: &reqwest::Client, token: &str, id: &str, enable: bool) {
    let response = client
        .put(format!("{GO}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "PluginSettings": { "PluginStates": { id: { "Enable": enable } } }
        }))
        .send()
        .await
        .expect("Go answers");
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status, 200, "patching {id}={enable}: {body}");
}

/// A statuses body with each `plugin_path` cut to the bundle's own directory name, which is the
/// only part two servers with two plugin directories can share.
fn comparable(body: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(body).expect("statuses decode");
    for status in value.as_array_mut().expect("an array").iter_mut() {
        let path = status["plugin_path"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let base = path.rsplit('/').next().unwrap_or_default().to_owned();
        status["plugin_path"] = serde_json::Value::String(base);
    }
    value
}

/// Go's statuses once its environment has caught up with the bundles: `playbooks` running.
async fn go_statuses_settled(client: &reqwest::Client, token: &str) -> Vec<u8> {
    let mut last = Vec::new();
    for _ in 0..50 {
        let (status, body, _) =
            request_raw(client, GO, reqwest::Method::GET, Some(token), PATH, None).await;
        assert_eq!(
            status,
            200,
            "Go statuses: {}",
            String::from_utf8_lossy(&body)
        );
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let running = value.as_array().is_some_and(|all| {
            all.iter()
                .any(|s| s["plugin_id"] == "playbooks" && s["state"] == 2)
        });
        if running {
            return body;
        }
        last = body;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!(
        "Go never activated the probe bundle: {}",
        String::from_utf8_lossy(&last)
    );
}

#[tokio::test]
async fn statuses_from_this_host_match_go() {
    if !stack_enabled() {
        return;
    }
    // The read-only admin's permissions depend on the licence row.
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;

    let go_root = go_root();
    let go_plugins = go_root.join("plugins");
    let go_client = go_root.join("client/plugins");
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-statuses");
    let _ = std::fs::remove_dir_all(&scratch);
    let rs_plugins = scratch.join("plugins");
    let rs_client = scratch.join("client");
    std::fs::create_dir_all(&rs_plugins).unwrap();

    uninstall(&go_plugins, &go_client);
    install(&go_plugins);
    install(&rs_plugins);
    // Go reads its plugin directory on a configuration change.
    set_state(&client, &admin, "com.mattermost.nps", false).await;
    set_state(&client, &admin, "com.mattermost.nps", true).await;
    let go_body = go_statuses_settled(&client, &admin).await;

    let dir = rs_plugins.to_string_lossy().into_owned();
    let client_dir = rs_client.to_string_lossy().into_owned();
    let Some(hosting) = SecondServer::start(
        HOSTING_PORT,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ],
    )
    .await
    else {
        cleanup(&client, &admin, &go_plugins, &go_client).await;
        return;
    };

    let (rs_status, rs_body, served_by) = request_raw(
        &client,
        &hosting.base,
        reqwest::Method::GET,
        Some(&admin),
        PATH,
        None,
    )
    .await;
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert!(rs_body.ends_with(b"]\n"), "json.NewEncoder's newline");

    // Only the three probe bundles: a Go stack may host nothing else, and neither may this.
    let go = comparable(&go_body);
    let rust = comparable(&rs_body);
    assert_eq!(go, rust, "the statuses");
    assert_eq!(
        rust.as_array().map(Vec::len),
        Some(3),
        "the three probe bundles"
    );
    // The webapp was unpacked where the setting points, under Go's name for it.
    let unpacked: Vec<String> = std::fs::read_dir(rs_client.join("playbooks"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        unpacked
            .iter()
            .any(|f| f.starts_with("playbooks_") && f.ends_with("_bundle.js")),
        "{unpacked:?}"
    );

    // The refusals: a plain user is 403 on sysconsole_read_plugins, an anonymous caller 401.
    let team = common::create_team(&client, &admin, "plst").await;
    let user = create_plain_user(&client, &admin, &team, "plst").await;
    let (go_status, go_err, _) = request_raw(
        &client,
        GO,
        reqwest::Method::GET,
        Some(&user.token),
        PATH,
        None,
    )
    .await;
    let (rs_status, rs_err, served_by) = request_raw(
        &client,
        &hosting.base,
        reqwest::Method::GET,
        Some(&user.token),
        PATH,
        None,
    )
    .await;
    assert_eq!((go_status, rs_status), (403, 403));
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_error_bodies_match_except_known_gaps(&go_err, &rs_err, "GET as a plain user");
    let (go_status, go_err, _) =
        request_raw(&client, GO, reqwest::Method::GET, None, PATH, None).await;
    let (rs_status, rs_err, _) = request_raw(
        &client,
        &hosting.base,
        reqwest::Method::GET,
        None,
        PATH,
        None,
    )
    .await;
    assert_eq!((go_status, rs_status), (401, 401));
    assert_error_bodies_match_except_known_gaps(&go_err, &rs_err, "anonymous GET");
    // The permission is `sysconsole_read_plugins`, not `manage_system`: a read-only admin holds
    // the first and not the second, so it is admitted by Go's rule and refused by the wrong one.
    if common::set_user_roles(&user.id, "system_user system_read_only_admin").await {
        // A fresh token: a session keeps the roles it logged in with.
        let reader = common::login_plain_user(&client, "plst").await;
        let (go_status, go_read, _) =
            request_raw(&client, GO, reqwest::Method::GET, Some(&reader), PATH, None).await;
        let (rs_status, rs_read, _) = request_raw(
            &client,
            &hosting.base,
            reqwest::Method::GET,
            Some(&reader),
            PATH,
            None,
        )
        .await;
        assert_eq!(
            (go_status, rs_status),
            (200, 200),
            "a read-only admin: {}",
            String::from_utf8_lossy(&rs_read)
        );
        assert_eq!(comparable(&go_read), comparable(&rs_read));
    }
    delete_plain_user(&client, &admin, &user.id).await;

    // A configuration change reaches this host as Go's listener reaches Go's: disabling the
    // running probe deactivates it on both.
    set_state(&client, &admin, "playbooks", false).await;
    let go = settled(&client, &admin, GO, 0).await;
    let rust = settled(&client, &admin, &hosting.base, 0).await;
    assert_eq!(go, rust, "the statuses after playbooks was disabled");

    drop(hosting);
    uninstall(&go_plugins, &go_client);
    set_state(&client, &admin, "playbooks", true).await;
}

/// One server's statuses once `playbooks` is in `state`.
async fn settled(
    client: &reqwest::Client,
    token: &str,
    base: &str,
    state: i64,
) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for _ in 0..50 {
        let (status, body, _) =
            request_raw(client, base, reqwest::Method::GET, Some(token), PATH, None).await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        let value = comparable(&body);
        let reached = value.as_array().is_some_and(|all| {
            all.iter()
                .any(|s| s["plugin_id"] == "playbooks" && s["state"] == state)
        });
        if reached {
            return value;
        }
        last = value;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("{base}: playbooks never reached state {state}: {last}");
}

/// Put Go back: disable the running probe so no registration outlives its bundle, remove the
/// bundles, and restore the state.
async fn cleanup(client: &reqwest::Client, admin: &str, go_plugins: &Path, go_client: &Path) {
    set_state(client, admin, "playbooks", false).await;
    uninstall(go_plugins, go_client);
    set_state(client, admin, "playbooks", true).await;
}

/// With plugins off, the 501 comes before the permission check. Go's answer is transcribed from
/// plugin.go:199-201 rather than measured: switching plugins off on the shared Go server would
/// shut down its environment under every other suite.
#[tokio::test]
async fn disabled_plugins_are_501_before_the_permission_check() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-statuses-off");
    let dir = scratch.join("plugins").to_string_lossy().into_owned();
    let Some(server) = SecondServer::start(
        DISABLED_PORT,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_ENABLE", "false"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ],
    )
    .await
    else {
        return;
    };
    let team = common::create_team(&client, &admin, "ploff").await;
    let user = create_plain_user(&client, &admin, &team, "ploff").await;
    for token in [&admin, &user.token] {
        let (status, body, served_by) = request_raw(
            &client,
            &server.base,
            reqwest::Method::GET,
            Some(token),
            PATH,
            None,
        )
        .await;
        assert_eq!(status, 501);
        assert_eq!(served_by.as_deref(), Some("rust"));
        let error: serde_json::Value = serde_json::from_slice(&body).expect("an error body");
        assert_eq!(error["id"], "app.plugin.disabled.app_error");
        assert_eq!(error["status_code"], 501);
    }
    delete_plain_user(&client, &admin, &user.id).await;
}
