//! Cross-server parity for `api4/plugin_local.go` on the unix socket: all ten pairs, under both
//! plugin hosts.
//!
//! ```sh
//! scripts/parity.sh --test parity local_plugins
//! ```
//!
//! - **Go hosts the plugins** (the stack's own mm-api): the gates are answered here and must be
//!   Go's; everything past them must arrive as Go's own answer.
//! - **Rust hosts them** (a second mm-api with `MMRS_PLUGIN_HOST=rust` and a socket of its own):
//!   every pair is answered here. The oracle is the main Go server's socket, which is also where
//!   this server's `PluginStates` saves go, so there is one writer (see `plugin_toggle`). The
//!   probe is unpacked into main Go's plugin directory by hand and uploaded to ours over the
//!   socket.
//! - **Plugins off**: no Go oracle runs with `PluginSettings.Enable` off, so those answers are
//!   transcribed from the handlers (provisional, as in `plugin_statuses`).

use std::path::{Path, PathBuf};

use crate::common;

use super::plugin_toggle::go_plugin_dir;
use super::plugin_upload::{MULTIPART, bundle, form};
use common::local_socket::{go_socket, sockets_enabled};
use common::{GO, SecondServer, assert_error_bodies_match_except_known_gaps, client};

/// The hosting server and the two plugins-off ones; see `second_server_ports`.
const HOSTING_PORT: u16 = 8109;
const OFF_HOSTED_PORT: u16 = 8110;
const OFF_GO_HOSTED_PORT: u16 = 8111;
const PROBE: &str = "mmrs.local.probe";

/// One answer over a socket.
struct Answer {
    status: u16,
    rust: bool,
    content_type: String,
    body: Vec<u8>,
}

async fn send(socket: &Path, method: &str, path: &str, content_type: &str, body: &[u8]) -> Answer {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", content_type)
        .header("Content-Length", body.len().to_string())
        .body(axum::body::Body::from(body.to_vec()))
        .expect("request builds");
    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let rust = header("x-mmrs-served-by") == "rust";
    let content_type = header("content-type");
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    Answer {
        status,
        rust,
        content_type,
        body,
    }
}

/// How ours must have answered.
#[derive(Clone, Copy, PartialEq, Debug)]
enum By {
    Rust,
    Go,
}

/// The same request over Go's socket and `ours`: equal status, equal body (errors by field,
/// anything else byte for byte), and answered by whom `by` says. Returns the status.
async fn both(ours: &Path, by: By, method: &str, path: &str, json: &str) -> u16 {
    let go_socket = go_socket().expect("checked");
    let go = send(
        &go_socket,
        method,
        path,
        "application/json",
        json.as_bytes(),
    )
    .await;
    let rs = send(ours, method, path, "application/json", json.as_bytes()).await;
    let case = format!("{method} {path} {json}");
    assert_eq!(
        go.status,
        rs.status,
        "{case}: Go {} / ours {}",
        String::from_utf8_lossy(&go.body),
        String::from_utf8_lossy(&rs.body)
    );
    assert_eq!(rs.rust, by == By::Rust, "{case}: who answered");
    assert_eq!(go.content_type, rs.content_type, "{case}");
    if go.status >= 400 && by == By::Rust {
        assert_error_bodies_match_except_known_gaps(&go.body, &rs.body, &case);
    } else if go.status >= 400 {
        common::local_socket::assert_forwarded_body_is_gos(&go.body, &rs.body, &case);
    } else {
        assert_eq!(
            String::from_utf8_lossy(&go.body),
            String::from_utf8_lossy(&rs.body),
            "{case}"
        );
    }
    go.status
}

/// A second mm-api with local mode on and its own socket, forwarding to Go's.
async fn start_with_socket(port: u16, extra: &[(&str, &str)]) -> Option<(SecondServer, PathBuf)> {
    let go_socket = go_socket()?;
    let socket = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("pl-{port}.sock"));
    let _ = std::fs::remove_file(&socket);
    let (socket_s, go_s) = (
        socket.to_string_lossy().into_owned(),
        go_socket.to_string_lossy().into_owned(),
    );
    let mut env: Vec<(&str, &str)> = vec![
        ("MM_SERVICESETTINGS_ENABLELOCALMODE", "true"),
        ("MM_API_LOCAL_SOCKET", socket_s.as_str()),
        ("MM_SERVICESETTINGS_LOCALMODESOCKETLOCATION", go_s.as_str()),
    ];
    env.extend_from_slice(extra);
    let server = SecondServer::start(port, &env).await?;
    for _ in 0..50 {
        if socket.exists() {
            return Some((server, socket));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the second server never bound {}", socket.display());
}

/// A pid that no longer exists.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// The reattach bodies both routers refuse before `App.ReattachPlugin`, in Go's order: the
/// decoder, then the manifest, then the config. Keys fold as Go's decoder folds them, and a
/// `server` that is not an object is the decoder's failure too.
const REATTACH_REFUSALS: &[&str] = &[
    "",
    "[]",
    "{",
    "null",
    r#"{"PluginReattachConfig":{}}"#,
    r#"{"manifest":{"id":"mmrs.local.x"}}"#,
    r#"{"MANIFEST":{"id":"mmrs.local.x","server":[]},"PluginReattachConfig":{}}"#,
    r#"{"Manifest":{"id":"mmrs.local.x"},"PluginReattachConfig":{"Pid":"1"}}"#,
];

/// Under the stack's Go host: each gate answered here, as Go answers it — uploads are off on the
/// stack, so both installs are the 501 — and each request that passes forwarded, with its body.
#[tokio::test]
async fn a_go_host_answers_the_gates_and_forwards_the_rest() {
    if !sockets_enabled() {
        return;
    }
    let ours = common::local_socket::rust_socket().expect("checked");

    // Gates, answered here.
    for (method, path, body, status) in [
        ("POST", "/api/v4/plugins", "", 501),
        (
            "POST",
            "/api/v4/plugins/install_from_url?plugin_download_url=http://127.0.0.1:1/p.tar.gz",
            "",
            501,
        ),
        ("GET", "/api/v4/plugins/marketplace?page=x", "", 500),
        (
            "GET",
            "/api/v4/plugins/marketplace?local_only=true&remote_only=1",
            "",
            500,
        ),
        ("POST", "/api/v4/plugins/marketplace", "[]", 501),
        ("POST", "/api/v4/plugins/marketplace", r#"{"id":5}"#, 501),
    ] {
        assert_eq!(both(&ours, By::Rust, method, path, body).await, status);
    }
    for body in REATTACH_REFUSALS {
        assert_eq!(
            both(&ours, By::Rust, "POST", "/api/v4/plugins/reattach", body).await,
            400
        );
    }

    // Past the gates: Go's environment answers, through the forward.
    assert_eq!(both(&ours, By::Go, "GET", "/api/v4/plugins", "").await, 200);
    for (method, path) in [
        ("POST", "/api/v4/plugins/mmrs.local.absent/enable"),
        ("POST", "/api/v4/plugins/mmrs.local.absent/disable"),
        ("DELETE", "/api/v4/plugins/mmrs.local.absent"),
    ] {
        assert_eq!(both(&ours, By::Go, method, path, "").await, 404, "{path}");
    }
    // The one reattach error, and the detach that undoes it; the body went through.
    let webonly = r#"{"Manifest":{"id":"mmrs.local.webonly","webapp":{"bundle_path":"x"}},"PluginReattachConfig":{}}"#;
    assert_eq!(
        both(&ours, By::Go, "POST", "/api/v4/plugins/reattach", webonly).await,
        500
    );
    assert_eq!(
        both(
            &ours,
            By::Go,
            "POST",
            "/api/v4/plugins/mmrs.local.webonly/detach",
            ""
        )
        .await,
        200
    );
}

/// The probe's entry in one socket's `GET /plugins` — `"active"`, `"inactive"` or absent.
async fn probe_entry(socket: &Path) -> (Option<&'static str>, serde_json::Value) {
    let answer = send(socket, "GET", "/api/v4/plugins", "application/json", b"").await;
    assert_eq!(answer.status, 200, "{}", socket.display());
    // Ours must answer the list itself: a forwarded one is Go's and would match trivially.
    let go_socket = go_socket().expect("checked");
    assert_eq!(
        answer.rust,
        socket != go_socket,
        "who answered GET /plugins on {}",
        socket.display()
    );
    let plugins: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
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

/// Both sockets' place for the probe, which must agree.
async fn probe_place(ours: &Path, when: &str) -> Option<&'static str> {
    let go = probe_entry(&go_socket().expect("checked")).await;
    let rs = probe_entry(ours).await;
    assert_eq!(go, rs, "the probe in GET /plugins {when}");
    rs.0
}

/// The probe's entry in one socket's local-only Marketplace list.
async fn marketplace_entry(socket: &Path) -> serde_json::Value {
    let answer = send(
        socket,
        "GET",
        "/api/v4/plugins/marketplace?local_only=true",
        "application/json",
        b"",
    )
    .await;
    assert_eq!(
        answer.status,
        200,
        "{}: {}",
        socket.display(),
        String::from_utf8_lossy(&answer.body)
    );
    assert_eq!(
        answer.rust,
        socket != go_socket().expect("checked"),
        "who answered the Marketplace list on {}",
        socket.display()
    );
    let plugins: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
    plugins
        .as_array()
        .and_then(|a| a.iter().find(|p| p["manifest"]["id"] == PROBE))
        .cloned()
        .unwrap_or_default()
}

/// Take the probe's key back out of the shared `PluginStates`, through main Go.
async fn forget_probe_state(client: &reqwest::Client, admin: &str) {
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
    let Some(map) = states.as_object_mut() else {
        return;
    };
    if map.remove(PROBE).is_none() {
        return;
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

/// Under a Rust host every pair is answered here, and each answer is Go's local one.
#[tokio::test]
async fn a_rust_host_serves_every_pair_on_its_socket() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = common::go_minted_token(&client).await;

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("local-plugins");
    let _ = std::fs::remove_dir_all(&scratch);
    for dir in ["plugins", "client", "data"] {
        std::fs::create_dir_all(scratch.join(dir)).unwrap();
    }
    let s = |dir: &str| scratch.join(dir).to_string_lossy().into_owned();
    let (plugins, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let Some((_host, ours)) = start_with_socket(
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
    forget_probe_state(&client, &admin).await;

    // The probe: by hand into main Go's plugin directory, by upload over our socket.
    let probe = bundle(
        &scratch,
        "local",
        Some(&format!(
            r#"{{"id":"{PROBE}","name":"MMRS local probe","version":"1.0.0","webapp":{{"bundle_path":"dist/main.js"}}}}"#
        )),
    );
    let go_plugins = go_plugin_dir();
    let _ = std::fs::remove_dir_all(go_plugins.join(PROBE));
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(scratch.join("bundles/local/bundle.tar.gz"))
        .arg("-C")
        .arg(&go_plugins)
        .status()
        .expect("tar is on PATH");
    assert!(status.success());
    std::fs::rename(go_plugins.join("plugin"), go_plugins.join(PROBE)).unwrap();
    let upload = send(
        &ours,
        "POST",
        "/api/v4/plugins",
        MULTIPART,
        &form(Some(&probe), Some("true")),
    )
    .await;
    assert_eq!(
        (upload.status, upload.rust),
        (201, true),
        "{}",
        String::from_utf8_lossy(&upload.body)
    );
    let manifest: serde_json::Value = serde_json::from_slice(&upload.body).unwrap();
    assert_eq!(manifest["id"], PROBE);
    assert_eq!(
        probe_place(&ours, "after the installs").await,
        Some("inactive")
    );

    // The Marketplace, past its permission: the local-only list, and a body that does not decode.
    let go_socket = go_socket().expect("checked");
    let go_entry = marketplace_entry(&go_socket).await;
    assert!(go_entry.is_object(), "Go lists the probe: {go_entry}");
    assert_eq!(go_entry, marketplace_entry(&ours).await);
    assert_eq!(
        both(&ours, By::Rust, "POST", "/api/v4/plugins/marketplace", "[]").await,
        501
    );

    // An install from a URL nothing answers: past the permission, the download fails. Main Go
    // has uploads off, so the oracle is the uploads-on Go server over HTTP, as an administrator.
    let path = "/api/v4/plugins/install_from_url?plugin_download_url=http://127.0.0.1:1/p.tar.gz";
    let go = client
        .post(format!("{}{path}", super::plugin_upload::oracle()))
        .bearer_auth(&admin)
        .header("X-Requested-With", "XMLHttpRequest")
        .send()
        .await
        .expect("the plugins oracle answers");
    let go_status = go.status().as_u16();
    let go_body = go.bytes().await.unwrap();
    let rs = send(&ours, "POST", path, "application/json", b"").await;
    assert_eq!((go_status, rs.status, rs.rust), (400, 400, true));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs.body, "install_from_url");

    // Enable, disable, remove: the same answers, and the same place for the probe after each.
    let one = format!("/api/v4/plugins/{PROBE}");
    for (method, path, status, place) in [
        ("POST", format!("{one}/enable"), 200, Some("active")),
        ("POST", format!("{one}/disable"), 200, Some("inactive")),
        ("DELETE", one.clone(), 200, None),
        ("DELETE", one.clone(), 404, None),
        (
            "POST",
            "/api/v4/plugins/mmrs.local.absent/enable".to_owned(),
            404,
            None,
        ),
    ] {
        assert_eq!(
            both(&ours, By::Rust, method, &path, "").await,
            status,
            "{path}"
        );
        assert_eq!(
            probe_place(&ours, &format!("after {method} {path}")).await,
            place
        );
    }
    assert!(
        !scratch
            .join(format!("data/plugins/{PROBE}.tar.gz"))
            .exists(),
        "the bundle left our file store"
    );

    // Reattach and detach. The refusals; the one reattach error; a reattach to a process that is
    // gone, which Go swallows into a success; and the detaches, which succeed for any id.
    for body in REATTACH_REFUSALS {
        assert_eq!(
            both(&ours, By::Rust, "POST", "/api/v4/plugins/reattach", body).await,
            400
        );
    }
    let webonly = r#"{"Manifest":{"id":"mmrs.local.webonly","webapp":{"bundle_path":"x"}},"PluginReattachConfig":{}}"#;
    assert_eq!(
        both(&ours, By::Rust, "POST", "/api/v4/plugins/reattach", webonly).await,
        500
    );
    let gone = format!(
        r#"{{"manifest":{{"ID":"mmrs.local.gone","SERVER":{{"executable":"x"}}}},"pluginReattachConfig":{{"Protocol":"netrpc","ProtocolVersion":1,"Pid":{},"Addr":{{"Name":"/nonexistent/mmrs.sock","Net":"unix"}}}}}}"#,
        dead_pid()
    );
    assert_eq!(
        both(&ours, By::Rust, "POST", "/api/v4/plugins/reattach", &gone).await,
        200
    );
    // The same id again, now without a server: `ReattachPlugin` detaches first, so the running
    // registration is replaced and refused — without the detach, `Reattach` would find the id
    // running and answer success.
    let again = r#"{"Manifest":{"id":"mmrs.local.gone","webapp":{"bundle_path":"x"}},"PluginReattachConfig":{}}"#;
    assert_eq!(
        both(&ours, By::Rust, "POST", "/api/v4/plugins/reattach", again).await,
        500
    );
    for id in ["mmrs.local.gone", "mmrs.local.webonly", "mmrs.local.absent"] {
        let path = format!("/api/v4/plugins/{id}/detach");
        assert_eq!(both(&ours, By::Rust, "POST", &path, "").await, 200);
    }

    let _ = std::fs::remove_dir_all(go_plugins.join(PROBE));
    forget_probe_state(&client, &admin).await;
}

/// With `PluginSettings.Enable` off each pair is its 501, under either host — reattach only after
/// its body has been validated — and nothing is forwarded. Transcribed: no Go oracle runs with
/// plugins off (see the module docs).
#[tokio::test]
async fn plugins_off_is_the_501_under_either_host() {
    if !sockets_enabled() {
        return;
    }
    let valid = r#"{"Manifest":{"id":"mmrs.local.x"},"PluginReattachConfig":{}}"#;
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "GET",
            "/api/v4/plugins",
            "",
            "app.plugin.disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins",
            "",
            "app.plugin.upload_disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins/install_from_url",
            "",
            "app.plugin.disabled.app_error",
        ),
        (
            "GET",
            "/api/v4/plugins/marketplace?page=x",
            "",
            "app.plugin.disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins/marketplace",
            "[]",
            "app.plugin.disabled.app_error",
        ),
        (
            "DELETE",
            "/api/v4/plugins/mmrs.x",
            "",
            "app.plugin.disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins/mmrs.x/enable",
            "",
            "app.plugin.disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins/mmrs.x/disable",
            "",
            "app.plugin.disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins/mmrs.x/detach",
            "",
            "app.plugin.disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins/reattach",
            valid,
            "app.plugin.disabled.app_error",
        ),
        (
            "POST",
            "/api/v4/plugins/reattach",
            "{}",
            "plugin_reattach_request.is_valid.manifest.app_error",
        ),
    ];
    for (port, host) in [(OFF_HOSTED_PORT, "rust"), (OFF_GO_HOSTED_PORT, "go")] {
        let Some((_server, ours)) = start_with_socket(
            port,
            &[
                ("MMRS_PLUGIN_HOST", host),
                ("MM_PLUGINSETTINGS_ENABLE", "false"),
            ],
        )
        .await
        else {
            return;
        };
        for (method, path, body, id) in cases {
            let answer = send(&ours, method, path, "application/json", body.as_bytes()).await;
            let case = format!("{host} host: {method} {path} {body}");
            assert!(answer.rust, "{case}: forwarded");
            let error: serde_json::Value =
                serde_json::from_slice(&answer.body).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(error["id"], *id, "{case}");
            let status = if id.starts_with("plugin_reattach") {
                400
            } else {
                501
            };
            assert_eq!(answer.status, status, "{case}");
            assert_eq!(error["status_code"], status, "{case}");
        }
    }
}
