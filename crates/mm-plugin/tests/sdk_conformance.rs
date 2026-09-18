//! The Rust plugin SDK under a Go host.
//!
//! `reference/dump/plugingen host` runs Go's real `plugin.Environment`. It installs
//! `examples/conformance_plugin` as a plugin bundle, activates it, and calls every generated hook
//! with its `Z_<Hook>Args` fixture. Its API is plugintest's mock, answering every generated
//! method with its `Z_<Method>Returns` fixture. The Rust plugin, on activation, calls every
//! generated API method with its `Z_<Method>Args` fixture. Both sides write a transcript, and this
//! checks them against the oracle and against each other:
//! - activation happened in Go's order: SetAPI, then OnConfigurationChange, then the tour;
//! - what each side received as arguments is the fixture;
//! - what Go received as hook returns is the fixture, sent unchanged;
//! - what Rust received as API returns is what Go's API server sent (the fixture after
//!   `encodableError`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{Map, Value as Json};

mod common;
use common::*;

macro_rules! names {
    ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
        &[$($name),*]
    };
}
const HOOKS: &[&str] = mm_plugin::for_each_hook!(names);

/// The API methods the Rust plugin calls with their fixture arguments: everything with a
/// generated client. The log methods are checked by name; `LogAuditRec` and
/// `LogAuditRecWithLevel` have no Rust client yet (see `rpc/handwritten.rs`).
const API_CALLED: &[&str] = mm_plugin::for_each_api_call!(names);

/// The hooks whose Go client seeds its answer with the value it passed, so what it records is the
/// plugin's answer decoded into that (client_rpc.go).
const MERGING_HOOKS: [&str; 4] = [
    "MessageWillBePosted",
    "MessageWillBeUpdated",
    "ChannelMemberWillBeAdded",
    "TeamMemberWillBeAdded",
];

/// What Go's API mock must have seen from the plugin's log calls.
fn logged_args() -> Json {
    serde_json::json!({
        "A": "a logged line",
        "B": [
            {"$iface": "string", "value": "key"},
            {"$iface": "string", "value": "42"},
            {"$iface": "string", "value": "true"},
        ],
    })
}

/// What a merging hook's Go client records: the plugin's whole fixture answer, decoded into the
/// value the host passed. `MessageWillBeUpdated` replaces instead, so it keeps the fixture.
fn merged_returns(name: &str) -> Json {
    use mm_plugin::wire::plugin as w;
    match name {
        "MessageWillBePosted" => {
            let sent: w::Z_MessageWillBePostedArgs = fixture("Z_MessageWillBePostedArgs");
            merged(
                "Z_MessageWillBePostedReturns",
                w::Z_MessageWillBePostedReturns {
                    a: sent.b,
                    b: String::new(),
                },
            )
        }
        "ChannelMemberWillBeAdded" => {
            let sent: w::Z_ChannelMemberWillBeAddedArgs = fixture("Z_ChannelMemberWillBeAddedArgs");
            merged(
                "Z_ChannelMemberWillBeAddedReturns",
                w::Z_ChannelMemberWillBeAddedReturns {
                    a: sent.b,
                    b: String::new(),
                },
            )
        }
        "TeamMemberWillBeAdded" => {
            let sent: w::Z_TeamMemberWillBeAddedArgs = fixture("Z_TeamMemberWillBeAddedArgs");
            merged(
                "Z_TeamMemberWillBeAddedReturns",
                w::Z_TeamMemberWillBeAddedReturns {
                    a: sent.b,
                    b: String::new(),
                },
            )
        }
        other => panic!("{other} does not merge"),
    }
}

/// `examples/conformance_plugin`, built by this test.
///
/// `cargo test --test sdk_conformance` does not rebuild examples. A stale plugin once made a
/// correct change look broken. An mtime check cannot replace a build: cargo leaves a binary
/// untouched when a source is rewritten with its old contents, which is exactly what the mutation
/// harness does. So build it here.
fn rust_plugin() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let status = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "-p",
                    "mm-plugin",
                    "--example",
                    "conformance_plugin",
                ])
                .status()
                .unwrap();
            assert!(
                status.success(),
                "building examples/conformance_plugin failed"
            );
            let exe = std::env::current_exe().unwrap();
            let profile_dir = exe.parent().and_then(|deps| deps.parent()).unwrap();
            profile_dir.join("examples").join("conformance_plugin")
        })
        .clone()
}

type Transcript = Vec<Map<String, Json>>;

fn read_transcript(path: &Path) -> Transcript {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Every record naming `key`, by name: the last one wins.
///
/// `LoadPluginConfiguration` records a `config` rather than `args`/`returns`, so it is looked up
/// by name like the rest.
fn by_name(transcript: &[Map<String, Json>], key: &str) -> BTreeMap<String, Map<String, Json>> {
    transcript
        .iter()
        .filter_map(|e| Some((e.get(key)?.as_str()?.to_owned(), e.clone())))
        .collect()
}

/// How long the Go host may take. It is a second's work; a plugin that never answers a call
/// would otherwise hang the suite, which a mutation run proved by hanging for two hours.
const HOST_TIMEOUT: Duration = Duration::from_secs(60);

/// Install the Rust plugin as a bundle and run the Go host over it; the Go and Rust transcripts.
fn run_go_host(name: &str, env: &[(&str, &str)]) -> (Transcript, Transcript) {
    let dir = scratch(name);
    let bundle = dir.join("plugins/conformance");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(
        bundle.join("plugin.json"),
        r#"{"id": "conformance", "name": "Conformance", "version": "0.0.1", "server": {"executable": "plugin"}}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(rust_plugin(), bundle.join("plugin")).unwrap();
    let go_transcript = dir.join("go.jsonl");
    let rust_transcript = dir.join("rust.jsonl");
    let output = dir.join("host.log");

    let mut cmd = Command::new(plugingen());
    cmd.arg("host")
        .arg(oracle_dir())
        .arg(dir.join("plugins"))
        .arg("conformance")
        .current_dir(root().join("reference/dump"))
        .env("TZ", "Asia/Kolkata")
        .env("PLUGINGEN_TRANSCRIPT", &go_transcript)
        // Inherited by the plugin process, which reads the same oracle.
        .env("MM_PLUGIN_GOB_DIR", oracle_dir())
        // Inherited by the plugin process, which the host launches.
        .env("CONFORMANCE_TRANSCRIPT", &rust_transcript);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // To a file, not a pipe: the host and the plugin both log, and a full pipe would deadlock
    // exactly like the missing timeout did.
    let log = std::fs::File::create(&output).unwrap();
    cmd.stdout(log.try_clone().unwrap()).stderr(log);

    let mut child = cmd.spawn().unwrap();
    let deadline = std::time::Instant::now() + HOST_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            // The plugin is the host's child, and dies with it.
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the Go host did not finish within {HOST_TIMEOUT:?}; its output:\n{}",
                std::fs::read_to_string(&output).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "plugingen host failed:\n{}",
        std::fs::read_to_string(&output).unwrap_or_default()
    );
    (
        read_transcript(&go_transcript),
        read_transcript(&rust_transcript),
    )
}

#[test]
fn sdk_rust_plugin_runs_under_the_go_environment() {
    let expected = expected();
    let (go, rust) = run_go_host("sdk-go-host", &[]);
    let mut failures = Vec::new();

    // Go activated the plugin and shut it down.
    let activation = go.iter().find(|e| e.contains_key("activated")).unwrap();
    assert_eq!(
        activation.get("activated"),
        Some(&Json::Bool(true)),
        "{activation:?}"
    );
    assert!(go.iter().any(|e| e.contains_key("shutdown")));

    // The plugin's activation ran in Go's order, and finished before any hook the host called.
    let position = |pred: &dyn Fn(&Map<String, Json>) -> bool| rust.iter().position(pred);
    let set_api = position(&|e| e.contains_key("set_api")).expect("SetAPI never ran");
    let config = position(&|e| e.get("hook") == Some(&Json::from("OnConfigurationChange")))
        .expect("OnConfigurationChange never ran");
    let first_api = position(&|e| e.contains_key("api")).expect("the API tour never ran");
    let activated = position(&|e| e.contains_key("activated")).expect("OnActivate never finished");
    assert!(
        set_api < config && config < first_api && first_api < activated,
        "{rust:#?}"
    );

    let go_api = by_name(&go, "api");
    let rust_api = by_name(&rust, "api");
    for name in API_CALLED {
        let want_args = &expected[&format!("Z_{name}Args")];
        match go_api.get(*name) {
            None => failures.push(format!("api {name}: the Go host never saw the call")),
            Some(e) => {
                if &e["args"] != want_args {
                    failures.push(format!(
                        "api {name}: Go received\n{}\nexpected\n{want_args}",
                        e["args"]
                    ));
                }
                match rust_api.get(*name) {
                    Some(r) if r["returns"] == e["returns"] => {}
                    Some(r) => failures.push(format!(
                        "api {name}: Rust received\n{}\nGo sent\n{}",
                        r["returns"], e["returns"]
                    )),
                    None => {
                        failures.push(format!("api {name}: the Rust plugin recorded no returns"))
                    }
                }
            }
        }
    }

    // The host's hook calls come after activation; Go's own OnConfigurationChange call is earlier.
    let rust_hooks = by_name(&rust[activated..], "hook");
    let go_hooks = by_name(&go, "hook");
    for name in HOOKS {
        let want_args = &expected[&format!("Z_{name}Args")];
        match rust_hooks.get(*name) {
            Some(e) if &e["args"] == want_args => {}
            Some(e) => failures.push(format!(
                "hook {name}: Rust received\n{}\nexpected\n{want_args}",
                e["args"]
            )),
            None => failures.push(format!("hook {name}: the Rust plugin never saw the call")),
        }
        // A merging hook's client decodes the plugin's answer into the value it passed;
        // MessageWillBeUpdated takes the answer as it is.
        let want_returns = if MERGING_HOOKS.contains(name) && *name != "MessageWillBeUpdated" {
            merged_returns(name)
        } else {
            expected[&format!("Z_{name}Returns")].clone()
        };
        match go_hooks.get(*name) {
            Some(e) if e["returns"] == want_returns => {}
            Some(e) => failures.push(format!(
                "hook {name}: Go received\n{}\nexpected\n{want_returns}",
                e["returns"]
            )),
            None => failures.push(format!("hook {name}: the Go host recorded no returns")),
        }
    }

    // The API methods whose clients are hand-written.
    for name in ["LogDebug", "LogInfo", "LogWarn", "LogError"] {
        match go_api.get(name) {
            Some(e) if e["args"] == logged_args() => {}
            other => failures.push(format!("api {name}: Go received {other:?}")),
        }
    }
    // The Go host asked the Rust plugin to serve HTTP: it read the body over one connection and
    // answered over the other.
    let want_response = http_response("POST", CONFORMANCE_URL, &stream_payload());
    let served = by_name(&go, "http");
    for name in ["ServeHTTP", "ServeMetrics"] {
        match served.get(name) {
            Some(e) => {
                let got = serde_json::json!({
                    "status": e["status"],
                    "header": e["header"],
                    "body": e["body"],
                });
                assert_eq!(got, want_response, "{name}: what Go's writer received");
            }
            None => failures.push(format!("{name}: the Go host recorded no response")),
        }
        match rust_hooks.get(name) {
            Some(e) => {
                assert_eq!(
                    e["stream"],
                    stream_digest(&stream_payload()),
                    "{name}: the request body the plugin read"
                );
                assert_eq!(
                    e["args"],
                    render_typed(&http_request()),
                    "{name}: the request the plugin received"
                );
            }
            None => failures.push(format!("{name}: the Rust plugin never saw the call")),
        }
    }

    // The hijack scenario (plugingen/hijack.go) under Go's real server: a recorder refuses, and
    // the client of a real connection receives exactly what the script sends through Go's
    // buffers.
    match served.get("hijack-recorder") {
        Some(e) => {
            let got = serde_json::json!({
                "status": e["status"],
                "header": e["header"],
                "body": e["body"],
            });
            assert_eq!(got, hijack_refused(), "what Go's recorder received");
        }
        None => failures.push("hijack: the Go host recorded no recorder answer".into()),
    }
    match served.get("hijack") {
        Some(e) => {
            assert_eq!(e.get("error"), None, "the Go client failed");
            assert_eq!(
                e["received"],
                hijack_received(),
                "what Go's client received"
            );
        }
        None => failures.push("hijack: the Go host recorded no client".into()),
    }
    assert_eq!(
        rust_hooks.get("hijack").cloned().map(Json::Object),
        Some(hijack_recorded()),
        "what the Rust plugin saw of the hijacked connection"
    );

    // The database: the Go host answered the plugin's four questions, and the sentinel it sent
    // was still a sentinel when the plugin read it.
    let asked: Vec<&Json> = go.iter().filter_map(|e| e.get("driver")).collect();
    assert_eq!(
        asked,
        [
            &Json::from("Conn"),
            &Json::from("ConnPing"),
            &Json::from("ConnQuery"),
            &Json::from("RowsColumns")
        ],
        "the plugin's database calls, in order"
    );
    let query = go
        .iter()
        .find(|e| e.get("driver") == Some(&Json::from("ConnQuery")))
        .expect("the query");
    assert_eq!(query["query"], Json::from("SELECT 1"));
    assert_eq!(query["args"], serde_json::json!(["one=1"]));

    let tour = rust
        .iter()
        .find(|e| e.get("driver") == Some(&Json::from("tour")))
        .expect("the Rust plugin's driver tour");
    assert_eq!(tour["conn"], Json::from("conn-1"));
    assert_eq!(tour["rows"], Json::from("rows-1"));
    assert_eq!(tour["columns"], serde_json::json!(["id", "name"]));
    assert_eq!(tour["conn_error"], serde_json::json!({"$iface": ""}));
    // `driver.ErrBadConn` crossed as an ErrorString with Go's code 6.
    assert_eq!(
        tour["ping_error"],
        serde_json::json!({
            "$iface": "*plugin.ErrorString",
            "value": {"Code": 6, "Err": "driver: bad connection"},
        }),
        "the sentinel the host sent"
    );

    // The Go host lent the plugin a file and a writer: the replacement came back whole, and the
    // hook answered with the info it was given.
    let file = go
        .iter()
        .find(|e| e.get("hook") == Some(&Json::from("FileWillBeUploaded")))
        .expect("the Go host did not call FileWillBeUploaded");
    assert_eq!(
        file["replacement"],
        Json::String(replacement_file(&stream_payload())),
        "the replacement file"
    );
    assert_eq!(file["info_id"], Json::from("fileinfo"));
    assert_eq!(file["rejection"], Json::from(""));
    assert_eq!(
        rust_hooks["FileWillBeUploaded"]["stream"],
        stream_digest(&stream_payload()),
        "the file the plugin read"
    );

    // The plugin's outward HTTP call reached the host, and its answer came back whole.
    let (status, header, body) = outward_response();
    let outward = rust_api
        .get("PluginHTTP")
        .expect("the Rust plugin made no outward HTTP call");
    assert_eq!(outward["status"], status);
    assert_eq!(outward["header"], serde_json::json!(header));
    assert_eq!(outward["body"], String::from_utf8_lossy(&body).as_ref());
    assert_eq!(
        go_api.get("PluginHTTP").map(|e| &e["stream"]),
        Some(&stream_digest(&stream_payload())),
        "the request body the Go host read"
    );

    // Each stream the Rust plugin lent arrived whole at the Go host, and it answered with the
    // method's fixture.
    let want_stream = stream_digest(&stream_payload());
    for name in [
        "UploadData",
        "InstallPlugin",
        "ReceiveSharedChannelAttachmentSyncMsg",
    ] {
        match go_api.get(name).map(|e| &e["stream"]) {
            Some(got) if got == &want_stream => {}
            other => failures.push(format!("api {name}: Go read {other:?}")),
        }
        let want_returns = &expected[&format!("Z_{name}Returns")];
        match rust_api.get(name).map(|e| &e["returns"]) {
            Some(got) if got == want_returns => {}
            other => failures.push(format!("api {name}: the Rust plugin recorded {other:?}")),
        }
    }

    // The Rust plugin's audit record reached Go in its gob-safe form.
    for name in ["LogAuditRec", "LogAuditRecWithLevel"] {
        let want = &expected[&format!("Z_{name}Args.safe")];
        match go_api.get(name) {
            Some(e) if &e["args"] == want => {}
            Some(e) => failures.push(format!(
                "api {name}: Go received\n{}\nexpected\n{want}",
                e["args"]
            )),
            None => failures.push(format!("api {name}: the Go host never saw the call")),
        }
    }

    // The host answered LoadPluginConfiguration with a value; the plugin must have it as JSON.
    let config = serde_json::json!({"enabled": true, "name": "conformance"});
    assert_eq!(
        go_api.get("LoadPluginConfiguration").map(|e| &e["config"]),
        Some(&config)
    );
    assert_eq!(
        rust_api
            .get("LoadPluginConfiguration")
            .map(|e| &e["config"]),
        Some(&config)
    );
    report(&failures);
}

/// An error from `OnActivate` refuses activation, and Go reports it as the plugin's error.
#[test]
fn sdk_an_on_activate_error_refuses_activation() {
    let (go, rust) = run_go_host(
        "sdk-go-host-refused",
        &[("CONFORMANCE_REFUSE_ACTIVATION", "1")],
    );
    let activation = go.iter().find(|e| e.contains_key("activated")).unwrap();
    assert_eq!(
        activation.get("activated"),
        Some(&Json::Bool(false)),
        "{activation:?}"
    );
    let error = activation
        .get("error")
        .and_then(Json::as_str)
        .unwrap_or_default();
    // model.AppError.Error(): "<Where>: <Message>".
    assert!(
        error.starts_with("conformance.OnActivate: activation refused"),
        "{error:?}"
    );
    assert!(rust.iter().any(|e| e.contains_key("refused")));
    assert!(
        !rust.iter().any(|e| e.contains_key("api")),
        "a refused plugin toured the API"
    );
    assert!(
        !go.iter().any(|e| e.contains_key("hook")),
        "Go called hooks on a refused plugin"
    );
}
