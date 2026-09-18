//! The plugin environment against Go's (environment.go, supervisor.go).
//!
//! One bundle tree, built here, holds every shape `Activate` branches on: a server plugin, one
//! that refuses `OnActivate`, webapp-only bundles (one read from `plugin.yaml`), and each way a
//! bundle can fail to start. `reference/dump/plugingen env` runs a fixed script over Go's
//! `plugin.Environment`; [`script`] runs the same one over [`Environment`]; the two transcripts
//! must be equal, and so must what the plugins themselves logged.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use mm_plugin::environment::Environment;
use mm_plugin::rpc::{PluginApi, PluginApiHttp, PluginApiStreams};
use serde_json::{Value as Json, json};

mod common;
use common::*;

/// The API a plugin gets: nothing implemented, as the plugins here call nothing.
struct NoApi;
impl PluginApi for NoApi {}
impl PluginApiStreams for NoApi {}
impl PluginApiHttp for NoApi {}

struct NoDriver;
impl mm_plugin::rpc::Driver for NoDriver {}

/// `examples/env_plugin`, built by this test for the same reason `sdk_conformance` builds its
/// plugin: `cargo test` does not rebuild examples.
fn env_plugin() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "mm-plugin", "--example", "env_plugin"])
                .status()
                .unwrap();
            assert!(status.success(), "building examples/env_plugin failed");
            let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/debug/examples/env_plugin");
            assert!(exe.exists(), "{exe:?}");
            exe
        })
        .clone()
}

fn put(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn executable(bundle: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let to = bundle.join("server").join(name);
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(env_plugin(), &to).unwrap();
    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every bundle the script activates, plus the entries `Available` must skip.
fn build_root(name: &str) -> PathBuf {
    let root = scratch(name);
    let plugins = root.join("plugins");
    let manifest = |dir: &str, json: &str| put(&plugins.join(dir).join("plugin.json"), json);

    manifest(
        "ok",
        r#"{"id":"ok","name":"OK","description":"runs","version":"1.0.0","server":{"executable":"server/env_plugin"}}"#,
    );
    executable(&plugins.join("ok"), "env_plugin");
    manifest(
        "refuse",
        r#"{"id":"refuse","name":"Refuses","version":"1.0.0","server":{"executable":"server/env_plugin_refuse"}}"#,
    );
    executable(&plugins.join("refuse"), "env_plugin_refuse");

    manifest(
        "webapp",
        r#"{"id":"WebApp","name":"Web App","version":"0.1.0","webapp":{"bundle_path":"webapp/dist/main.js"}}"#,
    );
    put(
        &plugins.join("webapp/webapp/dist/main.js"),
        "console.log('webapp');\n",
    );
    put(&plugins.join("webapp/webapp/dist/extra.css"), "body {}\n");

    // plugin.yaml wins over plugin.json, and its id is lowercased like JSON's.
    put(
        &plugins.join("yaml/plugin.yaml"),
        "id: YamlPlugin\nname: From YAML\ndescription: read from plugin.yaml\nversion: 2.0.0\nwebapp:\n  bundle_path: dist/main.js\n",
    );
    manifest("yaml", r#"{"id":"shadowed","name":"never read"}"#);
    put(&plugins.join("yaml/dist/main.js"), "console.log('yaml');\n");

    manifest(
        "nocomponent",
        r#"{"id":"nocomponent","name":"None","version":"1.0.0"}"#,
    );
    for (dir, version) in [("minversion", "99.0.0"), ("badminversion", "not-a-version")] {
        manifest(
            dir,
            &format!(
                r#"{{"id":"{dir}","version":"1.0.0","min_server_version":"{version}","webapp":{{"bundle_path":"main.js"}}}}"#
            ),
        );
        put(&plugins.join(dir).join("main.js"), "//\n");
    }
    manifest(
        "missingexe",
        r#"{"id":"missingexe","server":{"executable":"server/nothere"}}"#,
    );
    manifest(
        "escape",
        r#"{"id":"escape","server":{"executable":"../../outside"}}"#,
    );
    manifest(
        "noarch",
        r#"{"id":"noarch","server":{"executables":{"windows-amd64":"server/x.exe"}}}"#,
    );
    manifest(
        "dotpath",
        r#"{"id":"dotpath","webapp":{"bundle_path":"../main.js"}}"#,
    );
    manifest("badjson", "{not json");
    for dir in ["dup1", "dup2"] {
        manifest(dir, r#"{"id":"dup","webapp":{"bundle_path":"main.js"}}"#);
        put(&plugins.join(dir).join("main.js"), "//\n");
    }
    manifest(
        ".hidden",
        r#"{"id":"hidden","webapp":{"bundle_path":"main.js"}}"#,
    );
    std::fs::create_dir_all(plugins.join("nomanifest")).unwrap();
    put(&plugins.join("stray.txt"), "not a bundle\n");
    std::fs::create_dir_all(root.join("webapp")).unwrap();
    root
}

/// What each plugin process logged, sorted: the two environments may interleave them.
fn plugin_log(path: &Path) -> Vec<String> {
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn go_transcript() -> (Json, Vec<String>) {
    let root = build_root("environment-go");
    let log = root.join("plugin.log");
    let out = Command::new(plugingen())
        .arg("env")
        .arg(&root)
        .current_dir(root_dir())
        .env("ENV_PLUGIN_LOG", &log)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plugingen env: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        serde_json::from_slice(&out.stdout).unwrap(),
        plugin_log(&log),
    )
}

fn root_dir() -> PathBuf {
    root().join("reference/dump")
}

fn err_string<E: std::fmt::Display>(r: Result<(), E>) -> String {
    r.err().map(|e| e.to_string()).unwrap_or_default()
}

/// plugingen/env.go's `runEnv`, over the Rust environment.
async fn script(root: &Path) -> Json {
    let plugin_dir = root.join("plugins");
    let webapp_dir = root.join("webapp");
    let env = Environment::new(
        Box::new(|_| Arc::new(NoApi)),
        Arc::new(NoDriver),
        plugin_dir,
        webapp_dir.clone(),
    );
    let mut steps = Vec::new();
    let statuses = |env: &Environment<NoApi, NoDriver>, label: &str| {
        let (st, err) = match env.statuses() {
            Ok(st) => (serde_json::to_value(st).unwrap(), String::new()),
            Err(e) => (Json::Null, e.to_string()),
        };
        json!({"step": "statuses", "label": label, "statuses": st, "error": err})
    };

    let (ids, err) = match env.available() {
        Ok(infos) => (
            infos
                .iter()
                .map(|i| format!("{} {}", i.manifest.as_ref().unwrap().id, i.path))
                .collect::<Vec<_>>(),
            String::new(),
        ),
        Err(e) => (Vec::new(), e.to_string()),
    };
    steps.push(json!({"step": "available", "ids": ids, "error": err}));

    for id in [
        "ok",
        "ok",
        "refuse",
        "webapp",
        "yamlplugin",
        "nocomponent",
        "minversion",
        "badminversion",
        "missingexe",
        "escape",
        "noarch",
        "dotpath",
        "badjson",
        "dup",
        "hidden",
        "absent",
    ] {
        let mut entry = json!({"step": "activate", "id": id});
        match env.activate(id).await {
            Ok(Some(manifest)) => {
                entry["activated"] = json!(true);
                entry["error"] = json!("");
                entry["manifest"] = json!(manifest.id);
            }
            Ok(None) => {
                entry["activated"] = json!(false);
                entry["error"] = json!("");
            }
            Err(e) => {
                entry["activated"] = json!(false);
                entry["error"] = json!(e.to_string());
            }
        }
        steps.push(entry);
    }
    steps.push(statuses(&env, "after activation"));

    let mut files = Vec::new();
    let mut stack = vec![webapp_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(
                    path.strip_prefix(&webapp_dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    files.sort();
    steps.push(json!({"step": "webapp", "files": files}));

    let mut hooks = serde_json::Map::new();
    for id in ["ok", "webapp", "refuse", "absent"] {
        hooks.insert(
            id.into(),
            json!(err_string(env.hooks_for_plugin(id).map(drop))),
        );
    }
    let mut active: Vec<String> = env
        .active()
        .into_iter()
        .filter_map(|i| i.manifest.map(|m| m.id))
        .collect();
    active.sort();
    let public = env.public_files_path("ok");
    let manifest = env.get_manifest("webapp");
    steps.push(json!({
        "step": "lookups", "hooks": hooks, "active": active,
        "public": public.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        "public_error": err_string(public.map(drop)),
        "public_refused": err_string(env.public_files_path("refuse").map(drop)),
        "manifest": manifest.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
        "manifest_error": err_string(manifest.map(drop)),
        "manifest_absent": err_string(env.get_manifest("absent").map(drop)),
        "is_active": {"ok": env.is_active("ok"), "refuse": env.is_active("refuse")},
        "state": {
            "ok": env.get_plugin_state("ok"),
            "refuse": env.get_plugin_state("refuse"),
            "absent": env.get_plugin_state("absent"),
        },
    }));

    // plugingen/env.go's EnvOkManifestV2: a new version on disk before the restart.
    std::fs::write(
        root.join("plugins/ok/plugin.json"),
        r#"{"id":"ok","name":"OK","description":"runs","version":"1.0.1","server":{"executable":"server/env_plugin"}}"#,
    )
    .unwrap();
    let ok = env.deactivate("ok").await;
    let ok_again = env.deactivate("ok").await;
    let webapp = env.deactivate("webapp").await;
    let refuse = env.deactivate("refuse").await;
    let absent = env.deactivate("absent").await;
    let is_active = env.is_active("ok");
    let hooks_ok = err_string(env.hooks_for_plugin("ok").map(drop));
    let restart_ok = err_string(env.restart_plugin("ok").await);
    steps.push(json!({
        "step": "deactivate", "ok": ok, "ok_again": ok_again, "webapp": webapp,
        "refuse": refuse, "absent": absent, "is_active": is_active, "hooks_ok": hooks_ok,
        "restart_ok": restart_ok,
    }));
    steps.push(statuses(&env, "after deactivation and restart"));
    let mut versions: Vec<String> = env
        .active()
        .into_iter()
        .filter_map(|i| i.manifest.map(|m| format!("{} {}", m.id, m.version)))
        .collect();
    versions.sort();
    steps.push(json!({"step": "active after restart", "active": versions}));

    env.remove_plugin("refuse");
    steps.push(statuses(&env, "after removing refuse"));

    env.shutdown().await;
    steps.push(statuses(&env, "after shutdown"));

    let text = serde_json::to_string(&steps)
        .unwrap()
        .replace(&root.to_string_lossy().into_owned(), "$ROOT");
    serde_json::from_str(&text).unwrap()
}

#[test]
fn environment_matches_go_step_for_step() {
    let (go, go_log) = go_transcript();

    let root = build_root("environment-rust");
    let log = root.join("plugin.log");
    // SAFETY: set before the runtime starts, and nothing else in this test binary reads it.
    unsafe { std::env::set_var("ENV_PLUGIN_LOG", &log) };
    let rust = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(script(&root));

    let (go_steps, rust_steps) = (go.as_array().unwrap(), rust.as_array().unwrap());
    assert_eq!(go_steps.len(), rust_steps.len(), "step count");
    let mut failures = Vec::new();
    for (i, (g, r)) in go_steps.iter().zip(rust_steps).enumerate() {
        if g != r {
            failures.push(format!(
                "step {i}:\nGo:   {}\nRust: {}",
                serde_json::to_string_pretty(g).unwrap(),
                serde_json::to_string_pretty(r).unwrap()
            ));
        }
    }
    report(&failures);
    assert_eq!(plugin_log(&log), go_log, "what the plugins logged");
}
