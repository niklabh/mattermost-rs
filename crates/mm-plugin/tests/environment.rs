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
impl mm_plugin::rpc::PluginApiDynamic for NoApi {}

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

/// Held by each test that points `ENV_PLUGIN_LOG` at its own log for the plugins it launches.
static PLUGIN_LOG: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _log = PLUGIN_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: set before the runtime starts, while holding `PLUGIN_LOG`, which every writer of
    // the variable holds; the plugins this environment launches inherit it.
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

/// plugingen/reattach.go's `ReattachManifests`.
fn reattach_manifest(name: &str) -> mm_model::manifest::Manifest {
    let json = match name {
        "webonly" => r#"{"id":"webonly","version":"1.0.0","webapp":{"bundle_path":"main.js"}}"#,
        "minversion" => {
            r#"{"id":"reminv","version":"1.0.0","min_server_version":"99.0.0","server":{"executable":"x"}}"#
        }
        "dead" => {
            r#"{"id":"dead","version":"1.0.0","server":{"executable":"x"},"webapp":{"bundle_path":"main.js"}}"#
        }
        "ok" => {
            r#"{"id":"ok","name":"OK","version":"1.0.0","server":{"executable":"server/env_plugin"}}"#
        }
        "refuse" => {
            r#"{"id":"refuse","name":"Refuses","version":"1.0.0","server":{"executable":"server/env_plugin_refuse"}}"#
        }
        other => panic!("no manifest {other}"),
    };
    serde_json::from_str(json).unwrap()
}

/// plugingen/reattach.go's `launch`: start the executable as a plugin ourselves, as a developer's
/// tooling would, and hand the environment only its reattach configuration.
async fn launch(
    path: &Path,
    log: &Path,
) -> (
    goplugin::Client,
    mm_model::plugin_reattach::PluginReattachConfig,
) {
    let mut config = goplugin::ClientConfig::new(mm_plugin::rpc::handshake());
    // The log path goes on the command rather than into this process's environment, which
    // `environment_matches_go_step_for_step` sets for itself on another thread.
    let mut command = goplugin::PluginCommand::new(path);
    command
        .env
        .push(("ENV_PLUGIN_LOG".into(), log.as_os_str().to_owned()));
    config.cmd = Some(command);
    let client = goplugin::Client::start(config).await.unwrap();
    let r = client.reattach_config();
    let reattach = mm_model::plugin_reattach::PluginReattachConfig {
        protocol: r.protocol,
        protocol_version: 1,
        addr: mm_model::plugin_reattach::UnixAddr {
            name: r.addr.to_string(),
            net: r.addr.network().to_owned(),
        },
        pid: i64::from(r.pid),
        test: r.test,
    };
    (client, reattach)
}

/// plugingen/reattach.go's `runReattach`, over the Rust environment.
async fn reattach_script(root: &Path, log: &Path) -> Json {
    let plugin_dir = root.join("plugins");
    let env = Environment::new(
        Box::new(|_| Arc::new(NoApi)),
        Arc::new(NoDriver),
        plugin_dir.clone(),
        root.join("webapp"),
    );

    let mut gone = Command::new("true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    let dead = mm_model::plugin_reattach::PluginReattachConfig {
        protocol: "netrpc".into(),
        protocol_version: 1,
        addr: mm_model::plugin_reattach::UnixAddr {
            name: root.join("nobody.sock").to_string_lossy().into_owned(),
            net: "unix".into(),
        },
        pid: i64::from(gone_pid),
        test: false,
    };
    let (ok_client, ok_config) = launch(&plugin_dir.join("ok/server/env_plugin"), log).await;
    let (refuse_client, refuse_config) =
        launch(&plugin_dir.join("refuse/server/env_plugin_refuse"), log).await;

    let ids = ["webonly", "reminv", "dead", "ok", "refuse"];
    let observe = |env: &Environment<NoApi, NoDriver>, label: &str| {
        let mut state = serde_json::Map::new();
        let mut hooks = serde_json::Map::new();
        for id in ids {
            state.insert(id.into(), json!(env.get_plugin_state(id)));
            hooks.insert(
                id.into(),
                json!(err_string(env.hooks_for_plugin(id).map(drop))),
            );
        }
        let mut active: Vec<String> = env
            .active()
            .into_iter()
            .map(|i| format!("{} {}", i.manifest.unwrap().id, i.path))
            .collect();
        active.sort();
        let (errs, statuses_error) = match env.statuses() {
            Ok(st) => (
                st.0.iter()
                    .filter(|s| s.plugin_id == "ok" || s.plugin_id == "refuse")
                    .map(|s| (s.plugin_id.clone(), json!(s.error)))
                    .collect::<serde_json::Map<_, _>>(),
                String::new(),
            ),
            Err(e) => (serde_json::Map::new(), e.to_string()),
        };
        json!({"step": "observe", "label": label, "state": state, "hooks": hooks,
            "active": active, "status_errors": errs, "statuses_error": statuses_error})
    };

    let mut steps = Vec::new();
    for (name, config) in [
        ("webonly", &dead),
        ("minversion", &dead),
        ("dead", &dead),
        ("ok", &ok_config),
        ("ok", &ok_config),
        ("refuse", &refuse_config),
    ] {
        let error = err_string(env.reattach(&reattach_manifest(name), config).await);
        steps.push(json!({"step": "reattach", "name": name, "error": error}));
    }
    steps.push(observe(&env, "after reattaching"));

    let mut deactivated = serde_json::Map::new();
    for id in ids {
        deactivated.insert(id.into(), json!(env.deactivate(id).await));
    }
    steps.push(json!({"step": "deactivate", "deactivated": deactivated}));
    steps.push(observe(&env, "after deactivating"));

    for id in ids {
        env.remove_plugin(id);
    }
    steps.push(observe(&env, "after removing"));
    env.shutdown().await;
    ok_client.kill().await;
    refuse_client.kill().await;

    let text = serde_json::to_string(&steps)
        .unwrap()
        .replace(&root.to_string_lossy().into_owned(), "$ROOT");
    serde_json::from_str(&text).unwrap()
}

/// `Environment.Reattach` against Go's, step for step: the one error (no server component), the
/// failures it swallows and marks running (a version check, a process that is gone, a plugin
/// that refuses `OnActivate`), a real reattach whose hooks answer, a second one that is a no-op,
/// and what `Deactivate` and `RemovePlugin` make of each.
#[test]
fn reattach_matches_go_step_for_step() {
    let go_root = build_root("reattach-go");
    let go_log = go_root.join("plugin.log");
    let out = Command::new(plugingen())
        .arg("reattach")
        .arg(&go_root)
        .current_dir(root_dir())
        .env("ENV_PLUGIN_LOG", &go_log)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plugingen reattach: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let go: Json = serde_json::from_slice(&out.stdout).unwrap();

    let root = build_root("reattach-rust");
    let log = root.join("plugin.log");
    let rust = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(reattach_script(&root, &log));

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
    assert_eq!(
        plugin_log(&log),
        plugin_log(&go_log),
        "what the plugins logged"
    );
}

/// The two bundles `plugingen health` checks: `ok`, and `crashy`, which exits on request.
fn build_health_root(name: &str) -> PathBuf {
    let root = scratch(name);
    let plugins = root.join("plugins");
    put(
        &plugins.join("ok/plugin.json"),
        r#"{"id":"ok","name":"OK","version":"1.0.0","server":{"executable":"server/env_plugin"}}"#,
    );
    executable(&plugins.join("ok"), "env_plugin");
    put(
        &plugins.join("crashy/plugin.json"),
        r#"{"id":"crashy","name":"Crashes","version":"2.0.0","server":{"executable":"server/env_plugin_crashy"}}"#,
    );
    executable(&plugins.join("crashy"), "env_plugin_crashy");
    std::fs::create_dir_all(root.join("webapp")).unwrap();
    root
}

/// plugingen/health.go's `HealthIDs`.
const HEALTH_IDS: [&str; 3] = ["ok", "crashy", "absent"];

type HealthEnv = Environment<NoApi, NoDriver>;

async fn health(env: &HealthEnv) -> Json {
    let mut out = serde_json::Map::new();
    for id in HEALTH_IDS {
        out.insert(
            id.into(),
            json!(err_string(env.perform_health_check(id).await)),
        );
    }
    Json::Object(out)
}

fn health_observe(env: &HealthEnv, label: &str) -> Json {
    let (statuses, statuses_error) = match env.statuses() {
        Ok(st) => (
            st.0.iter()
                .map(|s| {
                    (
                        s.plugin_id.clone(),
                        json!({"state": s.state, "error": s.error, "version": s.version}),
                    )
                })
                .collect::<serde_json::Map<_, _>>(),
            String::new(),
        ),
        Err(e) => (serde_json::Map::new(), e.to_string()),
    };
    let mut active: Vec<String> = env
        .active()
        .into_iter()
        .filter_map(|i| i.manifest.map(|m| m.id))
        .collect();
    active.sort();
    let mut hooks = serde_json::Map::new();
    for id in HEALTH_IDS {
        hooks.insert(
            id.into(),
            json!(err_string(env.hooks_for_plugin(id).map(drop))),
        );
    }
    json!({"step": "observe", "label": label, "statuses": statuses,
        "statuses_error": statuses_error, "active": active, "hooks": hooks,
        "job": env.health_check_job().is_some()})
}

/// plugingen/health.go's `crash`: ask `crashy` to exit, then wait until it stops answering.
async fn crash(env: &HealthEnv, request: &Path, label: &str) -> Json {
    std::fs::write(request, b"").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while request.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "crashy never took the crash request"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    while env.perform_health_check("crashy").await.is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "crashy still answers after crashing"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    json!({"step": "crash", "label": label, "health": health(env).await})
}

/// plugingen/health.go's `runHealth`, over the Rust environment.
async fn health_script(root: &Path, log: &Path) -> Json {
    let plugin_dir = root.join("plugins");
    let env = Arc::new(Environment::new(
        Box::new(|_| Arc::new(NoApi)),
        Arc::new(NoDriver),
        plugin_dir.clone(),
        root.join("webapp"),
    ));
    let mut request = log.as_os_str().to_owned();
    request.push(".crash");
    let request = PathBuf::from(request);
    let mut steps = Vec::new();

    let before = env.health_check_job().is_some();
    env.toggle_plugin_health_check_job(true).await;
    let job = env.health_check_job().unwrap();
    env.toggle_plugin_health_check_job(true).await;
    let same = env
        .health_check_job()
        .is_some_and(|j| Arc::ptr_eq(&j, &job));
    steps.push(json!({"step": "toggle on", "before": before, "on": true, "same": same}));

    for id in HEALTH_IDS {
        let (activated, error) = match env.activate(id).await {
            Ok(m) => (m.is_some(), String::new()),
            Err(e) => (false, e.to_string()),
        };
        steps.push(json!({"step": "activate", "id": id, "activated": activated, "error": error}));
    }
    steps.push(health_observe(&env, "activated"));

    for id in HEALTH_IDS {
        job.check_plugin(id).await;
    }
    steps.push(json!({"step": "healthy", "health": health(&env).await}));
    steps.push(health_observe(&env, "after checking healthy plugins"));

    for label in ["first", "second", "third"] {
        steps.push(crash(&env, &request, label).await);
        job.check_plugin("crashy").await;
        job.check_plugin("ok").await;
        steps.push(health_observe(&env, &format!("after the {label} failure")));
    }

    job.check_plugin("crashy").await;
    steps.push(health_observe(&env, "a check after deactivation"));

    steps.push(crash(&env, &request, "before a failed restart").await);
    let exe = plugin_dir.join("crashy/server/env_plugin_crashy");
    let away = plugin_dir.join("crashy/server/env_plugin_crashy.away");
    std::fs::rename(&exe, &away).unwrap();
    job.check_plugin("crashy").await;
    steps.push(health_observe(&env, "after a failed restart"));
    job.check_plugin("crashy").await;
    steps.push(json!({"step": "no supervisor", "health": health(&env).await}));
    steps.push(health_observe(&env, "a check with no supervisor"));
    std::fs::rename(&away, &exe).unwrap();

    env.toggle_plugin_health_check_job(false).await;
    steps.push(json!({"step": "toggle off", "job": env.health_check_job().is_some()}));
    env.toggle_plugin_health_check_job(true).await;
    let again = env.health_check_job();
    steps.push(json!({"step": "toggle on again", "job": again.is_some(),
        "new": again.is_some_and(|j| !Arc::ptr_eq(&j, &job))}));

    env.shutdown().await;
    steps.push(health_observe(&env, "after shutdown"));

    let text = serde_json::to_string(&steps)
        .unwrap()
        .replace(&root.to_string_lossy().into_owned(), "$ROOT");
    serde_json::from_str(&text).unwrap()
}

/// The health check against Go's, step for step: healthy plugins are left alone; a plugin that
/// stops answering is restarted twice and deactivated on its third failure inside the hour, into
/// state 4 with its failures forgotten; its dead supervisor still answers a direct check (a new
/// first failure, so a restart); a restart that fails leaves it failed to start with no supervisor,
/// which passes every check; the toggle starts one job and stops it; and `Shutdown` stops it too.
#[test]
fn health_check_matches_go_step_for_step() {
    let go_root = build_health_root("health-go");
    let go_log = go_root.join("plugin.log");
    let out = Command::new(plugingen())
        .arg("health")
        .arg(&go_root)
        .current_dir(root_dir())
        .env("ENV_PLUGIN_LOG", &go_log)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plugingen health: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let go: Json = serde_json::from_slice(&out.stdout).unwrap();

    let root = build_health_root("health-rust");
    let log = root.join("plugin.log");
    // The plugins the environment launches read the log path, and so their crash request, from
    // this process's environment, which `environment_matches_go_step_for_step` sets too.
    let _log = PLUGIN_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: set while holding `PLUGIN_LOG`, which every writer of the variable holds.
    unsafe { std::env::set_var("ENV_PLUGIN_LOG", &log) };
    let rust = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(health_script(&root, &log));

    let (go_steps, rust_steps) = (go.as_array().unwrap(), rust.as_array().unwrap());
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
    assert_eq!(go_steps.len(), rust_steps.len(), "step count");
    assert_eq!(
        plugin_log(&log),
        plugin_log(&go_log),
        "what the plugins logged"
    );
}

/// The job's own loop (health_check.go, `run`), which the Go comparison cannot wait thirty seconds
/// for: on each tick it checks every running plugin, so a crashed one is restarted without anyone
/// asking, and a plugin that is not running is not checked at all.
#[test]
fn the_health_check_job_checks_running_plugins_on_each_tick() {
    let root = build_health_root("health-loop");
    let log = root.join("plugin.log");
    let _log = PLUGIN_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: as in `health_check_matches_go_step_for_step`.
    unsafe { std::env::set_var("ENV_PLUGIN_LOG", &log) };
    let mut request = log.as_os_str().to_owned();
    request.push(".crash");
    let request = PathBuf::from(request);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let env: Arc<HealthEnv> = Arc::new(Environment::new(
            Box::new(|_| Arc::new(NoApi)),
            Arc::new(NoDriver),
            root.join("plugins"),
            root.join("webapp"),
        ));
        env.activate("crashy").await.unwrap();
        crash(&env, &request, "loop").await;
        env.toggle_health_check_job_every(true, std::time::Duration::from_millis(50))
            .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        // Restarted: running again, with a supervisor that answers.
        while env.hooks_for_plugin("crashy").is_err()
            || env.perform_health_check("crashy").await.is_err()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the job never restarted the crashed plugin"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let activations = plugin_log(&log)
            .iter()
            .filter(|l| l.as_str() == "env_plugin_crashy: OnActivate")
            .count();
        assert_eq!(activations, 2, "started, then restarted by the job");

        // Not running: the loop leaves it alone even though its dead supervisor would fail.
        env.deactivate("crashy").await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!env.is_active("crashy"), "a stopped plugin is not checked");
        env.shutdown().await;
        assert!(env.health_check_job().is_none(), "Shutdown stops the job");
    });
}
