//! Cross-server parity for the **plugin hook call sites** on the post and reaction write paths
//! (docs/PLUGIN_PLAN.md, Phase 5; `mm_app::plugin_hooks`).
//!
//! ```sh
//! scripts/parity.sh --test parity plugin_hooks
//! ```
//!
//! # How this can be a parity test at all
//!
//! One plugin binary — `mm-plugin`'s `examples/hook_recorder`, written with the Rust SDK — runs
//! under **both** hosts, because a plugin built on the SDK speaks what a Go plugin speaks (D1).
//! It appends one JSON line per hook it is handed, rendered from the gob stream itself, so the
//! two transcripts are the same function of what each host sent. A difference is the host's.
//!
//! The stack's Go server hosts no plugins, so this suite starts **its own** Go server (Go's
//! port + 74, its own run directory, the shared database and configuration) and a Rust host
//! beside it on :8119, with an identical bundle in each plugin directory. The same request goes
//! to each, and the suite compares three things: the client's answer, the rows the two servers
//! wrote, and the plugin's transcript.
//!
//! # What is compared, and what cannot be
//!
//! Two posts on two servers have different ids and different timestamps, so
//! [`normalise`] replaces each id-shaped and time-shaped value with a token that still says
//! *empty or not* and *zero or not*. Everything else is compared exactly: the hook names, their
//! order, the `plugin.Context` (the session is one token, so `SessionId` matches, and both peers
//! are 127.0.0.1) and every remaining field of the `Post` and `Reaction` gob carried.
//! `RequestId` is minted per request by each server and is the one field of the six that cannot
//! be compared.
//!
//! # The rejecting hooks
//!
//! `hook_recorder` keys its answer off the post's own message, so one request provokes the same
//! branch on both sides: `!reject <reason>`, `!dismiss`, `!rewrite <text>` and their `-edit`
//! forms. Those exercise Go's rejection id (`"Post rejected by plugin. " + reason`, the reason
//! inside the id), `DismissPostError` standing alone, and the merge that fills a one-field
//! replacement back in.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value as Json;

use crate::common;

use common::{GO, SecondServer, client, go_minted_token, request_raw, stack_enabled};

/// The Rust host; see `second_server_ports`.
const HOST_PORT: u16 = 8119;
/// This suite's Go server sits at Go's port plus this.
const GO_OFFSET: u16 = 74;
/// The bundle id, which `PluginStates` has to enable on both sides.
const PLUGIN_ID: &str = "mmrs.hookrecorder";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `examples/hook_recorder`, built by this test.
///
/// `cargo test --test parity` does not build another crate's examples, and cargo leaves a binary
/// untouched when a source is rewritten with its old contents — which is exactly what the
/// mutation harness does. So build it here rather than trusting what is on disk.
fn hook_recorder() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "mm-plugin", "--example", "hook_recorder"])
                .current_dir(repo())
                .status()
                .expect("cargo runs");
            assert!(status.success(), "building examples/hook_recorder failed");
            let exe = std::env::current_exe().expect("the test binary");
            let profile = exe
                .parent()
                .and_then(|deps| deps.parent())
                .expect("target/<profile>");
            profile.join("examples").join("hook_recorder")
        })
        .clone()
}

/// The bundle as a tar.gz in a file store, which is the only place a plugin survives start-up:
/// `syncPlugins` **removes every locally installed plugin** before installing what the file store
/// holds (app/plugin.go:286), so a bundle dropped straight into the plugin directory is deleted
/// by the first `initPlugins` on both sides.
///
/// Built once: the debug executable is 130 MB, and `gzip -1` takes under a second where the
/// default takes ten.
fn bundle() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-bundle");
            let stage = scratch.join(PLUGIN_ID);
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&stage).expect("the staging directory");
            std::fs::write(
                stage.join("plugin.json"),
                format!(
                    r#"{{"id": "{PLUGIN_ID}", "name": "Hook Recorder", "version": "0.1.0", "server": {{"executable": "plugin"}}}}"#
                ),
            )
            .expect("the manifest");
            // Copied, not linked: a bundle carrying a symlink is what `extractTarGz` refuses.
            std::fs::copy(hook_recorder(), stage.join("plugin")).expect("the executable");
            let tarball = scratch.join(format!("{PLUGIN_ID}.tar.gz"));
            let status = Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "tar -c -C {stage} {PLUGIN_ID} | gzip -1 > {out}",
                    stage = scratch.display(),
                    out = tarball.display()
                ))
                .status()
                .expect("tar runs");
            assert!(status.success(), "packing the bundle failed");
            tarball
        })
        .clone()
}

/// A run directory: the bundle in the file store, and the two plugin directories empty.
fn lay_out(run: &Path) -> PathBuf {
    let _ = std::fs::remove_dir_all(run);
    for dir in ["data/plugins", "plugins", "client", "logs"] {
        std::fs::create_dir_all(run.join(dir)).expect("the run directory");
    }
    std::fs::copy(
        bundle(),
        run.join("data/plugins").join(format!("{PLUGIN_ID}.tar.gz")),
    )
    .expect("the bundle reaches the file store");
    run.join("hooks.jsonl")
}

/// The Go server this suite starts: killed on drop.
struct GoServer {
    child: std::process::Child,
    base: String,
}

impl Drop for GoServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn go_port() -> u16 {
    GO.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("GO has a port")
}

/// Start the stack's Go binary in `run` with its own file store and plugin directories, as
/// `parity::plugin_startup` does. **Two database connections, not fifty**: the stack's servers
/// already hold most of the ceiling, and a server with Go's default `MaxIdleConns` takes the
/// database down for every suite running beside this one.
async fn start_go(run: &Path, env: &[(&str, &str)]) -> GoServer {
    let binary = repo().join("reference/.build/mattermost");
    assert!(
        binary.exists(),
        "no Go binary at {} — run scripts/go-server.sh",
        binary.display()
    );
    let src = repo().join("reference/mattermost/server");
    for dir in ["i18n", "templates", "fonts"] {
        let _ = std::os::unix::fs::symlink(src.join(dir), run.join(dir));
    }
    let port = go_port() + GO_OFFSET;
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ss -ltnp 2>/dev/null | awk '$4 ~ /:{port}$/' \
             | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u | xargs -r kill -9"
        ))
        .status();
    let dsn = format!(
        "{}?sslmode=disable&connect_timeout=10",
        std::env::var("DATABASE_URL").expect("parity.sh sets DATABASE_URL")
    );
    let s = |p: &str| run.join(p).to_string_lossy().into_owned();
    let log = std::fs::File::create(run.join("go.log")).expect("the log");
    let mut command = Command::new(&binary);
    command
        .arg("server")
        .current_dir(run)
        .env("PWD", run)
        .env("MM_CONFIG", &dsn)
        .env("MM_SQLSETTINGS_DRIVERNAME", "postgres")
        .env("MM_SQLSETTINGS_DATASOURCE", &dsn)
        .env(
            "MM_SERVICESETTINGS_SITEURL",
            format!("http://localhost:{port}"),
        )
        .env("MM_SERVICESETTINGS_LISTENADDRESS", format!(":{port}"))
        .env("MM_SERVICESETTINGS_ENABLELOCALMODE", "false")
        .env("MM_SQLSETTINGS_MAXIDLECONNS", "2")
        .env("MM_SQLSETTINGS_MAXOPENCONNS", "5")
        .env("MM_JOBSETTINGS_RUNJOBS", "false")
        .env("MM_JOBSETTINGS_RUNSCHEDULER", "false")
        .env("MM_FILESETTINGS_DIRECTORY", format!("{}/", s("data")))
        .env("MM_PLUGINSETTINGS_DIRECTORY", s("plugins"))
        .env("MM_PLUGINSETTINGS_CLIENTDIRECTORY", s("client"))
        .stdout(log.try_clone().expect("the log"))
        .stderr(log);
    for (key, value) in env {
        command.env(key, value);
    }
    let child = command.spawn().expect("the Go server starts");
    let mut server = GoServer {
        child,
        base: format!("http://127.0.0.1:{port}"),
    };
    let client = client();
    for _ in 0..450 {
        assert!(
            server.child.try_wait().ok().flatten().is_none(),
            "the Go server exited — see {}",
            run.join("go.log").display()
        );
        if client
            .get(format!("{}/api/v4/system/ping", server.base))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return server;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!(
        "the Go server never answered — see {}",
        run.join("go.log").display()
    );
}

/// Set (or, with `None`, remove) this suite's id in the shared `PluginStates`, through main Go.
async fn plant_state(client: &reqwest::Client, admin: &str, enable: Option<bool>) {
    let config: Json = client
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
        match enable {
            Some(on) => {
                map.insert(PLUGIN_ID.to_owned(), serde_json::json!({ "Enable": on }));
            }
            None => {
                map.remove(PLUGIN_ID);
            }
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
    assert!(status.is_success(), "patching PluginStates: {status}");
}

/// A plugin id no bundle installs, planted as a channel's guard so that both hosts take
/// `resolveGuards`' fail-closed branch (guarded_hooks.go:56).
const ABSENT_GUARD: &str = "mmrs.absentguard";

/// Plant, or remove, a `ChannelGuards` row. There is no route that writes one: the two plugin API
/// methods that do (`RegisterChannelGuard`, `UnregisterChannelGuard`) are the plugin plan's
/// Phase 6, so the only way to reach the guarded branch at all is to write the row.
///
/// It has to be planted **before** either server starts, because Go loads the whole table into a
/// cache in `NewChannels` and reloads it only on a register or a cluster message
/// (app/channel_guards.go:29).
async fn plant_guard(channel_id: &str, planted: bool) {
    let pool = common::fixture_pool().await.expect("the stack database");
    if planted {
        sqlx::query(
            "INSERT INTO ChannelGuards (ChannelId, PluginId, CreatedAt) VALUES ($1, $2, $3)
             ON CONFLICT (ChannelId, PluginId) DO NOTHING",
        )
        .bind(channel_id)
        .bind(ABSENT_GUARD)
        .bind(1_700_000_000_000i64)
        .execute(&pool)
        .await
        .expect("the guard row is written");
    } else {
        sqlx::query("DELETE FROM ChannelGuards WHERE ChannelId = $1")
            .bind(channel_id)
            .execute(&pool)
            .await
            .expect("the guard row goes");
    }
}

/// Wait until a server reports the recorder running.
async fn wait_until_running(client: &reqwest::Client, admin: &str, base: &str) {
    for _ in 0..200 {
        let (status, body, _) = request_raw(
            client,
            base,
            reqwest::Method::GET,
            Some(admin),
            "/api/v4/plugins/statuses",
            None,
        )
        .await;
        if status == 200 {
            let value: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
            if value.as_array().is_some_and(|all| {
                all.iter()
                    .any(|s| s["plugin_id"] == PLUGIN_ID && s["state"] == 2)
            }) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("{base}: the hook recorder never reached the running state");
}

// ---------------------------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------------------------

/// Values two servers cannot agree on because each minted its own. `OriginalId` is here because
/// the old post `MessageHasBeenUpdated` carries is the **history row**, whose `OriginalId` is the
/// post's id on that server.
const ID_KEYS: [&str; 6] = [
    "Id",
    "PostId",
    "PendingPostId",
    "RootId",
    "OriginalId",
    "RequestId",
];
/// Epoch milliseconds, likewise. `DeleteAt` is one of them on the history row, and its
/// zero-or-not is still asserted where it matters (the deleted post the delete hook carries).
const TIME_KEYS: [&str; 5] = ["CreateAt", "UpdateAt", "EditAt", "LastReplyAt", "DeleteAt"];

/// Replace what cannot be compared with a token that keeps the only thing worth asserting about
/// it: an id is `""` or `"<id>"`, a timestamp is `0` or `"<set>"`. A mutation that stopped
/// setting one, or set one it should not, still shows.
fn normalise(value: &mut Json) {
    match value {
        Json::Array(items) => items.iter_mut().for_each(normalise),
        Json::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if ID_KEYS.contains(&key.as_str()) {
                    if let Some(text) = entry.as_str() {
                        *entry = Json::String(if text.is_empty() { "" } else { "<id>" }.to_owned());
                        continue;
                    }
                }
                if TIME_KEYS.contains(&key.as_str()) {
                    if let Some(n) = entry.as_i64() {
                        *entry = if n == 0 {
                            Json::from(0)
                        } else {
                            Json::String("<set>".to_owned())
                        };
                        continue;
                    }
                }
                normalise(entry);
            }
        }
        _ => {}
    }
}

/// The same idea for a **response body**, whose keys are the JSON names rather than the Go ones.
/// Used on the created post, so that everything the two servers wrote to the row — props, type,
/// metadata, the file id list — is compared and not just the message.
fn normalise_body(value: &mut Json) {
    const ID_KEYS: [&str; 5] = ["id", "post_id", "pending_post_id", "root_id", "original_id"];
    const TIME_KEYS: [&str; 5] = [
        "create_at",
        "update_at",
        "edit_at",
        "delete_at",
        "last_reply_at",
    ];
    match value {
        Json::Array(items) => items.iter_mut().for_each(normalise_body),
        Json::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if ID_KEYS.contains(&key.as_str()) {
                    if let Some(text) = entry.as_str() {
                        *entry = Json::String(if text.is_empty() { "" } else { "<id>" }.to_owned());
                        continue;
                    }
                }
                if TIME_KEYS.contains(&key.as_str()) {
                    if let Some(n) = entry.as_i64() {
                        *entry = if n == 0 {
                            Json::from(0)
                        } else {
                            Json::String("<set>".to_owned())
                        };
                        continue;
                    }
                }
                normalise_body(entry);
            }
        }
        _ => {}
    }
}

/// Two created posts, compared field for field once the ids and timestamps are tokens.
fn same_post(go: &Json, rust: &Json, what: &str) {
    let (mut go, mut rust) = (go.clone(), rust.clone());
    normalise_body(&mut go);
    normalise_body(&mut rust);
    assert_eq!(go, rust, "{what}: the two rows");
}

/// The transcript so far, normalised.
fn transcript(path: &Path) -> Vec<Json> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut value: Json =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("transcript line {l:?}: {e}"));
            normalise(&mut value);
            value
        })
        .collect()
}

/// The hook names in the transcript, in order.
fn names(entries: &[Json]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| e["hook"].as_str().map(str::to_owned))
        .collect()
}

/// Wait for both transcripts to hold `expected` hooks, then return them. The `*HasBeen*` hooks
/// are dispatched on a detached task on both sides, so neither is ordered against the response.
async fn settled(go: &Path, rust: &Path, expected: usize) -> (Vec<Json>, Vec<Json>) {
    for _ in 0..200 {
        let (g, r) = (transcript(go), transcript(rust));
        if g.len() >= expected && r.len() >= expected {
            return (g, r);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (g, r) = (transcript(go), transcript(rust));
    panic!(
        "waited for {expected} hooks; Go saw {:?}, Rust saw {:?}",
        names(&g),
        names(&r)
    );
}

/// One action against both servers: the request, then the hooks it caused, compared.
struct Pair {
    client: reqwest::Client,
    admin: String,
    go_base: String,
    rust_base: String,
    go_log: PathBuf,
    rust_log: PathBuf,
    seen: usize,
}

impl Pair {
    /// Send the same request to each server and return `(status, body)` for both. The Rust one
    /// is asserted to have been served here rather than forwarded.
    async fn both(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> ((u16, Json), (u16, Json)) {
        let decode = |bytes: Vec<u8>| -> Json {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                Json::String(String::from_utf8_lossy(&bytes).trim_end().to_owned())
            })
        };
        let (gs, gb, _) = request_raw(
            &self.client,
            &self.go_base,
            method.clone(),
            Some(&self.admin),
            path,
            body,
        )
        .await;
        let (rs, rb, served_by) = request_raw(
            &self.client,
            &self.rust_base,
            method,
            Some(&self.admin),
            path,
            body,
        )
        .await;
        assert_eq!(served_by.as_deref(), Some("rust"), "{path} was forwarded");
        ((gs, decode(gb)), (rs, decode(rb)))
    }

    /// Wait for `more` further hooks on each side and assert the two agree, entry for entry.
    async fn hooks(&mut self, more: usize, what: &str) -> Vec<Json> {
        self.seen += more;
        let (go, rust) = settled(&self.go_log, &self.rust_log, self.seen).await;
        assert_eq!(names(&go), names(&rust), "{what}: the hooks that fired");
        assert_eq!(go.len(), self.seen, "{what}: Go fired an extra hook");
        assert_eq!(rust.len(), self.seen, "{what}: Rust fired an extra hook");
        for (index, (g, r)) in go.iter().zip(rust.iter()).enumerate() {
            assert_eq!(g, r, "{what}: hook {index} differs");
        }
        go[self.seen - more..].to_vec()
    }
}

/// The error body's shape, without the per-request id.
fn error_of(body: &Json) -> (Option<&str>, Option<&str>, Option<i64>) {
    (
        body["id"].as_str(),
        body["message"].as_str(),
        body["status_code"].as_i64(),
    )
}

/// The planted `PluginStates` key and the planted guard row are **shared-stack state**, so they
/// have to come out even when the body panics. They did not, once: a mutation run left
/// `mmrs.hookrecorder` enabled in the shared configuration document and
/// `mm_store::db_config_active` failed on the drift for every checkout until it was taken out by
/// hand. So the body runs under `catch_unwind` and the cleanup is unconditional.
#[tokio::test]
async fn the_post_and_reaction_hooks_fire_as_go_fires_them() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_hook_tour(&client, &admin))
        .catch_unwind()
        .await;
    // Whatever happened, the shared configuration and the guard table go back as they were.
    plant_state(&client, &admin, None).await;
    let planted = LAST_GUARDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(channel) = planted {
        plant_guard(&channel, false).await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The channel this run planted a guard on, so the cleanup can find it after a panic.
static LAST_GUARDED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

async fn run_the_hook_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;

    // Before either server starts: Go caches the guard table at start-up.
    let team = common::create_team(&client, &admin, "hookhk").await;
    let channel = common::create_channel(&client, &admin, &team, "hookhk").await;
    let guarded = common::create_channel(&client, &admin, &team, "hookgd").await;
    *LAST_GUARDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(guarded.clone());
    plant_guard(&guarded, true).await;

    let go = start_go(
        &go_run,
        &[("HOOK_RECORDER_TRANSCRIPT", &go_log.to_string_lossy())],
    )
    .await;

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust = SecondServer::start_in(
        HOST_PORT,
        &rs_run,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
            ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
            ("HOOK_RECORDER_TRANSCRIPT", &rust_log.to_string_lossy()),
        ],
    )
    .await
    .expect("the Rust host starts");

    wait_until_running(&client, &admin, &go.base).await;
    wait_until_running(&client, &admin, &rust.base).await;

    let mut pair = Pair {
        client: client.clone(),
        admin: admin.clone(),
        go_base: go.base.clone(),
        rust_base: rust.base.clone(),
        go_log,
        rust_log,
        seen: 0,
    };
    let post_body = |message: &str| {
        serde_json::to_vec(&serde_json::json!({
            "channel_id": channel,
            "message": message,
            // Props are the conversion most likely to drift: gob carries `map[string]any` as
            // registered interface values, and a JSON number reaches Go's map as a `float64`
            // whatever it was written as.
            "props": { "mmrs_hook": "yes", "mmrs_count": 3, "mmrs_on": true },
        }))
        .expect("a body")
    };

    // 1. A plain post: MessageWillBePosted, then MessageHasBeenPosted.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(&post_body("hook recorder plain")),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a plain post");
    let fired = pair.hooks(2, "a plain post").await;
    assert_eq!(
        names(&fired),
        ["MessageWillBePosted", "MessageHasBeenPosted"],
        "Go's order"
    );
    // The `Will` hook sees the post before it is saved, the `Has` hook after: no id and no
    // CreateAt, then both. Without this the two hooks could be swapped and the diff would not
    // notice, because both carry a post. A zero field is **absent** from the rendering, because
    // gob omits it — which is itself the thing worth asserting.
    assert_eq!(
        fired[0]["args"]["B"]["Id"],
        Json::Null,
        "gob omits an empty Id"
    );
    assert_eq!(fired[0]["args"]["B"]["CreateAt"], Json::Null);
    assert_eq!(fired[1]["args"]["B"]["Id"], Json::String("<id>".to_owned()));
    assert_eq!(
        fired[1]["args"]["B"]["CreateAt"],
        Json::String("<set>".to_owned())
    );
    // The context: five of the six fields are the same request on two servers.
    assert_eq!(fired[0]["args"]["A"]["IPAddress"], "127.0.0.1");
    assert_eq!(
        fired[0]["args"]["A"]["SessionId"].as_str().map(str::len),
        Some(26),
        "the session id itself, not normalised away, so the two hosts must agree on it"
    );
    let rust_post_id = rb["id"].as_str().expect("an id").to_owned();

    // 1b. The other three `plugin.Context` fields, which are headers and so empty unless a
    //     request carries them. `Connection-Id` is `model.ConnectionId`, and the client's
    //     `X-Request-ID` is deliberately ignored: each server mints its own.
    let headed = reqwest::Client::builder()
        .user_agent("mmrs-hook-parity/1.0")
        .build()
        .expect("a client");
    for base in [&go.base, &rust.base] {
        let response = headed
            .post(format!("{base}/api/v4/posts"))
            .bearer_auth(&admin)
            .header("Content-Type", "application/json")
            .header("Accept-Language", "fr-CA,fr;q=0.9")
            .header("Connection-Id", "conn-parity-1")
            .header("X-Request-ID", "client-supplied")
            .body(post_body("hook recorder with headers"))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status().as_u16(), 201, "{base}");
    }
    let fired = pair.hooks(2, "a post carrying headers").await;
    let context = &fired[0]["args"]["A"];
    assert_eq!(context["UserAgent"], "mmrs-hook-parity/1.0");
    assert_eq!(context["AcceptLanguage"], "fr-CA,fr;q=0.9");
    assert_eq!(context["ConnectionId"], "conn-parity-1");
    assert_ne!(
        context["RequestId"], "client-supplied",
        "the request id is minted, never the client's"
    );

    // 2. A replacement carrying one field: the merge fills the other 25 back in.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(&post_body("!rewrite replaced by the plugin")),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    assert_eq!(gb["message"], "replaced by the plugin");
    assert_eq!(rb["message"], "replaced by the plugin");
    // The replacement carried `Message` and nothing else, so everything else in the row — the
    // channel, the props, the type — is what the merge put back.
    same_post(&gb, &rb, "a rewritten post");
    assert_eq!(gb["props"]["mmrs_count"], rb["props"]["mmrs_count"]);
    pair.hooks(2, "a rewritten post").await;

    // 3. A rejection: the reason is inside the error id, and `where` is the lower-case
    //    `createPost`.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(&post_body("!reject not on my watch")),
        )
        .await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    assert_eq!(error_of(&gb), error_of(&rb), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "Post rejected by plugin. not on my watch");
    pair.hooks(1, "a rejected post").await;

    // 4. The one reason that is not prefixed.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(&post_body("!dismiss")),
        )
        .await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    assert_eq!(error_of(&gb), error_of(&rb), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "plugin.message_will_be_posted.dismiss_post");
    pair.hooks(1, "a dismissed post").await;

    // 5. An edit: MessageWillBeUpdated (new and old) then MessageHasBeenUpdated.
    let go_post_id = {
        let (status, body, _) = request_raw(
            &client,
            &go.base,
            reqwest::Method::POST,
            Some(&admin),
            "/api/v4/posts",
            Some(&post_body("hook recorder to edit")),
        )
        .await;
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
        let value: Json = serde_json::from_slice(&body).expect("a post");
        value["id"].as_str().expect("an id").to_owned()
    };
    let (status, body, served_by) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/posts",
        Some(&post_body("hook recorder to edit")),
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let rust_edit_id = serde_json::from_slice::<Json>(&body).expect("a post")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    // One post per server, so each transcript gains one pair — not two.
    pair.hooks(2, "a post to edit").await;

    let patch = |message: &str| {
        serde_json::to_vec(&serde_json::json!({ "message": message })).expect("a body")
    };
    let (gs, gb, _) = request_raw(
        &client,
        &go.base,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/posts/{go_post_id}/patch"),
        Some(&patch("hook recorder edited")),
    )
    .await;
    let (rs, rb, served_by) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/posts/{rust_edit_id}/patch"),
        Some(&patch("hook recorder edited")),
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(
        (gs, rs),
        (200, 200),
        "Go {} / Rust {}",
        String::from_utf8_lossy(&gb),
        String::from_utf8_lossy(&rb)
    );
    let fired = pair.hooks(2, "an edit").await;
    assert_eq!(
        names(&fired),
        ["MessageWillBeUpdated", "MessageHasBeenUpdated"]
    );
    assert_eq!(
        fired[0]["args"]["B"]["Message"], "hook recorder edited",
        "B is the new post"
    );
    assert_eq!(
        fired[0]["args"]["C"]["Message"], "hook recorder to edit",
        "C is the old post"
    );
    assert_eq!(fired[1]["args"]["B"]["Message"], "hook recorder edited");
    assert_eq!(fired[1]["args"]["C"]["Message"], "hook recorder to edit");

    // 6. A refused edit: `where` is `UpdatePost`, capital U, and the reason is in the id.
    let refuse = patch("!reject-edit no edits either");
    let (gs, gb, _) = request_raw(
        &client,
        &go.base,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/posts/{go_post_id}/patch"),
        Some(&refuse),
    )
    .await;
    let (rs, rb, _) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/posts/{rust_edit_id}/patch"),
        Some(&refuse),
    )
    .await;
    assert_eq!((gs, rs), (400, 400));
    let (gb, rb): (Json, Json) = (
        serde_json::from_slice(&gb).expect("an error"),
        serde_json::from_slice(&rb).expect("an error"),
    );
    assert_eq!(error_of(&gb), error_of(&rb), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "Post rejected by plugin. no edits either");
    pair.hooks(1, "a refused edit").await;

    // 7. A reaction, added and removed.
    let me: Json = serde_json::from_slice(
        &request_raw(
            &client,
            &go.base,
            reqwest::Method::GET,
            Some(&admin),
            "/api/v4/users/me",
            None,
        )
        .await
        .1,
    )
    .expect("the caller");
    let me = me["id"].as_str().expect("an id").to_owned();
    let reaction_body = |post_id: &str| {
        serde_json::to_vec(&serde_json::json!({
            "user_id": me,
            "post_id": post_id,
            "emoji_name": "+1",
        }))
        .expect("a body")
    };
    let (gs, gb, _) = request_raw(
        &client,
        &go.base,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/reactions",
        Some(&reaction_body(&go_post_id)),
    )
    .await;
    let (rs, rb, served_by) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/reactions",
        Some(&reaction_body(&rust_post_id)),
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(
        (gs, rs),
        (200, 200),
        "Go {} / Rust {}",
        String::from_utf8_lossy(&gb),
        String::from_utf8_lossy(&rb)
    );
    let fired = pair.hooks(1, "a reaction").await;
    assert_eq!(names(&fired), ["ReactionHasBeenAdded"]);
    assert_eq!(
        fired[0]["args"]["B"]["EmojiName"], "+1",
        "the reaction crosses whole"
    );
    assert_eq!(
        fired[0]["args"]["B"]["ChannelId"], channel,
        "pre-populated from the post, not from the request"
    );

    let (gs, _, _) = request_raw(
        &client,
        &go.base,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/users/{me}/posts/{go_post_id}/reactions/+1"),
        None,
    )
    .await;
    let (rs, _, served_by) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/users/{me}/posts/{rust_post_id}/reactions/+1"),
        None,
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks(1, "a removed reaction").await;
    assert_eq!(names(&fired), ["ReactionHasBeenRemoved"]);

    // 8. A delete: `MessageHasBeenDeleted` carries the post as it was *before* the row changed.
    let (gs, _, _) = request_raw(
        &client,
        &go.base,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/posts/{go_post_id}"),
        None,
    )
    .await;
    let (rs, _, served_by) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/posts/{rust_edit_id}"),
        None,
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks(1, "a delete").await;
    assert_eq!(names(&fired), ["MessageHasBeenDeleted"]);
    assert_eq!(
        fired[0]["args"]["B"]["DeleteAt"],
        Json::Null,
        "the post as it was before the delete: a zero DeleteAt, so gob omits it"
    );

    // 9. A channel with a guard whose plugin is not installed: `resolveGuards` refuses the
    //    request at 503 before any hook runs, on both sides, and the transcript does not grow.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(
                &serde_json::to_vec(&serde_json::json!({
                    "channel_id": guarded,
                    "message": "into a guarded channel",
                }))
                .expect("a body"),
            ),
        )
        .await;
    assert_eq!((gs, rs), (503, 503), "Go {gb} / Rust {rb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a guarded channel whose plugin is not installed",
    );
    assert_eq!(gb["id"], "app.plugin.inactive_guard.app_error");
    tokio::time::sleep(Duration::from_millis(500)).await;
    pair.hooks(0, "a guarded channel fires nothing").await;

    // Every hook, in the order the whole session produced them, on both sides at once.
    let (go_all, rust_all) = settled(&pair.go_log, &pair.rust_log, pair.seen).await;
    let expected = [
        "MessageWillBePosted",
        "MessageHasBeenPosted",
        "MessageWillBePosted",
        "MessageHasBeenPosted",
        "MessageWillBePosted",
        "MessageHasBeenPosted",
        "MessageWillBePosted",
        "MessageWillBePosted",
        "MessageWillBePosted",
        "MessageHasBeenPosted",
        "MessageWillBeUpdated",
        "MessageHasBeenUpdated",
        "MessageWillBeUpdated",
        "ReactionHasBeenAdded",
        "ReactionHasBeenRemoved",
        "MessageHasBeenDeleted",
    ];
    assert_eq!(names(&go_all), expected, "the Go host's whole transcript");
    assert_eq!(
        names(&rust_all),
        expected,
        "the Rust host's whole transcript"
    );

    drop(rust);
    drop(go);
    common::delete_channel(&client, &admin, &channel).await;
    common::delete_channel(&client, &admin, &guarded).await;
}

/// The rendering the transcript is written in is a pure function of the value, so the
/// normalisation this suite compares through can be checked without a stack.
#[test]
fn normalise_keeps_empty_apart_from_set() {
    let mut value: Json = serde_json::json!({
        "hook": "MessageWillBePosted",
        "args": {
            "A": { "RequestId": "abcdefghijklmnopqrstuvwxyz", "SessionId": "s" },
            "B": {
                "Id": "", "CreateAt": 0, "UpdateAt": 17_000_000_000_000i64,
                "Message": "hello", "RootId": "rrrr",
            },
        },
    });
    normalise(&mut value);
    assert_eq!(
        value,
        serde_json::json!({
            "hook": "MessageWillBePosted",
            "args": {
                "A": { "RequestId": "<id>", "SessionId": "s" },
                "B": {
                    "Id": "", "CreateAt": 0, "UpdateAt": "<set>",
                    "Message": "hello", "RootId": "<id>",
                },
            },
        })
    );
}

/// `names` reads the key the recorder writes, and nothing else in the line.
#[test]
fn names_reads_the_hook_key() {
    let entries = vec![
        serde_json::json!({ "hook": "A", "args": { "hook": "not this" } }),
        serde_json::json!({ "other": "B" }),
    ];
    assert_eq!(names(&entries), ["A"]);
}

/// The transcript is JSON lines, and a blank line is not one.
#[test]
fn a_blank_line_is_not_an_entry() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-unit");
    std::fs::create_dir_all(&dir).expect("the directory");
    let path = dir.join("hooks.jsonl");
    std::fs::write(&path, "{\"hook\":\"One\"}\n\n{\"hook\":\"Two\"}\n").expect("the file");
    assert_eq!(names(&transcript(&path)), ["One", "Two"]);
}

/// A transcript that is not there yet is empty, not a panic: both servers are polled before
/// either has answered a request.
#[test]
fn a_missing_transcript_is_empty() {
    assert!(transcript(Path::new("/nonexistent/hooks.jsonl")).is_empty());
}
