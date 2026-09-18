//! Cross-server parity for `POST /api/v4/plugins` (api4/plugin.go:45, `uploadPlugin` and
//! `installPlugin`) with this server hosting plugins.
//!
//! ```sh
//! scripts/parity.sh --test parity plugin_upload
//! ```
//!
//! The main Go server runs with `EnableUploads` off and the API cannot turn it on, so the oracle
//! is `scripts/go-plugins.sh`: the same database with uploads on, its own plugin directory and
//! its own file store. The Rust side is a second `mm-api` hosting plugins over scratch
//! directories. Every case is sent to both and the answers compared: the refusals in Go's order,
//! the plain-text 400s `http.Error` writes, an install, the refused re-install, a forced upgrade,
//! and the install of a plugin the configuration already enables, which must be running when the
//! upload answers.
//!
//! The oracle keeps what the last run installed; every install here is forced first, which
//! removes it, so each run starts from the same place on both sides.

use std::path::{Path, PathBuf};

use crate::common;

use common::{
    GO, SecondServer, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, go_minted_token, request_raw, stack_enabled,
};

const PATH: &str = "/api/v4/plugins";
/// The hosting server; see `second_server_ports`.
const HOSTING_PORT: u16 = 8073;
/// A hosting server that requires signatures.
const SIGNED_PORT: u16 = 8070;
const BOUNDARY: &str = "mmrsPluginUploadBoundary";

/// `scripts/go-plugins.sh`: Go's port + 36.
fn oracle() -> String {
    let port: u16 = GO
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("GO has a port");
    format!("http://localhost:{}", port + 36)
}

/// A bundle directory `<root>/<id>` holding `plugin.json` and a webapp, packed as `<id>.tar.gz`.
fn bundle(scratch: &Path, name: &str, manifest: Option<&str>) -> Vec<u8> {
    let root = scratch.join("bundles").join(name);
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("plugin");
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    if let Some(manifest) = manifest {
        std::fs::write(dir.join("plugin.json"), manifest).unwrap();
    }
    std::fs::write(dir.join("dist/main.js"), format!("// {name}\n")).unwrap();
    let out = root.join("bundle.tar.gz");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(&root)
        .arg("plugin")
        .status()
        .expect("tar is on PATH");
    assert!(status.success());
    std::fs::read(out).unwrap()
}

/// A `multipart/form-data` body: `force` when given, and the file under `plugin` when given.
fn form(file: Option<&[u8]>, force: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(force) = force {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"force\"\r\n\r\n{force}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some(file) = file {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"plugin\"; filename=\"p.tar.gz\"\r\nContent-Type: application/gzip\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(file);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

struct Answer {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    rust: bool,
}

async fn send(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    content_type: &str,
    body: &[u8],
) -> Answer {
    let response = client
        .post(format!("{base}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Content-Type", content_type)
        .body(body.to_vec())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let content_type = header("content-type");
    let rust = header("x-mmrs-served-by") == "rust";
    Answer {
        status: response.status().as_u16(),
        content_type,
        rust,
        body: response.bytes().await.unwrap().to_vec(),
    }
}

/// Send one case to both and compare: the status, then the body — as an error when it is one,
/// byte for byte otherwise (the manifests and the plain-text 400s).
async fn both(
    client: &reqwest::Client,
    rust: &str,
    token: &str,
    content_type: &str,
    body: &[u8],
    case: &str,
) -> serde_json::Value {
    let go = send(client, &oracle(), token, content_type, body).await;
    let rs = send(client, rust, token, content_type, body).await;
    assert!(rs.rust, "{case}: not served by this server");
    assert_eq!(
        go.status,
        rs.status,
        "{case}: Go {} / Rust {}",
        String::from_utf8_lossy(&go.body),
        String::from_utf8_lossy(&rs.body)
    );
    assert_eq!(go.content_type, rs.content_type, "{case}: Content-Type");
    let is_error = serde_json::from_slice::<serde_json::Value>(&go.body)
        .is_ok_and(|v| v.get("status_code").is_some());
    if is_error {
        return assert_error_bodies_match_except_known_gaps(&go.body, &rs.body, case);
    }
    assert_eq!(
        String::from_utf8_lossy(&go.body),
        String::from_utf8_lossy(&rs.body),
        "{case}"
    );
    serde_json::from_slice(&go.body).unwrap_or(serde_json::Value::Null)
}

const MULTIPART: &str = "multipart/form-data; boundary=mmrsPluginUploadBoundary";

#[tokio::test]
async fn uploads_install_as_they_do_on_go() {
    if !stack_enabled() {
        return;
    }
    // The read-only admin's permissions depend on the licence row.
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let ping = client
        .get(format!("{}/api/v4/system/ping", oracle()))
        .send()
        .await;
    assert!(
        ping.is_ok_and(|r| r.status().is_success()),
        "plugin_upload: no plugins oracle at {}. Run `scripts/go-plugins.sh start`, or \
         `scripts/stack.sh up <n>`, which starts it.",
        oracle()
    );
    let admin = go_minted_token(&client).await;

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-upload");
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

    let probe = bundle(
        &scratch,
        "probe",
        Some(
            r#"{"id":"mmrs.upload.probe","name":"MMRS upload probe","version":"1.0.0","webapp":{"bundle_path":"dist/main.js"}}"#,
        ),
    );
    let upgrade = bundle(
        &scratch,
        "upgrade",
        Some(
            r#"{"id":"mmrs.upload.probe","name":"MMRS upload probe","version":"1.0.1","webapp":{"bundle_path":"dist/main.js"}}"#,
        ),
    );
    // An id the configuration enables by default, so the install activates it.
    let enabled = bundle(
        &scratch,
        "enabled",
        Some(
            r#"{"id":"mattermost-ai","name":"MMRS enabled probe","version":"0.0.1","webapp":{"bundle_path":"dist/main.js"}}"#,
        ),
    );
    let no_manifest = bundle(&scratch, "nomanifest", None);
    let bad_id = bundle(
        &scratch,
        "badid",
        Some(r#"{"id":"x","name":"too short","webapp":{"bundle_path":"dist/main.js"}}"#),
    );

    // The permission comes after the 501 gates, which both pass here.
    let team = common::create_team(&client, &admin, "plup").await;
    let user = create_plain_user(&client, &admin, &team, "plup").await;
    both(
        &client,
        rust,
        &user.token,
        MULTIPART,
        &form(Some(&probe), None),
        "a plain user",
    )
    .await;
    // A read-only admin holds sysconsole_read_plugins and not the write permission the route asks
    // for, so it is refused where a check of the wrong one would admit it.
    if common::set_user_roles(&user.id, "system_user system_read_only_admin").await {
        let reader = common::login_plain_user(&client, "plup").await;
        both(
            &client,
            rust,
            &reader,
            MULTIPART,
            &form(Some(&probe), None),
            "a read-only admin",
        )
        .await;
    }
    delete_plain_user(&client, &admin, &user.id).await;

    // The body, as `http.Error` answers it.
    both(
        &client,
        rust,
        &admin,
        "application/json",
        b"{}",
        "not multipart",
    )
    .await;
    both(
        &client,
        rust,
        &admin,
        "multipart/form-data",
        b"x",
        "no boundary",
    )
    .await;
    both(
        &client,
        rust,
        &admin,
        MULTIPART,
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"force\"\r\n\r\ntru")
            .as_bytes(),
        "cut short",
    )
    .await;
    both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(None, Some("true")),
        "no file",
    )
    .await;
    both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(Some(b"not gzip"), None),
        "not gzip",
    )
    .await;
    both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(Some(&no_manifest), None),
        "no manifest",
    )
    .await;
    both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(Some(&bad_id), None),
        "an invalid id",
    )
    .await;

    // Forced, whatever the oracle kept from the last run; then refused without force; then an
    // upgrade.
    let installed = both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(Some(&probe), Some("true")),
        "a forced install",
    )
    .await;
    assert_eq!(installed["version"], "1.0.0");
    both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(Some(&probe), Some("false")),
        "a re-install",
    )
    .await;
    let upgraded = both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(Some(&upgrade), Some("true")),
        "a forced upgrade",
    )
    .await;
    assert_eq!(upgraded["version"], "1.0.1");
    assert!(
        scratch
            .join("data/plugins/mmrs.upload.probe.tar.gz")
            .exists(),
        "the bundle was kept in the file store"
    );

    // Enabled by the configuration: running by the time the upload answers.
    both(
        &client,
        rust,
        &admin,
        MULTIPART,
        &form(Some(&enabled), Some("true")),
        "an enabled plugin",
    )
    .await;
    for path in ["/api/v4/plugins/webapp", "/api/v4/plugins"] {
        let (go_status, go_body, _) = request_raw(
            &client,
            &oracle(),
            reqwest::Method::GET,
            Some(&admin),
            path,
            None,
        )
        .await;
        let (rs_status, rs_body, _) = request_raw(
            &client,
            rust,
            reqwest::Method::GET,
            Some(&admin),
            path,
            None,
        )
        .await;
        assert_eq!((go_status, rs_status), (200, 200), "{path}");
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{path} after the uploads"
        );
        assert!(
            String::from_utf8_lossy(&rs_body).contains("mattermost-ai"),
            "{path} names the enabled plugin"
        );
    }
}

/// With `RequirePluginSignature` on, uploads are refused by the same 501 as uploads off, ahead of
/// the permission: an unsigned upload could never be verified. Go's answer is transcribed from
/// plugin.go:47-49 rather than measured; no Go server here runs with the requirement on.
#[tokio::test]
async fn a_signature_requirement_refuses_every_upload() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-upload-signed");
    let dir = scratch.join("plugins").to_string_lossy().into_owned();
    let Some(host) = SecondServer::start(
        SIGNED_PORT,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_ENABLEUPLOADS", "true"),
            ("MM_PLUGINSETTINGS_REQUIREPLUGINSIGNATURE", "true"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ],
    )
    .await
    else {
        return;
    };
    let answer = send(&client, &host.base, &admin, MULTIPART, &form(None, None)).await;
    assert_eq!(answer.status, 501);
    let error: serde_json::Value = serde_json::from_slice(&answer.body).expect("an error body");
    assert_eq!(error["id"], "app.plugin.upload_disabled.app_error");
}
