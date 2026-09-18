//! Cross-server parity for the three writes on one plugin with this server hosting plugins:
//! `POST /plugins/{plugin_id}/enable`, `POST /plugins/{plugin_id}/disable` and
//! `DELETE /plugins/{plugin_id}` (api4/plugin.go:220-382).
//!
//! ```sh
//! scripts/parity.sh --test parity plugin_toggle
//! ```
//!
//! Enable and disable save `PluginStates[id]`. Go saves it itself; this server hands the save to
//! the main Go server (`mm_app::peer_config`) and reloads. So the oracle is the **main** Go
//! server, not the uploads one: a Go server saves its whole in-memory configuration, and
//! `scripts/go-plugins.sh`'s copy is as old as its start, so a save there puts back every setting
//! anyone changed since — measured, it dropped a key this suite had just planted. With main Go the
//! only writer, both sides' saves land on one current copy.
//!
//! The probe goes into main Go's plugin directory by hand (`Available` reads the disk) and into the
//! hosting Rust server by upload. Each is asked in turn to enable it, disable it and remove it;
//! the answers, and where each side's `GET /plugins` puts the probe, must agree.
//!
//! Go's remove leaves `PluginStates[id] = false` in the shared document for good. The suite takes
//! that key back out at the end, so the document is what it was.

use std::path::PathBuf;

use crate::common;

use super::plugin_upload::{MULTIPART, bundle, form, send};
use common::{
    GO, SecondServer, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, go_minted_token, request_raw, stack_enabled,
};

/// The hosting server; see `second_server_ports`.
const HOSTING_PORT: u16 = 8069;
const PROBE: &str = "mmrs.toggle.probe";
/// A key no default supplies, planted so the suite can see that a save carries the whole map.
const KEEP: &str = "mmrs.toggle.keep";

/// One write on both servers: the same status, and the same body — `{"status":"OK"}` or an error.
async fn both(
    client: &reqwest::Client,
    rust: &str,
    method: reqwest::Method,
    token: &str,
    path: &str,
    case: &str,
) -> u16 {
    let (go_status, go_body, _) =
        request_raw(client, GO, method.clone(), Some(token), path, None).await;
    let (rs_status, rs_body, served_by) =
        request_raw(client, rust, method, Some(token), path, None).await;
    assert_eq!(
        go_status,
        rs_status,
        "{case}: Go {} / Rust {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    if go_status == 200 {
        assert_eq!(served_by.as_deref(), Some("rust"), "{case}");
        assert_eq!(go_body, rs_body, "{case}");
    } else if go_status != 404 || served_by.as_deref() == Some("rust") {
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, case);
    }
    go_status
}

/// Where the probe sits in one server's `GET /plugins` — `"active"`, `"inactive"` or absent —
/// and its entry. The oracle also keeps what other suites installed on it, so only the probe is
/// compared.
async fn probe_entry(
    client: &reqwest::Client,
    base: &str,
    admin: &str,
) -> (Option<&'static str>, serde_json::Value) {
    let (status, body, _) = request_raw(
        client,
        base,
        reqwest::Method::GET,
        Some(admin),
        "/api/v4/plugins",
        None,
    )
    .await;
    assert_eq!(status, 200, "{base}: GET /plugins");
    let plugins: serde_json::Value = serde_json::from_slice(&body).unwrap();
    for list in ["active", "inactive"] {
        if let Some(entry) = plugins[list]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"] == PROBE))
        {
            return (Some(list), entry.clone());
        }
    }
    (None, serde_json::Value::Null)
}

/// Both sides' place for the probe, which must agree; whether it is active.
async fn probe_active(client: &reqwest::Client, rust: &str, admin: &str, when: &str) -> bool {
    let go = probe_entry(client, GO, admin).await;
    let rs = probe_entry(client, rust, admin).await;
    assert_eq!(go, rs, "the probe in GET /plugins {when}");
    rs.0 == Some("active")
}

/// Take the probe's key back out of the shared `PluginStates`: a patch replaces the map, so the
/// map without it is sent whole.
async fn forget_probe_state(client: &reqwest::Client, admin: &str, plant: bool) {
    let config: serde_json::Value = client
        .get(format!("{GO}/api/v4/config"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the configuration");
    let mut states = config["PluginSettings"]["PluginStates"].clone();
    if let Some(map) = states.as_object_mut() {
        map.remove(PROBE);
        map.remove(KEEP);
        if plant {
            map.insert(KEEP.to_owned(), serde_json::json!({ "Enable": false }));
        }
    }
    let status = client
        .put(format!("{GO}/api/v4/config/patch"))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "PluginSettings": { "PluginStates": states } }))
        .send()
        .await
        .expect("Go answers")
        .status();
    assert!(status.is_success(), "restoring PluginStates: {status}");
}

#[tokio::test]
async fn enable_disable_and_remove_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-toggle");
    let _ = std::fs::remove_dir_all(&scratch);
    for dir in ["plugins", "client", "data"] {
        std::fs::create_dir_all(scratch.join(dir)).unwrap();
    }
    let s = |dir: &str| scratch.join(dir).to_string_lossy().into_owned();
    let (plugins, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let Some(host) = SecondServer::start(
        HOSTING_PORT,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_ENABLEUPLOADS", "true"),
            ("MM_PLUGINSETTINGS_DIRECTORY", plugins.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
            ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ],
    )
    .await
    else {
        return;
    };
    let rust = host.base.as_str();

    // The same probe on both, not enabled by anything yet, and a key only this suite plants.
    forget_probe_state(&client, &admin, true).await;
    let probe = bundle(
        &scratch,
        "toggle",
        Some(&format!(
            r#"{{"id":"{PROBE}","name":"MMRS toggle probe","version":"1.0.0","webapp":{{"bundle_path":"dist/main.js"}}}}"#
        )),
    );
    // Main Go: unpacked into its plugin directory, where `Available` finds it.
    let go_plugins = go_plugin_dir();
    let _ = std::fs::remove_dir_all(go_plugins.join(PROBE));
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(scratch.join("bundles/toggle/bundle.tar.gz"))
        .arg("-C")
        .arg(&go_plugins)
        .status()
        .expect("tar is on PATH");
    assert!(status.success());
    std::fs::rename(go_plugins.join("plugin"), go_plugins.join(PROBE)).unwrap();
    {
        let base = rust.to_owned();
        let answer = send(
            &client,
            &base,
            &admin,
            MULTIPART,
            &form(Some(&probe), Some("true")),
        )
        .await;
        assert_eq!(
            answer.status,
            201,
            "{base}: {}",
            String::from_utf8_lossy(&answer.body)
        );
    }
    assert!(!probe_active(&client, rust, &admin, "after the installs").await);

    let one = format!("/api/v4/plugins/{PROBE}");
    let (enable, disable) = (format!("{one}/enable"), format!("{one}/disable"));

    // The refusals: a plain user, a plugin that is not installed, and an id Go's router refuses.
    let team = common::create_team(&client, &admin, "pltg").await;
    let user = create_plain_user(&client, &admin, &team, "pltg").await;
    for (method, path) in [
        (reqwest::Method::POST, enable.as_str()),
        (reqwest::Method::POST, disable.as_str()),
        (reqwest::Method::DELETE, one.as_str()),
    ] {
        let status = both(&client, rust, method, &user.token, path, "a plain user").await;
        assert_eq!(status, 403, "{path}");
    }
    // A read-only admin reads plugins and cannot write them.
    if common::set_user_roles(&user.id, "system_user system_read_only_admin").await {
        let reader = common::login_plain_user(&client, "pltg").await;
        let status = both(
            &client,
            rust,
            reqwest::Method::POST,
            &reader,
            &enable,
            "a read-only admin",
        )
        .await;
        assert_eq!(status, 403);
    }
    delete_plain_user(&client, &admin, &user.id).await;
    for path in [
        "/api/v4/plugins/mmrs.not.installed/enable",
        "/api/v4/plugins/mmrs.not.installed/disable",
        "/api/v4/plugins/mmrs.not.installed",
    ] {
        let method = if path.ends_with("installed") {
            reqwest::Method::DELETE
        } else {
            reqwest::Method::POST
        };
        let status = both(&client, rust, method, &admin, path, "not installed").await;
        assert_eq!(status, 404, "{path}");
    }
    let status = both(
        &client,
        rust,
        reqwest::Method::POST,
        &admin,
        "/api/v4/plugins/mmrs%20probe/enable",
        "an id outside Go's mux",
    )
    .await;
    assert_eq!(status, 404);

    // Enable: running on both by the time each answers. The id is matched lowercased.
    let upper = format!("/api/v4/plugins/{}/enable", PROBE.to_uppercase());
    assert_eq!(
        both(
            &client,
            rust,
            reqwest::Method::POST,
            &admin,
            &upper,
            "enable, upper case"
        )
        .await,
        200
    );
    assert!(probe_active(&client, rust, &admin, "after enable").await);
    // Rust's save went through the main Go server, whole map and all: the planted key survived.
    let states = live_states(&client, &admin).await;
    assert_eq!(states[PROBE]["Enable"], true, "{states}");
    assert_eq!(states[KEEP]["Enable"], false, "{states}");

    assert_eq!(
        both(
            &client,
            rust,
            reqwest::Method::POST,
            &admin,
            &disable,
            "disable"
        )
        .await,
        200
    );
    assert!(!probe_active(&client, rust, &admin, "after disable").await);

    assert_eq!(
        both(
            &client,
            rust,
            reqwest::Method::DELETE,
            &admin,
            &one,
            "remove"
        )
        .await,
        200
    );
    assert_eq!(
        probe_entry(&client, rust, &admin).await.0,
        None,
        "the probe is gone"
    );
    probe_active(&client, rust, &admin, "after remove").await;
    assert!(
        !scratch
            .join(format!("data/plugins/{PROBE}.tar.gz"))
            .exists(),
        "the bundle left the file store"
    );
    assert_eq!(
        both(
            &client,
            rust,
            reqwest::Method::DELETE,
            &admin,
            &one,
            "remove again"
        )
        .await,
        404
    );

    let _ = std::fs::remove_dir_all(go_plugins.join(PROBE));
    forget_probe_state(&client, &admin, false).await;
}

/// The main Go server's plugin directory on this stack.
fn go_plugin_dir() -> PathBuf {
    let offset: u16 = std::env::var("MMRS_PORT_OFFSET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let suffix = if offset == 0 {
        String::new()
    } else {
        format!("-{}", offset / 100)
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../reference/.build/mmroot{suffix}/plugins"))
}

/// The live `PluginStates` as the main Go server holds it.
async fn live_states(client: &reqwest::Client, admin: &str) -> serde_json::Value {
    let config: serde_json::Value = client
        .get(format!("{GO}/api/v4/config"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the configuration");
    config["PluginSettings"]["PluginStates"].clone()
}
