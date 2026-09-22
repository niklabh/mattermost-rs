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

/// The Rust host of the post and reaction tranche; see `second_server_ports`.
const HOST_PORT: u16 = 8119;
/// Its Go server sits at Go's port plus this.
const GO_OFFSET: u16 = 74;
/// The Rust host of the channel and team membership tranche, which runs its own pair of servers
/// so that either tranche can be run, and debugged, on its own.
const MEMBERSHIP_HOST_PORT: u16 = 8130;
/// Its Go server.
const MEMBERSHIP_GO_OFFSET: u16 = 80;
/// The Rust host of the user lifecycle tranche — creation, the two login hooks, deactivation.
const LIFECYCLE_HOST_PORT: u16 = 8131;
/// Its Go server.
const LIFECYCLE_GO_OFFSET: u16 = 81;
/// The Rust host of the file download tranche.
const DOWNLOAD_HOST_PORT: u16 = 8132;
/// Its Go server.
const DOWNLOAD_GO_OFFSET: u16 = 82;
/// The Rust host of the file upload tranche.
const UPLOAD_HOST_PORT: u16 = 8133;
/// Its Go server.
const UPLOAD_GO_OFFSET: u16 = 83;
/// The Rust host of the channel lifecycle tranche.
const CHANNEL_HOST_PORT: u16 = 8134;
/// Its Go server.
const CHANNEL_GO_OFFSET: u16 = 84;
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
///
/// It also gets Go's `i18n/`, because **both** servers started here resolve it against their
/// working directory and neither will start without it — `utils.TranslationsPreInit` on the Go
/// side, `mm_app::i18n::init` on ours since the error messages became Go's sentences.
fn lay_out(run: &Path) -> PathBuf {
    let _ = std::fs::remove_dir_all(run);
    for dir in ["data/plugins", "plugins", "client", "logs"] {
        std::fs::create_dir_all(run.join(dir)).expect("the run directory");
    }
    let _ = std::os::unix::fs::symlink(
        repo().join("reference/mattermost/server/i18n"),
        run.join("i18n"),
    );
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
async fn start_go(run: &Path, env: &[(&str, &str)], offset: u16) -> GoServer {
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
    let port = go_port() + offset;
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
/// `LastUpdateAt` and `LastViewedAt` are the `ChannelMember` pair: both are stamped by the save,
/// so the two servers' members differ by the milliseconds between the two requests. `LastLogin`
/// is the lifecycle tranche's: one account logs in to both servers, and the second login reads
/// the time the first one wrote. `LastPasswordUpdate` is too: each side's created account is
/// stamped by its own save. `LastPostAt` and `LastRootPostAt` are the channel tour's: two channels
/// Go made a second apart carry join posts a second apart.
const TIME_KEYS: [&str; 11] = [
    "CreateAt",
    "UpdateAt",
    "EditAt",
    "LastReplyAt",
    "DeleteAt",
    "LastUpdateAt",
    "LastViewedAt",
    "LastLogin",
    "LastPasswordUpdate",
    "LastPostAt",
    "LastRootPostAt",
];

/// A `FileInfo`'s storage paths, which carry the file's own id (and, for a resumable upload,
/// the session's) as a segment.
const PATH_KEYS: [&str; 3] = ["Path", "ThumbnailPath", "PreviewPath"];

/// Every 26-character id segment of a storage path as `<id>`, the rest untouched — so the date,
/// the layout and the file name are still compared.
fn path_without_ids(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.len() == 26
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                "<id>"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

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
                if PATH_KEYS.contains(&key.as_str()) {
                    if let Some(path) = entry.as_str() {
                        *entry = Json::String(path_without_ids(path));
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
    const TIME_KEYS: [&str; 7] = [
        "create_at",
        "update_at",
        "edit_at",
        "delete_at",
        "last_reply_at",
        "last_post_at",
        "last_root_post_at",
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

/// Every occurrence of each `from` replaced by its `to`, over the raw text.
///
/// The membership tranche needs it: two servers cannot add the **same** user to the same channel
/// (the second finds the member already there and writes nothing), so each side acts on a subject
/// of its own and the two ids — and the two usernames, which reach the system posts and their
/// props — are rewritten to one token before anything is compared. Everything else still has to
/// match exactly, which is the point of doing it as a substitution rather than as another
/// [`normalise`] key.
fn scrub(text: &str, pairs: &[(String, String)]) -> String {
    let mut out = text.to_owned();
    for (from, to) in pairs {
        out = out.replace(from.as_str(), to);
    }
    out
}

/// The transcript so far, scrubbed of this side's subject and normalised.
///
/// **Only complete lines.** The plugin appends while the suite polls, so a read can land in the
/// middle of an entry; a tail with no newline yet is an entry still being written, not a
/// malformed one. Measured: 2 of 15 runs of the lifecycle tour died parsing half a deactivated
/// user, and one of them was a no-op mutation control reported as "caught".
fn transcript_of(path: &Path, pairs: &[(String, String)]) -> Vec<Json> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    complete
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let line = scrub(l, pairs);
            let mut value: Json = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("transcript line {line:?}: {e}"));
            sort_slices_by_id(&mut value);
            normalise(&mut value);
            value
        })
        .collect()
}

/// The two consumed hooks are handed a **slice**, and Go builds it by walking a map, so its order
/// is whatever the runtime gave that iteration; this server walks a `BTreeMap`. Neither order is
/// a promise, so a slice of posts is sorted by id before the ids are turned into tokens — both
/// servers read the same rows, so the same ids sort the same way on both sides.
fn sort_slices_by_id(value: &mut Json) {
    let Some(args) = value.get_mut("args").and_then(Json::as_object_mut) else {
        return;
    };
    for key in ["A", "B"] {
        let Some(items) = args.get_mut(key).and_then(Json::as_array_mut) else {
            continue;
        };
        if items.is_empty() || !items.iter().all(|item| item["Id"].is_string()) {
            continue;
        }
        items.sort_by(|a, b| a["Id"].as_str().cmp(&b["Id"].as_str()));
    }
}

/// [`transcript_of`] with nothing to scrub.
fn transcript(path: &Path) -> Vec<Json> {
    transcript_of(path, &[])
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
        GO_OFFSET,
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

// ---------------------------------------------------------------------------------------------
// The channel and team membership tranche
// ---------------------------------------------------------------------------------------------

/// One side's subject, and the tokens the other side's is rewritten to.
///
/// Two servers on one database cannot perform the *same* membership change: whichever goes second
/// finds the row already there and writes nothing, fires nothing and answers 200 from a branch the
/// other never took. So each side gets a plain user of its own, and [`scrub`] turns both into one
/// token before the transcripts are compared. The **actor** is shared, because a `model.User`
/// carries a bcrypt hash and two accounts can never agree on one.
struct Subject {
    id: String,
    username: String,
}

impl Subject {
    fn pairs(&self) -> Vec<(String, String)> {
        vec![
            (self.id.clone(), "<subject>".to_owned()),
            (self.username.clone(), "<subject-name>".to_owned()),
        ]
    }
}

/// The post-family hooks the system messages a membership change writes fire **on the Go host
/// only**. Go's `postAddToChannelMessage` and its eighteen siblings go through the whole of
/// `a.CreatePost`, which dispatches `MessageWillBePosted` and `MessageHasBeenPosted`; this
/// server's `create_system_post` is a narrow slice of `CreatePost` that does not — [D-950].
///
/// Dropped from both sides so that this suite is about the membership hooks and fails for their
/// reasons. The gap is the debt entry's, and it was found here: it is invisible without a plugin.
const SYSTEM_POST_HOOKS: [&str; 2] = ["MessageWillBePosted", "MessageHasBeenPosted"];

/// How long both transcripts must stay unchanged before they are judged complete.
///
/// The membership paths fire a *variable* number of hooks — a channel add is followed by a system
/// post, which fires two more — so waiting for a count means writing that count down twice. This
/// waits for quiescence instead and then insists the two sides agree, which is the assertion that
/// matters and which a wrong count would hide.
const QUIET: Duration = Duration::from_millis(600);

/// One action against both servers, each with its own subject.
struct MemberPair {
    client: reqwest::Client,
    go_base: String,
    rust_base: String,
    go_log: PathBuf,
    rust_log: PathBuf,
    go_scrub: Vec<(String, String)>,
    rust_scrub: Vec<(String, String)>,
    seen: usize,
}

impl MemberPair {
    /// The same request to each server, with each side's own subject in the path or the body.
    async fn each(
        &self,
        method: reqwest::Method,
        token: &str,
        go: (String, Option<Vec<u8>>),
        rust: (String, Option<Vec<u8>>),
    ) -> ((u16, Json), (u16, Json)) {
        self.each_as(method, Some(token), go, rust).await
    }

    /// [`MemberPair::each`] with an optional token — a login is sent with none.
    async fn each_as(
        &self,
        method: reqwest::Method,
        token: Option<&str>,
        go: (String, Option<Vec<u8>>),
        rust: (String, Option<Vec<u8>>),
    ) -> ((u16, Json), (u16, Json)) {
        let decode = |bytes: Vec<u8>, pairs: &[(String, String)]| -> Json {
            let text = scrub(&String::from_utf8_lossy(&bytes), pairs);
            serde_json::from_str(&text).unwrap_or_else(|_| Json::String(text.trim_end().to_owned()))
        };
        let (gs, gb, _) = request_raw(
            &self.client,
            &self.go_base,
            method.clone(),
            token,
            &go.0,
            go.1.as_deref(),
        )
        .await;
        let (rs, rb, served_by) = request_raw(
            &self.client,
            &self.rust_base,
            method,
            token,
            &rust.0,
            rust.1.as_deref(),
        )
        .await;
        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{} was forwarded",
            rust.0
        );
        (
            (gs, decode(gb, &self.go_scrub)),
            (rs, decode(rb, &self.rust_scrub)),
        )
    }

    /// Wait until both transcripts have stopped growing, then assert they agree entry for entry
    /// and return what this step added.
    async fn hooks(&mut self, what: &str) -> Vec<Json> {
        let without_system_posts = |entries: Vec<Json>| -> Vec<Json> {
            entries
                .into_iter()
                .filter(|e| {
                    !e["hook"]
                        .as_str()
                        .is_some_and(|h| SYSTEM_POST_HOOKS.contains(&h))
                })
                .collect()
        };
        let read = || {
            (
                without_system_posts(transcript_of(&self.go_log, &self.go_scrub)),
                without_system_posts(transcript_of(&self.rust_log, &self.rust_scrub)),
            )
        };
        let (mut go, mut rust) = read();
        let mut quiet_since = std::time::Instant::now();
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (g, r) = read();
            if g.len() == go.len() && r.len() == rust.len() {
                if quiet_since.elapsed() >= QUIET && g.len() == r.len() && g.len() > self.seen {
                    break;
                }
            } else {
                quiet_since = std::time::Instant::now();
            }
            go = g;
            rust = r;
        }
        assert_eq!(
            names(&go),
            names(&rust),
            "{what}: the hooks that fired\n  go:   {:?}\n  rust: {:?}",
            names(&go),
            names(&rust)
        );
        for (index, (g, r)) in go.iter().zip(rust.iter()).enumerate() {
            assert_eq!(g, r, "{what}: hook {index} differs");
        }
        let fresh = go[self.seen..].to_vec();
        self.seen = go.len();
        fresh
    }

    /// Wait for quiescence and assert **nothing** fired — for the two rejections, where the plugin
    /// is called and the request then dies before any notification hook.
    async fn no_more_hooks(&mut self, what: &str) {
        tokio::time::sleep(QUIET).await;
        let keep = |entries: Vec<Json>| -> Vec<Json> {
            entries
                .into_iter()
                .filter(|e| {
                    !e["hook"]
                        .as_str()
                        .is_some_and(|h| SYSTEM_POST_HOOKS.contains(&h))
                })
                .collect()
        };
        let go = keep(transcript_of(&self.go_log, &self.go_scrub));
        let rust = keep(transcript_of(&self.rust_log, &self.rust_scrub));
        assert_eq!(go.len(), self.seen, "{what}: Go fired {:?}", names(&go));
        assert_eq!(
            rust.len(),
            self.seen,
            "{what}: Rust fired {:?}",
            names(&rust)
        );
    }
}

/// The plain users this suite creates, so the cleanup can find them after a panic.
static MEMBERSHIP_USERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Cross-server parity for the six channel and team membership hook sites
/// (docs/PLUGIN_PLAN.md, Phase 5; [D-932]).
///
/// `ChannelMemberWillBeAdded`, `UserHasJoinedChannel`, `UserHasLeftChannel`,
/// `TeamMemberWillBeAdded`, `UserHasJoinedTeam` and `UserHasLeftTeam`, each under a real Go host
/// and the Rust host, with the same plugin binary and the same client action.
///
/// The two `*WillBeAdded` hooks are the ones that can change the answer, and the recorder drives
/// both branches off ids the test plants in the environment: one channel and one team it refuses,
/// one of each where it answers a member carrying **only** `SchemeAdmin` so that the host's
/// gob merge is what fills the rest back in.
#[tokio::test]
async fn the_membership_hooks_fire_as_go_fires_them() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_membership_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    let users = std::mem::take(
        &mut *MEMBERSHIP_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for id in users {
        common::delete_plain_user(&client, &admin, &id).await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn run_the_membership_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-members");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;

    // Every fixture is made through **main** Go, which hosts no plugins, so none of this setup
    // reaches the recorder.
    let home = common::create_team(&client, &admin, "hookmb").await;
    let channel = common::create_channel(&client, &admin, &home, "hookmb").await;
    let reject_channel = common::create_channel(&client, &admin, &home, "hookrj").await;
    let admin_channel = common::create_channel(&client, &admin, &home, "hookad").await;
    // A channel of its own for the removal, whose two subjects are added through **main** Go.
    // Adding them through the pair instead would leave the Go side's member carrying the
    // `MentionCount` of 1 that `system_add_to_channel`'s implicit mention gives the added user
    // and the Rust side's carrying 0 — [D-235], a gap in the notification port, not in the hook.
    let leave_channel = common::create_channel(&client, &admin, &home, "hooklv").await;
    let join_team = common::create_team(&client, &admin, "hookjn").await;
    let reject_team = common::create_team(&client, &admin, "hookrt").await;
    let admin_team = common::create_team(&client, &admin, "hookat").await;

    let actor = common::create_plain_user(&client, &admin, &home, "hookact").await;
    let go_user = common::create_plain_user(&client, &admin, &home, "hookgo").await;
    let rs_user = common::create_plain_user(&client, &admin, &home, "hookrs").await;
    MEMBERSHIP_USERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend([actor.id.clone(), go_user.id.clone(), rs_user.id.clone()]);

    // The actor drives every request, and it is one account rather than two because a
    // `model.User` crossing to a plugin carries `Password` — a bcrypt hash no two accounts share.
    // `manage_public_channel_members` is channel-scoped, so it has to be **in** each channel.
    for id in [&channel, &reject_channel, &admin_channel, &leave_channel] {
        for user in [&actor.id, &go_user.id, &rs_user.id] {
            if user != &actor.id && id != &leave_channel {
                continue;
            }
            let joined = client
                .post(format!("{GO}/api/v4/channels/{id}/members"))
                .bearer_auth(&admin)
                .json(&serde_json::json!({ "user_id": user }))
                .send()
                .await
                .expect("Go answers");
            assert!(joined.status().is_success(), "{user} joins {id}");
        }
    }
    // It needs `team_admin` on the three teams to add and remove members there.
    for team in [&join_team, &reject_team, &admin_team] {
        let joined = client
            .post(format!("{GO}/api/v4/teams/{team}/members"))
            .bearer_auth(&admin)
            .json(&serde_json::json!({ "team_id": team, "user_id": actor.id }))
            .send()
            .await
            .expect("Go answers");
        assert!(joined.status().is_success(), "the actor joins {team}");
        let promoted = client
            .put(format!(
                "{GO}/api/v4/teams/{team}/members/{}/roles",
                actor.id
            ))
            .bearer_auth(&admin)
            .json(&serde_json::json!({ "roles": "team_user team_admin" }))
            .send()
            .await
            .expect("Go answers");
        assert!(
            promoted.status().is_success(),
            "the actor is admin of {team}"
        );
    }
    // The roles live on the **session** row, copied at login, so the token has to be minted again
    // after the promotion or every team request is a 403 from Go.
    let actor_token = common::login_plain_user(&client, "hookact").await;

    let plugin_env: Vec<(&str, String)> = vec![
        ("HOOK_RECORDER_REJECT_CHANNEL", reject_channel.clone()),
        ("HOOK_RECORDER_ADMIN_CHANNEL", admin_channel.clone()),
        ("HOOK_RECORDER_REJECT_TEAM", reject_team.clone()),
        ("HOOK_RECORDER_ADMIN_TEAM", admin_team.clone()),
    ];

    let mut go_env: Vec<(&str, &str)> = vec![("HOOK_RECORDER_TRANSCRIPT", "")];
    let go_transcript = go_log.to_string_lossy().into_owned();
    go_env[0].1 = go_transcript.as_str();
    for (key, value) in &plugin_env {
        go_env.push((key, value.as_str()));
    }
    let go = start_go(&go_run, &go_env, MEMBERSHIP_GO_OFFSET).await;

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let mut rust_env: Vec<(&str, &str)> = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    for (key, value) in &plugin_env {
        rust_env.push((key, value.as_str()));
    }
    let rust = SecondServer::start_in(MEMBERSHIP_HOST_PORT, &rs_run, &rust_env)
        .await
        .expect("the Rust host starts");

    wait_until_running(&client, &admin, &go.base).await;
    wait_until_running(&client, &admin, &rust.base).await;

    let go_subject = Subject {
        id: go_user.id.clone(),
        username: common::plain_username("hookgo"),
    };
    let rs_subject = Subject {
        id: rs_user.id.clone(),
        username: common::plain_username("hookrs"),
    };
    let mut pair = MemberPair {
        client: client.clone(),
        go_base: go.base.clone(),
        rust_base: rust.base.clone(),
        go_log,
        rust_log,
        go_scrub: go_subject.pairs(),
        rust_scrub: rs_subject.pairs(),
        seen: 0,
    };

    let member_body = |user: &str, channel: &str| {
        Some(
            serde_json::to_vec(&serde_json::json!({ "user_id": user, "channel_id": channel }))
                .expect("a body"),
        )
    };
    let add_to = |channel: &str| {
        (
            (
                format!("/api/v4/channels/{channel}/members"),
                member_body(&go_user.id, channel),
            ),
            (
                format!("/api/v4/channels/{channel}/members"),
                member_body(&rs_user.id, channel),
            ),
        )
    };

    // 1. A plain channel add. The hook that can refuse runs first, then the notification, then
    //    the system post `PostAddToChannelMessage` writes — which is itself two post hooks, and
    //    their presence here is what shows the two families interleave in Go's order.
    let (go_call, rust_call) = add_to(&channel);
    let ((gs, gb), (rs, rb)) = pair
        .each(reqwest::Method::POST, &actor_token, go_call, rust_call)
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a channel member");
    let fired = pair.hooks("a channel add").await;
    assert_eq!(
        &names(&fired)[..2],
        ["ChannelMemberWillBeAdded", "UserHasJoinedChannel"],
        "Go's order, and the whole sequence was {:?}",
        names(&fired)
    );
    assert_eq!(fired[0]["args"]["B"]["ChannelId"], channel);
    assert_eq!(fired[0]["args"]["B"]["UserId"], "<subject>");
    // The member the `Will` hook sees has no `LastUpdateAt` — it has not been saved — and the one
    // the notification sees does. Without this the two could be swapped unnoticed.
    assert_eq!(
        fired[0]["args"]["B"]["LastUpdateAt"],
        Json::Null,
        "gob omits a zero LastUpdateAt on the unsaved member"
    );
    assert_eq!(fired[1]["args"]["B"]["LastUpdateAt"], "<set>");
    // `C` is the actor: `opts.UserRequestorID`, the session's user, and the whole 34-field
    // `model.User` — unsanitised, as Go sends it.
    assert_eq!(
        fired[1]["args"]["C"]["Username"],
        common::plain_username("hookact")
    );
    assert!(
        fired[1]["args"]["C"]["Password"].is_string(),
        "Go sends the stored hash: the hook's user is not sanitised"
    );

    // 2. A refused add: the reason is a translation **parameter** here, not part of the id the way
    //    the post hooks build theirs.
    let (go_call, rust_call) = add_to(&reject_channel);
    let ((gs, gb), (rs, rb)) = pair
        .each(reqwest::Method::POST, &actor_token, go_call, rust_call)
        .await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a channel add the plugin refused",
    );
    assert_eq!(
        gb["id"],
        "app.channel.add_user.to.channel.rejected_by_plugin"
    );
    let fired = pair.hooks("a refused channel add").await;
    assert_eq!(names(&fired), ["ChannelMemberWillBeAdded"]);
    pair.no_more_hooks("a refused channel add writes nothing")
        .await;

    // 3. A replacement carrying one field: `SchemeAdmin`, which the merge folds into the member
    //    the host sent, so the row that lands has the plugin's flag and the host's everything
    //    else. The roles column is derived from the flag, so both are on the wire.
    let (go_call, rust_call) = add_to(&admin_channel);
    let ((gs, gb), (rs, rb)) = pair
        .each(reqwest::Method::POST, &actor_token, go_call, rust_call)
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a member the plugin promoted");
    assert_eq!(gb["scheme_admin"], true, "the merge kept the plugin's flag");
    assert_eq!(gb["roles"], "channel_user channel_admin");
    assert_eq!(rb["roles"], "channel_user channel_admin");
    // The notify props came back on a member the plugin only set one field of.
    assert_eq!(gb["notify_props"]["desktop"], "default");
    pair.hooks("a promoted channel add").await;

    // 4. A removal. The member is read before the delete and handed to the hook, and the actor is
    //    the remover.
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::DELETE,
            &actor_token,
            (
                format!("/api/v4/channels/{leave_channel}/members/{}", go_user.id),
                None,
            ),
            (
                format!("/api/v4/channels/{leave_channel}/members/{}", rs_user.id),
                None,
            ),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks("a channel removal").await;
    assert_eq!(names(&fired)[0], "UserHasLeftChannel");
    assert_eq!(fired[0]["args"]["B"]["UserId"], "<subject>");
    assert_eq!(
        fired[0]["args"]["C"]["Username"],
        common::plain_username("hookact"),
        "the actor is the remover"
    );

    // 5. A team join. `AddTeamMember` passes an **empty** requestor, so `UserHasJoinedTeam`'s
    //    actor is nil whoever asked — and a nil pointer is a field gob omits entirely.
    let team_body = |user: &str, team: &str| {
        Some(
            serde_json::to_vec(&serde_json::json!({ "team_id": team, "user_id": user }))
                .expect("a body"),
        )
    };
    let join = |team: &str| {
        (
            (
                format!("/api/v4/teams/{team}/members"),
                team_body(&go_user.id, team),
            ),
            (
                format!("/api/v4/teams/{team}/members"),
                team_body(&rs_user.id, team),
            ),
        )
    };
    let (go_call, rust_call) = join(&join_team);
    let ((gs, gb), (rs, rb)) = pair
        .each(reqwest::Method::POST, &actor_token, go_call, rust_call)
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a team member");
    let fired = pair.hooks("a team join").await;
    assert_eq!(
        &names(&fired)[..2],
        ["TeamMemberWillBeAdded", "UserHasJoinedTeam"],
        "Go's order, and the whole sequence was {:?}",
        names(&fired)
    );
    assert_eq!(fired[0]["args"]["B"]["TeamId"], join_team);
    assert_eq!(fired[0]["args"]["B"]["UserId"], "<subject>");
    assert_eq!(
        fired[1]["args"]["C"],
        Json::Null,
        "AddTeamMember's requestor is empty, so the actor is nil and gob omits it"
    );

    // 6. A refused join.
    let (go_call, rust_call) = join(&reject_team);
    let ((gs, gb), (rs, rb)) = pair
        .each(reqwest::Method::POST, &actor_token, go_call, rust_call)
        .await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a team join the plugin refused",
    );
    assert_eq!(gb["id"], "app.team.join_user_to_team.rejected_by_plugin");
    let fired = pair.hooks("a refused team join").await;
    assert_eq!(names(&fired), ["TeamMemberWillBeAdded"]);
    pair.no_more_hooks("a refused team join writes nothing")
        .await;

    // 7. A replacement on the team side, the same shape as 3.
    let (go_call, rust_call) = join(&admin_team);
    let ((gs, gb), (rs, rb)) = pair
        .each(reqwest::Method::POST, &actor_token, go_call, rust_call)
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a team member the plugin promoted");
    assert_eq!(gb["scheme_admin"], true, "the merge kept the plugin's flag");
    assert_eq!(gb["roles"], "team_user team_admin");
    pair.hooks("a promoted team join").await;

    // 8. A team departure. `UserHasLeftTeam` is the first statement of
    //    `postProcessTeamMemberLeave`, so it fires before the three writes that can fail it.
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::DELETE,
            &actor_token,
            (
                format!("/api/v4/teams/{join_team}/members/{}", go_user.id),
                None,
            ),
            (
                format!("/api/v4/teams/{join_team}/members/{}", rs_user.id),
                None,
            ),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks("a team departure").await;
    assert!(
        names(&fired).contains(&"UserHasLeftTeam".to_owned()),
        "the departure fired {:?}",
        names(&fired)
    );
    let left = fired
        .iter()
        .find(|e| e["hook"] == "UserHasLeftTeam")
        .expect("the hook");
    assert_eq!(left["args"]["B"]["TeamId"], join_team);
    assert_eq!(left["args"]["B"]["UserId"], "<subject>");
    assert_eq!(
        left["args"]["C"]["Username"],
        common::plain_username("hookact"),
        "the actor is the requestor of the removal"
    );

    // 9. A rejoin. A deleted membership is **revived**, and `applyPreSaveHooks` runs on that
    //    branch too (app/teams/teams.go:226) — after the member-count check, not before it. A
    //    port that hung the hook off the insert alone passes every test above and silently stops
    //    calling the plugin for the one case a plugin most wants: somebody coming back.
    let (go_call, rust_call) = join(&join_team);
    let ((gs, gb), (rs, rb)) = pair
        .each(reqwest::Method::POST, &actor_token, go_call, rust_call)
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a revived team member");
    let fired = pair.hooks("a team rejoin").await;
    assert_eq!(
        &names(&fired)[..2],
        ["TeamMemberWillBeAdded", "UserHasJoinedTeam"],
        "the revival path runs both, and the whole sequence was {:?}",
        names(&fired)
    );

    drop(rust);
    drop(go);
    for id in [&channel, &reject_channel, &admin_channel, &leave_channel] {
        common::delete_channel(&client, &admin, id).await;
    }
}

/// What the lifecycle tour creates, so the cleanup can find it after a panic: plain users by id,
/// and planted bots by id.
static LIFECYCLE_USERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static LIFECYCLE_BOTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Cross-server parity for the four user lifecycle hook sites (docs/PLUGIN_PLAN.md, Phase 5;
/// [D-932]): `UserHasBeenCreated`, `UserWillLogIn`, `UserHasLoggedIn` and
/// `UserHasBeenDeactivated`, on the admin create, `POST /users/login`, `DELETE /users/{id}`,
/// `PUT /users/{id}/active` and `POST /bots/{id}/disable`.
///
/// Creations and deactivations cannot be repeated on a second server, so each side acts on a
/// user of its own and the transcripts are scrubbed, as in the membership tour. Logins can, so
/// **one** account logs in to both — which is what lets the unsanitised row, bcrypt hash and all,
/// be compared exactly.
#[tokio::test]
async fn the_user_lifecycle_hooks_fire_as_go_fires_them() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_lifecycle_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    let users = std::mem::take(
        &mut *LIFECYCLE_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for id in users {
        common::delete_plain_user(&client, &admin, &id).await;
    }
    let bots = std::mem::take(
        &mut *LIFECYCLE_BOTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for id in bots {
        common::unplant_bot(&id).await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn run_the_lifecycle_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-lifecycle");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;

    // Fixtures through **main** Go, which hosts no plugins. `create_plain_user` logs each account
    // in once, so the login account's `LastLogin` is already set when the pair first reads it.
    let home = common::create_team(&client, &admin, "hooklc").await;
    let login_user = common::create_plain_user(&client, &admin, &home, "lclogin").await;
    let reject_user = common::create_plain_user(&client, &admin, &home, "lcreject").await;
    LIFECYCLE_USERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend([login_user.id.clone(), reject_user.id.clone()]);

    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(&admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let admin_id = me["id"].as_str().expect("an id").to_owned();
    let go_bot = common::plant_bot("lcbotgo", &admin_id, 0)
        .await
        .expect("a bot");
    let rs_bot = common::plant_bot("lcbotrs", &admin_id, 0)
        .await
        .expect("a bot");
    LIFECYCLE_BOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend([go_bot.clone(), rs_bot.clone()]);

    let reject_env = [("HOOK_RECORDER_REJECT_USER", reject_user.id.as_str())];
    let go_transcript = go_log.to_string_lossy().into_owned();
    let mut go_env: Vec<(&str, &str)> = vec![("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())];
    go_env.extend(reject_env);
    let go = start_go(&go_run, &go_env, LIFECYCLE_GO_OFFSET).await;

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let mut rust_env: Vec<(&str, &str)> = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    rust_env.extend(reject_env);
    let rust = SecondServer::start_in(LIFECYCLE_HOST_PORT, &rs_run, &rust_env)
        .await
        .expect("the Rust host starts");

    wait_until_running(&client, &admin, &go.base).await;
    wait_until_running(&client, &admin, &rust.base).await;

    let (go_name, rs_name) = (
        common::plain_username("lcnewgo"),
        common::plain_username("lcnewrs"),
    );
    let mut pair = MemberPair {
        client: client.clone(),
        go_base: go.base.clone(),
        rust_base: rust.base.clone(),
        go_log,
        rust_log,
        go_scrub: vec![
            (go_bot.clone(), "<bot>".to_owned()),
            ("lcbotgo".to_owned(), "<bot-tag>".to_owned()),
            (go_name.clone(), "<subject-name>".to_owned()),
        ],
        rust_scrub: vec![
            (rs_bot.clone(), "<bot>".to_owned()),
            ("lcbotrs".to_owned(), "<bot-tag>".to_owned()),
            (rs_name.clone(), "<subject-name>".to_owned()),
        ],
        seen: 0,
    };
    let body = |value: Json| Some(serde_json::to_vec(&value).expect("a body"));

    // 1. An admin creates an account. `UserHasBeenCreated` carries `ruser` after
    //    `userService.createUser` sanitised it: no password, no auth data.
    let new_user = |name: &str| {
        (
            "/api/v4/users".to_owned(),
            body(serde_json::json!({
                "username": name,
                "email": format!("{name}@mmrs.invalid"),
                "password": common::PLAIN_USER_PASSWORD,
            })),
        )
    };
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            new_user(&go_name),
            new_user(&rs_name),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    let (go_new, rs_new) = (
        gb["id"].as_str().expect("an id").to_owned(),
        rb["id"].as_str().expect("an id").to_owned(),
    );
    LIFECYCLE_USERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend([go_new.clone(), rs_new.clone()]);
    pair.go_scrub
        .insert(0, (go_new.clone(), "<subject>".to_owned()));
    pair.rust_scrub
        .insert(0, (rs_new.clone(), "<subject>".to_owned()));
    // Stamped by each server's own save.
    let (mut go_created, mut rs_created) = (gb.clone(), rb.clone());
    for body in [&mut go_created, &mut rs_created] {
        body.as_object_mut()
            .map(|m| m.remove("last_password_update"));
    }
    same_post(&go_created, &rs_created, "a created user");
    let fired = pair.hooks("an admin create").await;
    assert_eq!(names(&fired), ["UserHasBeenCreated"]);
    assert_eq!(fired[0]["args"]["B"]["Id"], "<id>");
    assert_eq!(fired[0]["args"]["B"]["Username"], "<subject-name>");
    assert_eq!(
        fired[0]["args"]["B"]["Password"],
        Json::Null,
        "the created user is sanitised, and gob omits the empty hash"
    );
    assert_eq!(fired[0]["args"]["B"]["Roles"], "system_user");

    // 2. The shared account logs in to each server with no session. The two hooks bracket
    //    `DoLogin`, and both carry the row as it was read — the stored hash included.
    let login = |name: &str, device: Option<&str>| {
        let mut value = serde_json::json!({
            "login_id": name,
            "password": common::PLAIN_USER_PASSWORD,
        });
        if let Some(device) = device {
            value["device_id"] = Json::from(device);
        }
        ("/api/v4/users/login".to_owned(), body(value))
    };
    let login_name = common::plain_username("lclogin");
    let ((gs, mut gb), (rs, mut rb)) = pair
        .each_as(
            reqwest::Method::POST,
            None,
            login(&login_name, None),
            login(&login_name, None),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    // The second login reads the `LastLogin` the first one wrote.
    for body in [&mut gb, &mut rb] {
        body.as_object_mut().map(|m| m.remove("last_login"));
    }
    same_post(&gb, &rb, "the logged-in user");
    let fired = pair.hooks("a login").await;
    assert_eq!(names(&fired), ["UserWillLogIn", "UserHasLoggedIn"]);
    for entry in &fired {
        assert_eq!(entry["args"]["B"]["Id"], "<id>");
        assert!(
            entry["args"]["B"]["Password"].is_string(),
            "{}: the login hooks see the stored row, hash and all",
            entry["hook"]
        );
        assert_eq!(
            entry["args"]["A"]["SessionId"],
            Json::Null,
            "{}: a login sent with no token has no session in its context",
            entry["hook"]
        );
    }

    // 3. The same login sent **with** a session. `APIHandler` resolves it, so the context carries
    //    its id — the caller's session, never the one the login creates.
    let ((gs, gb), (rs, rb)) = pair
        .each_as(
            reqwest::Method::POST,
            Some(&admin),
            login(&login_name, None),
            login(&login_name, None),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    let fired = pair.hooks("a login from a live session").await;
    assert_eq!(names(&fired), ["UserWillLogIn", "UserHasLoggedIn"]);
    // `hooks` has already compared the two contexts field for field; what is left is that the
    // field is filled at all — a context built with no session is `""` on both sides and would
    // pass that comparison.
    for entry in &fired {
        let id = entry["args"]["A"]["SessionId"].as_str().unwrap_or_default();
        assert_eq!(
            id.len(),
            26,
            "{}: the context carries the caller's session, got {id:?}",
            entry["hook"]
        );
    }

    // 4. A login the plugin refuses. `Login rejected by plugin: <reason>` is the error id, and
    //    `login`'s mask turns it into an ordinary bad-credentials 401 on both.
    let reject_name = common::plain_username("lcreject");
    let ((gs, gb), (rs, rb)) = pair
        .each_as(
            reqwest::Method::POST,
            None,
            login(&reject_name, None),
            login(&reject_name, None),
        )
        .await;
    assert_eq!((gs, rs), (401, 401), "Go {gb} / Rust {rb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a login the plugin refused",
    );
    let fired = pair.hooks("a refused login").await;
    assert_eq!(names(&fired), ["UserWillLogIn"]);
    pair.no_more_hooks("a refused login never logs in").await;

    // 5. A malformed device id. `UserWillLogIn` runs **before** `DoLogin` validates it, so the
    //    plugin is asked about a login that then fails for a reason of its own.
    let ((gs, gb), (rs, rb)) = pair
        .each_as(
            reqwest::Method::POST,
            None,
            login(&login_name, Some("not-a-device")),
            login(&login_name, Some("not-a-device")),
        )
        .await;
    assert_eq!((gs, rs), (401, 401), "Go {gb} / Rust {rb}");
    let fired = pair.hooks("a login with a bad device id").await;
    assert_eq!(names(&fired), ["UserWillLogIn"]);
    pair.no_more_hooks("a bad device id never logs in").await;

    // 6. A deactivation through `DELETE`. The user the hook sees is the one the store's `Update`
    //    left behind: sanitised, with `DeleteAt` set.
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::DELETE,
            &admin,
            (format!("/api/v4/users/{go_new}"), None),
            (format!("/api/v4/users/{rs_new}"), None),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks("a deactivation").await;
    assert_eq!(names(&fired), ["UserHasBeenDeactivated"]);
    assert_eq!(fired[0]["args"]["B"]["Id"], "<id>");
    assert_eq!(fired[0]["args"]["B"]["DeleteAt"], "<set>");
    assert_eq!(fired[0]["args"]["B"]["Password"], Json::Null, "sanitised");

    // 7. Reactivation fires nothing — the hook is `!active && DeleteAt != 0` — and deactivating
    //    again through `PUT /active` fires it once more.
    let active = |on: bool| body(serde_json::json!({ "active": on }));
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::PUT,
            &admin,
            (format!("/api/v4/users/{go_new}/active"), active(true)),
            (format!("/api/v4/users/{rs_new}/active"), active(true)),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    pair.no_more_hooks("a reactivation").await;
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::PUT,
            &admin,
            (format!("/api/v4/users/{go_new}/active"), active(false)),
            (format!("/api/v4/users/{rs_new}/active"), active(false)),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    let fired = pair.hooks("a deactivation through /active").await;
    assert_eq!(names(&fired), ["UserHasBeenDeactivated"]);

    // 8. Disabling a bot is `UpdateActive(bot, false)` in Go, so it tells every plugin a user was
    //    deactivated.
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            (format!("/api/v4/bots/{go_bot}/disable"), None),
            (format!("/api/v4/bots/{rs_bot}/disable"), None),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    let fired = pair.hooks("a bot disabled").await;
    assert_eq!(names(&fired), ["UserHasBeenDeactivated"]);
    assert_eq!(fired[0]["args"]["B"]["Id"], "<id>");
    assert_eq!(fired[0]["args"]["B"]["IsBot"], true);

    // 9. A preference save, as the account that logged in. Both servers write the same row, so
    //    the batch the hook sees is compared as it is.
    let batch = body(serde_json::json!([{
        "user_id": login_user.id,
        "category": "display_settings",
        "name": "hooktour",
        "value": "on",
    }]));
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::PUT,
            &login_user.token,
            ("/api/v4/users/me/preferences".to_owned(), batch.clone()),
            ("/api/v4/users/me/preferences".to_owned(), batch.clone()),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks("a preference save").await;
    assert_eq!(names(&fired), ["PreferencesHaveChanged"]);
    assert_eq!(fired[0]["args"]["B"][0]["Name"], "hooktour");

    // 10. A custom status, which saves the recent statuses through the same `UpdatePreferences`.
    let status = body(serde_json::json!({ "emoji": "smile", "text": "from the hook tour" }));
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::PUT,
            &login_user.token,
            ("/api/v4/users/me/status/custom".to_owned(), status.clone()),
            ("/api/v4/users/me/status/custom".to_owned(), status),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    let fired = pair.hooks("a custom status").await;
    assert_eq!(names(&fired), ["PreferencesHaveChanged"]);
    assert_eq!(fired[0]["args"]["B"][0]["Category"], "custom_status");

    // 11. A deletion fires nothing: Go has no hook for it.
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::POST,
            &login_user.token,
            (
                "/api/v4/users/me/preferences/delete".to_owned(),
                batch.clone(),
            ),
            ("/api/v4/users/me/preferences/delete".to_owned(), batch),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    pair.no_more_hooks("a preference deletion").await;

    drop(rust);
    drop(go);
}

/// Cross-server parity for `FileWillBeDownloaded` on the four read routes (docs/PLUGIN_PLAN.md,
/// Phase 5; [D-932]): `GET /api/v4/files/{id}`, `/thumbnail`, `/preview` and the unauthenticated
/// `GET /files/{id}/public`.
///
/// A download changes nothing, so both servers fetch the **same** files with the same session and
/// the transcripts are compared with nothing scrubbed. The recorder refuses any file whose name
/// starts with `hookreject`.
#[tokio::test]
async fn the_download_hook_fires_as_go_fires_it() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_download_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// One `GET` to each server, answering status, the `X-Reject-Reason` header, the content type
/// and the body. `served_by` is asserted on the Rust side, as `MemberPair::each` does.
async fn fetch_both(
    client: &reqwest::Client,
    token: Option<&str>,
    go_base: &str,
    rust_base: &str,
    path: &str,
) -> [(u16, Option<String>, String, Vec<u8>); 2] {
    let mut out = Vec::new();
    for base in [go_base, rust_base] {
        let mut request = client.get(format!("{base}{path}"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.expect("the server answers");
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        if base == rust_base {
            common::assert_served_by_rust(response.headers(), path);
        }
        let status = response.status().as_u16();
        let reason = header("X-Reject-Reason");
        let content_type = header("Content-Type").unwrap_or_default();
        let body = response.bytes().await.expect("a body").to_vec();
        out.push((status, reason, content_type, body));
    }
    let rust = out.pop().expect("two answers");
    let go = out.pop().expect("two answers");
    [go, rust]
}

/// A `RenderWebAppError` page with each signature replaced by `<sig>`. The signature is ECDSA,
/// randomised in Go, so it differs on every request; everything around it must not. It appears
/// escaped for JavaScript (`\u0026s\u003D`) and for HTML (`&amp;s=`), and its base64 padding is
/// escaped the same way.
fn unsigned_page(page: &str) -> String {
    const MARKERS: [&str; 3] = [r"\u0026s\u003D", "&amp;s=", "&s="];
    const PADDING: [&str; 3] = [r"\u003D", "%3D", "="];
    let mut out = String::new();
    let mut rest = page;
    loop {
        let next = MARKERS
            .iter()
            .filter_map(|m| rest.find(m).map(|at| (at, *m)))
            .min_by_key(|(at, _)| *at);
        let Some((at, marker)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at + marker.len()]);
        out.push_str("<sig>");
        rest = rest[at + marker.len()..]
            .trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        while let Some(pad) = PADDING.iter().find(|p| rest.starts_with(**p)) {
            rest = &rest[pad.len()..];
        }
    }
}

async fn run_the_download_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-downloads");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;

    let team = common::create_team(&client, &admin, "hookdl").await;
    let channel = common::create_channel(&client, &admin, &team, "hookdl").await;
    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(&admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let admin_id = me["id"].as_str().expect("an id").to_owned();

    let go_transcript = go_log.to_string_lossy().into_owned();
    let go_env: Vec<(&str, &str)> = vec![
        ("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str()),
        ("MM_FILESETTINGS_ENABLEPUBLICLINK", "true"),
    ];
    let go = start_go(&go_run, &go_env, DOWNLOAD_GO_OFFSET).await;
    wait_until_running(&client, &admin, &go.base).await;

    // The files go in through the **pair's** Go server, into its file store, and the Rust host is
    // pointed at the same directory — which also holds the bundle, so it installs the same
    // plugin. The recorder does not implement `FileWillBeUploaded`, so the uploads leave no
    // transcript.
    let upload = |name: &'static str, content_type: &'static str, bytes: &'static [u8]| {
        let (client, admin, base, channel) = (
            client.clone(),
            admin.clone(),
            go.base.clone(),
            channel.clone(),
        );
        async move {
            let response = client
                .post(format!(
                    "{base}/api/v4/files?channel_id={channel}&filename={name}"
                ))
                .bearer_auth(&admin)
                .header("Content-Type", content_type)
                .body(bytes)
                .send()
                .await
                .expect("Go answers");
            assert!(response.status().is_success(), "uploading {name}");
            let uploaded: Json = response.json().await.expect("the upload decodes");
            uploaded["file_infos"][0]["id"]
                .as_str()
                .expect("an id")
                .to_owned()
        }
    };
    let image = upload("hookplain.png", "image/png", common::TINY_PNG).await;
    let text = upload(
        "hookplain.txt",
        "text/plain",
        b"a file the recorder lets through",
    )
    .await;
    let refused = upload(
        "hookreject.txt",
        "text/plain",
        b"a file the recorder withholds",
    )
    .await;
    // `getFileLink` refuses a file no post claims, so the two the public tour fetches are
    // attached — through **main** Go, which hosts no plugins, so the post fires nothing here.
    common::post_message_with_files(
        &client,
        &admin,
        &channel,
        "files for the public tour",
        &[image.clone(), refused.clone()],
    )
    .await;
    // The three uploads went through a server hosting the recorder, which records
    // `FileWillBeUploaded`; this tour is about downloads, so they are wiped before either side
    // is compared.
    assert_eq!(
        names(&transcript(&go_log)),
        ["FileWillBeUploaded"; 3],
        "only the uploads reached the recorder"
    );
    std::fs::write(&go_log, "").expect("the transcript is reset");

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir) = (s("plugins"), s("client"));
    let data = format!("{}/", go_run.join("data").to_string_lossy());
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let rust_env: Vec<(&str, &str)> = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("MM_FILESETTINGS_ENABLEPUBLICLINK", "true"),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    let rust = SecondServer::start_in(DOWNLOAD_HOST_PORT, &rs_run, &rust_env)
        .await
        .expect("the Rust host starts");
    wait_until_running(&client, &admin, &rust.base).await;

    let mut pair = MemberPair {
        client: client.clone(),
        go_base: go.base.clone(),
        rust_base: rust.base.clone(),
        go_log,
        rust_log,
        go_scrub: Vec::new(),
        rust_scrub: Vec::new(),
        seen: 0,
    };
    let get = |path: String| {
        let (client, admin, go, rust) = (
            client.clone(),
            admin.clone(),
            go.base.clone(),
            rust.base.clone(),
        );
        async move { fetch_both(&client, Some(&admin), &go, &rust, &path).await }
    };

    // 1-3. The original and both derived images of an image. Each asks the plugin once, with the
    //      route's download type and the session's user.
    for (suffix, kind) in [
        ("", "file"),
        ("/thumbnail", "thumbnail"),
        ("/preview", "preview"),
    ] {
        let [g, r] = get(format!("/api/v4/files/{image}{suffix}")).await;
        assert_eq!((g.0, r.0), (200, 200), "{kind}");
        assert_eq!(g.3, r.3, "{kind}: the bytes");
        let fired = pair.hooks(kind).await;
        assert_eq!(names(&fired), ["FileWillBeDownloaded"], "{kind}");
        assert_eq!(fired[0]["args"]["D"], kind);
        assert_eq!(fired[0]["args"]["C"], admin_id.as_str());
        assert_eq!(fired[0]["args"]["B"]["Name"], "hookplain.png");
        assert!(
            fired[0]["args"]["B"]["Path"].is_string(),
            "{kind}: gob carries the json:\"-\" path"
        );
    }

    // 4. A preview of a file with none: the 400 comes **before** the plugin is asked.
    let [g, r] = get(format!("/api/v4/files/{text}/preview")).await;
    assert_eq!((g.0, r.0), (400, 400));
    pair.no_more_hooks("a preview that does not exist").await;

    // 5. A thumbnail of a file with none: the plugin is asked **first**, then the 400.
    let [g, r] = get(format!("/api/v4/files/{text}/thumbnail")).await;
    assert_eq!((g.0, r.0), (400, 400));
    let fired = pair.hooks("a thumbnail that does not exist").await;
    assert_eq!(names(&fired), ["FileWillBeDownloaded"]);

    // 6. A refusal: 403, the reason as a `Reason` parameter of a real key, and the same reason in
    //    `X-Reject-Reason`.
    let [g, r] = get(format!("/api/v4/files/{refused}")).await;
    assert_eq!((g.0, r.0), (403, 403));
    assert_eq!(
        g.1.as_deref(),
        Some("the hook recorder withholds this file")
    );
    assert_eq!(g.1, r.1, "X-Reject-Reason");
    common::assert_error_bodies_match_except_known_gaps(&g.3, &r.3, "a refused download");
    let body: Json = serde_json::from_slice(&g.3).expect("a JSON error");
    assert_eq!(body["id"], "api.file.get_file.rejected_by_plugin");
    let fired = pair.hooks("a refused download").await;
    assert_eq!(names(&fired), ["FileWillBeDownloaded"]);

    // 7-8. The public link, which has no session: the user is `""`, and a refusal is the signed
    //      HTML page. Its `s=` signature is ECDSA and differs on every request, so the page is
    //      compared up to it.
    let link_of = |id: String| {
        let (client, admin, go) = (client.clone(), admin.clone(), go.base.clone());
        async move {
            let link: Json = client
                .get(format!("{go}/api/v4/files/{id}/link"))
                .bearer_auth(&admin)
                .send()
                .await
                .expect("Go answers")
                .json()
                .await
                .expect("a link");
            let link = link["link"].as_str().expect("the link").to_owned();
            link[link.find("/files/").expect("a public path")..].to_owned()
        }
    };
    let public = link_of(image.clone()).await;
    let [g, r] = fetch_both(&client, None, &go.base, &rust.base, &public).await;
    assert_eq!((g.0, r.0), (200, 200));
    assert_eq!(g.3, r.3, "the public bytes");
    let fired = pair.hooks("a public download").await;
    assert_eq!(names(&fired), ["FileWillBeDownloaded"]);
    assert_eq!(fired[0]["args"]["D"], "public");
    assert_eq!(
        fired[0]["args"]["C"],
        Json::Null,
        "no user, and gob omits \"\""
    );
    assert_eq!(fired[0]["args"]["A"]["SessionId"], Json::Null);

    let public = link_of(refused.clone()).await;
    let [g, r] = fetch_both(&client, None, &go.base, &rust.base, &public).await;
    assert_eq!((g.0, r.0), (403, 403));
    assert_eq!(g.1, r.1, "X-Reject-Reason on the page");
    assert_eq!(g.2, r.2, "the page's content type");
    let (go_page, rust_page) = (
        unsigned_page(&String::from_utf8_lossy(&g.3)),
        unsigned_page(&String::from_utf8_lossy(&r.3)),
    );
    assert!(go_page.contains("<sig>"), "the page is signed: {go_page}");
    assert_eq!(go_page, rust_page, "the page, signatures aside");
    let fired = pair.hooks("a refused public download").await;
    assert_eq!(names(&fired), ["FileWillBeDownloaded"]);

    drop(rust);
    drop(go);
    common::delete_channel(&client, &admin, &channel).await;
}

/// Cross-server parity for `FileWillBeUploaded` (docs/PLUGIN_PLAN.md, Phase 5; [D-932]) on the
/// simple upload behind `POST /api/v4/files` and on the completing chunk of
/// `POST /api/v4/uploads/{upload_id}`.
///
/// Each server stores its own copy of each file, so ids and paths differ and are tokenised; the
/// recorder's entry also says what it could **read**, which is the whole file on the first path
/// and nothing at all on the second.
#[tokio::test]
async fn the_upload_hook_fires_as_go_fires_it() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_upload_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The bytes of the one file called `name` anywhere under `root`.
fn find_stored(root: &Path, name: &str) -> Option<Vec<u8>> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_stored(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return std::fs::read(&path).ok();
        }
    }
    None
}

async fn run_the_upload_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-uploads");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;
    let team = common::create_team(&client, &admin, "hookul").await;
    let channel = common::create_channel(&client, &admin, &team, "hookul").await;

    let go_transcript = go_log.to_string_lossy().into_owned();
    let go = start_go(
        &go_run,
        &[("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())],
        UPLOAD_GO_OFFSET,
    )
    .await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let rust = SecondServer::start_in(
        UPLOAD_HOST_PORT,
        &rs_run,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
            ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
            ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
        ],
    )
    .await
    .expect("the Rust host starts");
    wait_until_running(&client, &admin, &go.base).await;
    wait_until_running(&client, &admin, &rust.base).await;

    let mut pair = MemberPair {
        client: client.clone(),
        go_base: go.base.clone(),
        rust_base: rust.base.clone(),
        go_log,
        rust_log,
        go_scrub: Vec::new(),
        rust_scrub: Vec::new(),
        seen: 0,
    };
    let simple = |name: &str, bytes: &[u8]| {
        let call = (
            format!("/api/v4/files?channel_id={channel}&filename={name}"),
            Some(bytes.to_vec()),
        );
        (call.clone(), call)
    };
    // What each server stored, read from its own data directory rather than through a download,
    // which would fire `FileWillBeDownloaded` and carry the text Go extracts and this server does
    // not ([D-651]).
    let (go_data, rust_data) = (go_run.join("data"), rs_run.join("data"));
    let stored = |name: &str| {
        [&go_data, &rust_data].map(|root| {
            find_stored(root, name).unwrap_or_else(|| panic!("{name} under {}", root.display()))
        })
    };

    // 1. An upload the recorder leaves alone. It reads the whole file.
    let content = b"an upload the recorder leaves alone";
    let (g, r) = simple("hookplain.txt", content);
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a plain upload");
    let fired = pair.hooks("a plain upload").await;
    assert_eq!(names(&fired), ["FileWillBeUploaded"]);
    assert_eq!(
        fired[0]["args"]["read"],
        "an upload the recorder leaves alone"
    );
    assert_eq!(fired[0]["args"]["B"]["Name"], "hookplain.txt");

    // 2. A replacement: the size is the replacement's, and so are the stored bytes.
    let (g, r) = simple("hookreplace.txt", b"the bytes the client sent");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a replaced upload");
    assert_eq!(gb["file_infos"][0]["size"], 29, "the replacement's length");
    let [go_bytes, rust_bytes] = stored("hookreplace.txt");
    assert_eq!(go_bytes, b"replaced by the hook recorder");
    assert_eq!(rust_bytes, go_bytes, "the stored replacement");
    pair.hooks("a replaced upload").await;

    // 3. An answer carrying one field is **merged** into the upload's own file info.
    let (g, r) = simple("hookrename.txt", b"renamed on the way in");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a renamed upload");
    assert_eq!(gb["file_infos"][0]["name"], "renamed.txt");
    assert_eq!(gb["file_infos"][0]["extension"], "txt", "the rest survives");
    pair.hooks("a renamed upload").await;

    // 4. A refusal. The refusing answer is merged **before** the reason is looked at, so the
    //    error names the file by the plugin's name for it.
    let (g, r) = simple("hookrefuse.txt", b"turned away");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a refused upload",
    );
    assert_eq!(gb["id"], "app.upload.run_plugins_hook.rejected");
    assert!(
        gb["message"]
            .as_str()
            .is_some_and(|m| m.contains("renamed-on-reject.txt")),
        "{gb}"
    );
    let fired = pair.hooks("a refused upload").await;
    assert_eq!(names(&fired), ["FileWillBeUploaded"]);

    // 5. An image replaced by bytes that are not one. Go makes the thumbnails from storage
    //    after the hook, so there are none — and no mini preview.
    let (g, r) = simple("hookunimage.png", common::TINY_PNG);
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "an image replaced by a non-image");
    assert_eq!(
        gb["file_infos"][0]["mini_preview"],
        Json::Null,
        "the images come from the replacement"
    );
    pair.hooks("an image replaced by a non-image").await;

    // 6. A resumable upload. Go hands the plugins the reader it has already closed, so they read
    //    **nothing** — and can still replace the file.
    let session_of = |base: String| {
        let (client, admin, channel) = (client.clone(), admin.clone(), channel.clone());
        async move {
            let created: Json = client
                .post(format!("{base}/api/v4/uploads"))
                .bearer_auth(&admin)
                .json(&serde_json::json!({
                    "channel_id": channel,
                    "filename": "hookreplace-session.txt",
                    "file_size": 21,
                }))
                .send()
                .await
                .expect("the server answers")
                .json()
                .await
                .expect("an upload session");
            created["id"].as_str().expect("an upload id").to_owned()
        }
    };
    let (go_upload, rust_upload) = (
        session_of(go.base.clone()).await,
        session_of(rust.base.clone()).await,
    );
    let chunk = Some(b"a resumable upload!!!".to_vec());
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            (format!("/api/v4/uploads/{go_upload}"), chunk.clone()),
            (format!("/api/v4/uploads/{rust_upload}"), chunk),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a completed resumable upload");
    assert_eq!(gb["size"], 29, "the replacement's length");
    let fired = pair.hooks("a completed resumable upload").await;
    assert_eq!(names(&fired), ["FileWillBeUploaded"]);
    assert_eq!(
        fired[0]["args"]["read"], "",
        "the plugin is lent a reader Go has already closed"
    );

    drop(rust);
    drop(go);
    common::delete_channel(&client, &admin, &channel).await;
}

/// The plain users the channel tour creates, for the cleanup.
static CHANNEL_USERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Cross-server parity for the channel lifecycle hooks (docs/PLUGIN_PLAN.md, Phase 5;
/// [D-932]): `ChannelHasBeenCreated` on all three creation paths, `ChannelWillBeUpdated`,
/// `ChannelWillBeArchived` and `ChannelWillBeRestored`.
///
/// Each side creates channels of its own, so their ids and names are scrubbed — and a DM's or
/// GM's name is a function of its members' ids, so it is scrubbed whole rather than piecewise.
#[tokio::test]
async fn the_channel_hooks_fire_as_go_fires_them() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_channel_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    let users = std::mem::take(
        &mut *CHANNEL_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for id in users {
        common::delete_plain_user(&client, &admin, &id).await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Create an open channel for each side, scrub its id and name, and return (Go's id, Rust's id).
///
/// With `through_main_go` both are created by the stack's Go server, which hosts no plugins, so
/// nothing is recorded and both rows carry Go's join post. Otherwise each is created by its own
/// side's server — the path `ChannelHasBeenCreated` is about, and one where this server writes
/// no join post ([D-231]), which every later payload would carry as a different `LastPostAt`.
async fn create_open_channel(
    pair: &mut MemberPair,
    admin: &str,
    team: &str,
    prefix: &str,
    nonce: &str,
    through_main_go: bool,
) -> (String, String) {
    let (go_name, rs_name) = (format!("{prefix}go{nonce}"), format!("{prefix}rs{nonce}"));
    let make = |name: &str| {
        (
            "/api/v4/channels".to_owned(),
            Some(
                serde_json::to_vec(&serde_json::json!({
                    "team_id": team,
                    "name": name,
                    "display_name": format!("Display {name}"),
                    "type": "O",
                }))
                .expect("a body"),
            ),
        )
    };
    let ((gs, gb), (rs, rb)) = if through_main_go {
        let mut out = Vec::new();
        for name in [&go_name, &rs_name] {
            let (path, bytes) = make(name);
            let (status, bytes, _) = request_raw(
                &pair.client,
                GO,
                reqwest::Method::POST,
                Some(admin),
                &path,
                bytes.as_deref(),
            )
            .await;
            out.push((
                status,
                serde_json::from_slice::<Json>(&bytes).unwrap_or(Json::Null),
            ));
        }
        let rust = out.pop().expect("two");
        (out.pop().expect("two"), rust)
    } else {
        pair.each(reqwest::Method::POST, admin, make(&go_name), make(&rs_name))
            .await
    };
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    let (gid, rid) = (
        gb["id"].as_str().expect("an id").to_owned(),
        rb["id"].as_str().expect("an id").to_owned(),
    );
    pair.go_scrub.push((gid.clone(), "<channel>".to_owned()));
    pair.go_scrub.push((go_name, "<channel-name>".to_owned()));
    pair.rust_scrub.push((rid.clone(), "<channel>".to_owned()));
    pair.rust_scrub.push((rs_name, "<channel-name>".to_owned()));
    (gid, rid)
}

async fn run_the_channel_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-channels");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;
    let team = common::create_team(&client, &admin, "hookch").await;
    let mut users = std::collections::BTreeMap::new();
    for tag in [
        "chdmgo", "chdmrs", "chgmgo1", "chgmgo2", "chgmrs1", "chgmrs2",
    ] {
        let user = common::create_plain_user(&client, &admin, &team, tag).await;
        CHANNEL_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(user.id.clone());
        users.insert(tag, user.id);
    }

    let go_transcript = go_log.to_string_lossy().into_owned();
    let go = start_go(
        &go_run,
        &[("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())],
        CHANNEL_GO_OFFSET,
    )
    .await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let rust = SecondServer::start_in(
        CHANNEL_HOST_PORT,
        &rs_run,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
            ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
            ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
        ],
    )
    .await
    .expect("the Rust host starts");
    wait_until_running(&client, &admin, &go.base).await;
    wait_until_running(&client, &admin, &rust.base).await;

    let mut pair = MemberPair {
        client: client.clone(),
        go_base: go.base.clone(),
        rust_base: rust.base.clone(),
        go_log,
        rust_log,
        go_scrub: Vec::new(),
        rust_scrub: Vec::new(),
        seen: 0,
    };
    // A run-unique suffix: channel names outlive the run.
    let nonce = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    );
    let body = |value: Json| Some(serde_json::to_vec(&value).expect("a body"));

    let patch = |header: &str, go_id: &str, rust_id: &str| {
        let b = body(serde_json::json!({ "header": header }));
        (
            (format!("/api/v4/channels/{go_id}/patch"), b.clone()),
            (format!("/api/v4/channels/{rust_id}/patch"), b),
        )
    };

    // 1. An open channel. The hook runs at the end of `CreateChannel`, with the saved channel.
    let (go_ch, rs_ch) =
        create_open_channel(&mut pair, &admin, &team, "hookplain", &nonce, false).await;
    let fired = pair.hooks("an open channel").await;
    assert_eq!(names(&fired), ["ChannelHasBeenCreated"]);
    assert_eq!(fired[0]["args"]["B"]["Id"], "<id>");
    assert_eq!(fired[0]["args"]["B"]["Name"], "<channel-name>");
    assert_eq!(fired[0]["args"]["B"]["Type"], "O");
    let (created_go, created_rs) = (go_ch, rs_ch);

    // The rest of the lifecycle acts on channels main Go made, for the reason
    // `create_open_channel` gives.
    let (go_ch, rs_ch) =
        create_open_channel(&mut pair, &admin, &team, "hookupdated", &nonce, true).await;

    // 2. An ordinary update: the new channel and the old, in that order.
    let (g, r) = patch("a header the recorder lets through", &go_ch, &rs_ch);
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "an updated channel");
    let fired = pair.hooks("an update").await;
    assert_eq!(names(&fired), ["ChannelWillBeUpdated"]);
    assert_eq!(
        fired[0]["args"]["B"]["Header"],
        "a header the recorder lets through"
    );
    assert_eq!(
        fired[0]["args"]["C"]["Header"],
        Json::Null,
        "the old one had none"
    );

    // 3. A refused update: the reason is a `Reason` parameter, and nothing is written.
    let (g, r) = patch("!reject-update not this header", &go_ch, &rs_ch);
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a refused update",
    );
    assert_eq!(gb["id"], "app.channel.update_channel.rejected_by_plugin");
    pair.hooks("a refused update").await;

    // 4. A replacement carrying the whole channel with a new header.
    let (g, r) = patch("!rewrite-header", &go_ch, &rs_ch);
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a rewritten channel");
    assert_eq!(gb["header"], "rewritten by the hook recorder");
    pair.hooks("a rewritten update").await;

    // 5. A replacement carrying **only** a header: Go takes it whole, blanking every other field,
    //    and the store refuses what is left. Both must refuse it the same way.
    let (g, r) = patch("!partial-header", &go_ch, &rs_ch);
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!(gs, rs, "Go {gb} / Rust {rb}");
    assert_ne!(gs, 200, "a channel with no id cannot be written: {gb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a partial replacement",
    );
    pair.hooks("a partial replacement").await;

    // 6-7. Archive, then restore.
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::DELETE,
            &admin,
            (format!("/api/v4/channels/{go_ch}"), None),
            (format!("/api/v4/channels/{rs_ch}"), None),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks("an archive").await;
    assert_eq!(names(&fired), ["ChannelWillBeArchived"]);
    assert_eq!(
        fired[0]["args"]["B"]["DeleteAt"],
        Json::Null,
        "asked before the write"
    );

    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            (format!("/api/v4/channels/{go_ch}/restore"), None),
            (format!("/api/v4/channels/{rs_ch}/restore"), None),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    let fired = pair.hooks("a restore").await;
    assert_eq!(names(&fired), ["ChannelWillBeRestored"]);
    assert_eq!(
        fired[0]["args"]["B"]["DeleteAt"], "<set>",
        "asked before the write"
    );

    // 8. A refused archive.
    let (go_alive, rs_alive) =
        create_open_channel(&mut pair, &admin, &team, "hookkeepalive", &nonce, true).await;
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::DELETE,
            &admin,
            (format!("/api/v4/channels/{go_alive}"), None),
            (format!("/api/v4/channels/{rs_alive}"), None),
        )
        .await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "app.channel.delete_channel.rejected_by_plugin");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a refused archive",
    );
    pair.hooks("a refused archive").await;

    // 9. A refused restore.
    let (go_arch, rs_arch) =
        create_open_channel(&mut pair, &admin, &team, "hookkeeparchived", &nonce, true).await;
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::DELETE,
            &admin,
            (format!("/api/v4/channels/{go_arch}"), None),
            (format!("/api/v4/channels/{rs_arch}"), None),
        )
        .await;
    assert_eq!((gs, rs), (200, 200));
    pair.hooks("its archive").await;
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            (format!("/api/v4/channels/{go_arch}/restore"), None),
            (format!("/api/v4/channels/{rs_arch}/restore"), None),
        )
        .await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "app.channel.restore_channel.rejected_by_plugin");
    pair.hooks("a refused restore").await;

    // 10a. Drafts, on each side's channel. The hook sits just before the store's upsert.
    let draft = |message: &str| {
        let make = |channel: &str| {
            Some(
                serde_json::to_vec(&serde_json::json!({
                    "channel_id": channel,
                    "message": message,
                }))
                .expect("a body"),
            )
        };
        (
            ("/api/v4/drafts".to_owned(), make(&go_ch)),
            ("/api/v4/drafts".to_owned(), make(&rs_ch)),
        )
    };
    let (g, r) = draft("a draft the recorder lets through");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!(gs, rs, "Go {gb} / Rust {rb}");
    assert!((200..300).contains(&gs), "Go {gb}");
    same_post(&gb, &rb, "a draft");
    let fired = pair.hooks("a draft").await;
    assert_eq!(names(&fired), ["DraftWillBeUpserted"]);
    assert_eq!(fired[0]["args"]["B"]["ChannelId"], "<channel>");

    let (g, r) = draft("!reject-draft not this one");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "app.draft.upsert.rejected_by_plugin");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a refused draft",
    );
    pair.hooks("a refused draft").await;

    let (g, r) = draft("!rewrite-draft");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!(gs, rs, "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a rewritten draft");
    assert_eq!(gb["message"], "rewritten by the hook recorder");
    pair.hooks("a rewritten draft").await;

    // A replacement carrying only a message is taken whole: no user, no channel.
    let (g, r) = draft("!partial-draft");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!(gs, rs, "Go {gb} / Rust {rb}");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "a partial draft",
    );
    pair.hooks("a partial draft").await;

    // An over-long message: the hook is asked, then the store's `IsValid` refuses, and Go's
    // `UpsertDraft` wraps that as a 500 like any other store failure.
    let (g, r) = draft(&"x".repeat(70_000));
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (500, 500), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "app.draft.save.app_error");
    common::assert_error_bodies_match_except_known_gaps(
        &serde_json::to_vec(&gb).expect("bytes"),
        &serde_json::to_vec(&rb).expect("bytes"),
        "an over-long draft",
    );
    pair.hooks("an over-long draft").await;

    // An empty message deletes the draft before the hook is reached.
    let (g, r) = draft("");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!(gs, rs, "Go {gb} / Rust {rb}");
    pair.no_more_hooks("an emptied draft").await;

    // 10. A direct message channel, with a counterpart of each side's own. Its name is the two
    //     ids sorted, so it is scrubbed whole before its parts.
    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(&admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let admin_id = me["id"].as_str().expect("an id").to_owned();
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            (
                "/api/v4/channels/direct".to_owned(),
                body(serde_json::json!([admin_id, users["chdmgo"]])),
            ),
            (
                "/api/v4/channels/direct".to_owned(),
                body(serde_json::json!([admin_id, users["chdmrs"]])),
            ),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    for (scrub, body, user) in [
        (&mut pair.go_scrub, &gb, &users["chdmgo"]),
        (&mut pair.rust_scrub, &rb, &users["chdmrs"]),
    ] {
        scrub.insert(
            0,
            (
                body["name"].as_str().expect("a name").to_owned(),
                "<dm-name>".to_owned(),
            ),
        );
        scrub.push((
            body["id"].as_str().expect("an id").to_owned(),
            "<dm>".to_owned(),
        ));
        scrub.push((user.clone(), "<dm-user>".to_owned()));
    }
    let fired = pair.hooks("a direct channel").await;
    assert_eq!(names(&fired), ["ChannelHasBeenCreated"]);
    assert_eq!(fired[0]["args"]["B"]["Type"], "D");
    assert_eq!(fired[0]["args"]["B"]["Name"], "<dm-name>");

    // 11. The same DM again: it exists, so nothing fires.
    let ((gs, _), (rs, _)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            (
                "/api/v4/channels/direct".to_owned(),
                body(serde_json::json!([admin_id, users["chdmgo"]])),
            ),
            (
                "/api/v4/channels/direct".to_owned(),
                body(serde_json::json!([admin_id, users["chdmrs"]])),
            ),
        )
        .await;
    assert_eq!((gs, rs), (201, 201));
    pair.no_more_hooks("an existing direct channel").await;

    // 12. A group channel. Its name is a hash of the member ids and its display name lists the
    //     usernames, so both are scrubbed.
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::POST,
            &admin,
            (
                "/api/v4/channels/group".to_owned(),
                body(serde_json::json!([users["chgmgo1"], users["chgmgo2"]])),
            ),
            (
                "/api/v4/channels/group".to_owned(),
                body(serde_json::json!([users["chgmrs1"], users["chgmrs2"]])),
            ),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    for (scrub, body, side) in [
        (&mut pair.go_scrub, &gb, "go"),
        (&mut pair.rust_scrub, &rb, "rs"),
    ] {
        scrub.insert(
            0,
            (
                body["name"].as_str().expect("a name").to_owned(),
                "<gm-name>".to_owned(),
            ),
        );
        scrub.push((
            body["id"].as_str().expect("an id").to_owned(),
            "<gm>".to_owned(),
        ));
        for n in ["1", "2"] {
            let tag = format!("chgm{side}{n}");
            scrub.push((users[tag.as_str()].clone(), format!("<gm-user-{n}>")));
            scrub.push((common::plain_username(&tag), format!("<gm-username-{n}>")));
        }
    }
    let fired = pair.hooks("a group channel").await;
    assert_eq!(names(&fired), ["ChannelHasBeenCreated"]);
    assert_eq!(fired[0]["args"]["B"]["Type"], "G");

    drop(rust);
    drop(go);
    for id in [created_go, created_rs, go_ch, rs_ch, go_alive, rs_alive] {
        common::delete_channel(&client, &admin, &id).await;
    }
}

// ---------------------------------------------------------------------------------------------
// The consumed tranche: MessagesWillBeConsumed and MessagesWillBeConsumedWithContext
// ---------------------------------------------------------------------------------------------

/// Cross-server parity for `MessagesWillBeConsumed` and `MessagesWillBeConsumedWithContext`
/// (docs/PLUGIN_PLAN.md, Phase 5; [D-932]) — the hooks Go fires on the way **out** of every post
/// read, `GetSinglePost` included, and on the `rpost` a create or an edit answers with.
///
/// The recorder implements them only when `HOOK_RECORDER_CONSUME` is set, so the older tours'
/// transcripts are untouched. The posts are made by the stack's own Go server, which hosts no
/// plugins, so their creation records nothing and both hosts then read the same rows. What the
/// plugin answers is keyed off each post's message: `!consume ` and `!consume-ctx ` ask for a
/// replacement carrying **only** an id and a message, which Go takes whole — the channel, the
/// author and the timestamps come back blank to the client — and `!consume-stranger` answers a
/// post under an id the host never asked about.
#[tokio::test]
async fn the_consumed_hooks_fire_as_go_fires_them() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_consumed_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// A post written by the stack's Go server, which records nothing.
async fn post_via_main_go(
    client: &reqwest::Client,
    admin: &str,
    channel: &str,
    root_id: &str,
    message: &str,
) -> String {
    let (status, body, _) = request_raw(
        client,
        GO,
        reqwest::Method::POST,
        Some(admin),
        "/api/v4/posts",
        Some(
            &serde_json::to_vec(&serde_json::json!({
                "channel_id": channel,
                "root_id": root_id,
                "message": message,
                "props": { "mmrs_consumed": "yes", "mmrs_n": 2 },
            }))
            .expect("a body"),
        ),
    )
    .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice::<Json>(&body).expect("a post")["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

impl Pair {
    /// Wait until both transcripts have stopped growing and agree in length, then compare what
    /// this step added. With `ordered` the comparison is entry for entry; without it each side's
    /// entries are sorted first, for a step that also fires a `*HasBeen*` hook — dispatched on a
    /// detached task on both sides, so its place among the consumed hooks is a race, not a fact.
    async fn quiet_hooks(&mut self, what: &str, ordered: bool) -> Vec<Json> {
        let read = || (transcript(&self.go_log), transcript(&self.rust_log));
        let (mut go, mut rust) = read();
        let mut quiet_since = std::time::Instant::now();
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (g, r) = read();
            if g.len() == go.len() && r.len() == rust.len() {
                if quiet_since.elapsed() >= QUIET && g.len() == r.len() && g.len() > self.seen {
                    break;
                }
            } else {
                quiet_since = std::time::Instant::now();
            }
            go = g;
            rust = r;
        }
        let mut fresh_go = go[self.seen.min(go.len())..].to_vec();
        let mut fresh_rust = rust[self.seen.min(rust.len())..].to_vec();
        if !ordered {
            let key = |e: &Json| (e["hook"].as_str().unwrap_or("").to_owned(), e.to_string());
            fresh_go.sort_by_key(key);
            fresh_rust.sort_by_key(key);
        }
        assert_eq!(
            names(&fresh_go),
            names(&fresh_rust),
            "{what}: the hooks that fired\n  go:   {:?}\n  rust: {:?}",
            names(&fresh_go),
            names(&fresh_rust)
        );
        for (index, (g, r)) in fresh_go.iter().zip(fresh_rust.iter()).enumerate() {
            assert_eq!(g, r, "{what}: hook {index} differs");
        }
        self.seen = go.len();
        fresh_go
    }
}

/// The messages of a consumed hook's slice, in the (sorted) order the transcript holds them.
fn slice_messages(entry: &Json, key: &str) -> Vec<String> {
    entry["args"][key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|p| p["Message"].as_str().unwrap_or("").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

async fn run_the_consumed_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-consumed");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;

    let team = common::create_team(&client, &admin, "hookcs").await;
    let channel = common::create_channel(&client, &admin, &team, "hookcs").await;

    let go = start_go(
        &go_run,
        &[
            ("HOOK_RECORDER_TRANSCRIPT", &go_log.to_string_lossy()),
            ("HOOK_RECORDER_CONSUME", "1"),
        ],
        GO_OFFSET + 1,
    )
    .await;

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust = SecondServer::start_in(
        HOST_PORT + 1,
        &rs_run,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
            ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
            ("HOOK_RECORDER_TRANSCRIPT", &rust_log.to_string_lossy()),
            ("HOOK_RECORDER_CONSUME", "1"),
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

    // The rows both hosts read, written by a server that hosts no plugins.
    let plain = post_via_main_go(&client, &admin, &channel, "", "consumed plain").await;
    let reply = post_via_main_go(&client, &admin, &channel, &plain, "consumed reply").await;
    let consume = post_via_main_go(&client, &admin, &channel, "", "!consume two").await;
    let consume_ctx = post_via_main_go(&client, &admin, &channel, "", "!consume-ctx three").await;
    let stranger = post_via_main_go(&client, &admin, &channel, "", "!consume-stranger").await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        (
            transcript(&pair.go_log).len(),
            transcript(&pair.rust_log).len()
        ),
        (0, 0),
        "the stack's Go server hosts no plugins"
    );

    // 1. A single read: the plain hook, then the context-aware one, over a one-post slice.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/posts/{plain}"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a plain read");
    let fired = pair.quiet_hooks("a plain read", true).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext"
        ],
        "the plain hook first, then the context-aware one"
    );
    assert_eq!(slice_messages(&fired[0], "A"), ["consumed plain"]);
    assert_eq!(slice_messages(&fired[1], "B"), ["consumed plain"]);
    assert_eq!(
        fired[0]["args"]["A"][0]["Metadata"],
        Json::Null,
        "ForPlugin nils the metadata"
    );
    assert_eq!(
        fired[0]["args"]["A"][0]["Props"]["$map"]["mmrs_n"]["$iface"], "float64",
        "props cross as Go's float64"
    );
    assert_eq!(fired[1]["args"]["A"]["IPAddress"], "127.0.0.1");
    assert_eq!(
        fired[1]["args"]["A"]["SessionId"].as_str().map(str::len),
        Some(26)
    );

    // 2. A replacement carrying only an id and a message is taken **whole** — and on a single
    //    read that is fatal: `GetPostIfAuthorized` goes on to look up the post's channel, which
    //    is now the empty string, so the route is a 404 `app.channel.get.existing.app_error` on
    //    both hosts, after the hooks have run. The context-aware hook is handed the post as the
    //    plain hook's answer left it.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/posts/{consume}"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (404, 404), "Go {gb} / Rust {rb}");
    assert_eq!(error_of(&gb), error_of(&rb), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "app.channel.get.existing.app_error");
    let fired = pair.quiet_hooks("a consumed read", true).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext"
        ]
    );
    assert_eq!(slice_messages(&fired[0], "A"), ["!consume two"]);
    assert_eq!(
        slice_messages(&fired[1], "B"),
        ["consumed: two"],
        "the slice is rebuilt from the plain hook's answer"
    );
    assert_eq!(
        fired[1]["args"]["B"][0]["ChannelId"],
        Json::Null,
        "and the rebuilt post is the replacement, whole"
    );

    // 3. The context-aware hook's own replacement: the same 404, and the plain hook saw the
    //    original message because it runs first.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/posts/{consume_ctx}"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (404, 404), "Go {gb} / Rust {rb}");
    assert_eq!(error_of(&gb), error_of(&rb), "Go {gb} / Rust {rb}");
    let fired = pair.quiet_hooks("a read consumed with context", true).await;
    assert_eq!(slice_messages(&fired[0], "A"), ["!consume-ctx three"]);
    assert_eq!(slice_messages(&fired[1], "B"), ["!consume-ctx three"]);

    // 4. A replacement under an id the host never asked about is ignored.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/posts/{stranger}"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    assert_eq!(
        gb["message"], "!consume-stranger",
        "the stranger was ignored"
    );
    same_post(&gb, &rb, "a stranger's replacement");
    pair.quiet_hooks("a stranger's replacement", true).await;

    // 5. A channel page: one slice with every post, rewritten in place. Both hosts read the
    //    same rows, so the ids agree and the comparison sorts by them.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/channels/{channel}/posts"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a channel page");
    // The list route looks nothing up after the hook, so here the client sees what "taken
    // whole" means: the new message, and no channel, author or timestamps.
    assert_eq!(gb["posts"][&consume]["message"], "consumed: two");
    assert_eq!(gb["posts"][&consume]["channel_id"], "");
    assert_eq!(gb["posts"][&consume]["create_at"], 0);
    assert_eq!(
        gb["posts"][&consume_ctx]["message"],
        "consumed with context: three"
    );
    assert_eq!(gb["posts"][&plain]["message"], "consumed plain");
    let fired = pair.quiet_hooks("a channel page", true).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext"
        ]
    );
    assert_eq!(fired[0]["args"]["A"].as_array().map(Vec::len), Some(6));

    // 6. The since branch, which reads through `GetPostsSince`.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/channels/{channel}/posts?since=1"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a since page");
    pair.quiet_hooks("a since page", true).await;

    // 7. A thread: `GetPostThread` over the root and its reply, then `GetPostIfAuthorized` over
    //    the root alone — two slices, in that order.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/posts/{plain}/thread"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a thread");
    let fired = pair.quiet_hooks("a thread", true).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
        ]
    );
    let mut thread = slice_messages(&fired[0], "A");
    thread.sort();
    assert_eq!(thread, ["consumed plain", "consumed reply"]);
    assert_eq!(slice_messages(&fired[2], "A"), ["consumed plain"]);
    let _ = reply;

    // 8. Around the last unread: the thread of the first unread post, then the window before
    //    it, then the window after — an empty window fires nothing, on both sides alike.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!(
                "/api/v4/users/me/channels/{channel}/posts/unread?limit_before=2&limit_after=2"
            ),
            None,
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "around the last unread");
    pair.quiet_hooks("around the last unread", true).await;

    // 9. The post info route reads through `GetSinglePost` too.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::GET,
            &format!("/api/v4/posts/{plain}/info"),
            None,
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    assert_eq!(gb, rb, "the post info");
    let fired = pair.quiet_hooks("the post info", true).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext"
        ]
    );

    // 10. Flagging a post: `updatePreferences` reads the post through `GetSinglePost` before it
    //     saves, and `PreferencesHaveChanged` follows on a detached task.
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
    let flag = serde_json::to_vec(&serde_json::json!([{
        "user_id": me,
        "category": "flagged_post",
        "name": plain,
        "value": "true",
    }]))
    .expect("a body");
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::PUT,
            "/api/v4/users/me/preferences",
            Some(&flag),
        )
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    let fired = pair.quiet_hooks("flagging a post", false).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
            "PreferencesHaveChanged",
        ]
    );

    // 11. The flagged list.
    let ((gs, gb), (rs, rb)) = pair
        .both(reqwest::Method::GET, "/api/v4/users/me/posts/flagged", None)
        .await;
    assert_eq!((gs, rs), (200, 200), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "the flagged list");
    let fired = pair.quiet_hooks("the flagged list", true).await;
    // `contains`, not equality: the flagged list is the shared admin's, and a concurrent suite
    // may have flagged posts of its own — the transcripts still agree, which is the assertion.
    assert!(
        slice_messages(&fired[0], "A").contains(&"consumed plain".to_owned()),
        "the flagged list carries this tour's post: {:?}",
        slice_messages(&fired[0], "A")
    );

    // 13. A create answered by each host: the two post hooks, then the consumed pair over the
    //     prepared post — `CreatePost` fires it on the `rpost` it answers with.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(
                &serde_json::to_vec(&serde_json::json!({
                    "channel_id": channel,
                    "message": "consumed on create",
                }))
                .expect("a body"),
            ),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a created post");
    let (go_created, rs_created) = (
        gb["id"].as_str().expect("an id").to_owned(),
        rb["id"].as_str().expect("an id").to_owned(),
    );
    let fired = pair.quiet_hooks("a created post", false).await;
    assert_eq!(
        names(&fired),
        [
            "MessageHasBeenPosted",
            "MessageWillBePosted",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
        ],
        "sorted by name"
    );
    assert_eq!(
        fired[2]["args"]["A"][0]["Id"], "<id>",
        "the consumed slice carries the saved post"
    );

    // 13b. A create whose answer the context-aware hook replaces: the client gets the
    //      replacement whole — no channel, no author — with the **prepared** post's metadata put
    //      back on it, which is the one field the hook cannot take away.
    let ((gs, gb), (rs, rb)) = pair
        .both(
            reqwest::Method::POST,
            "/api/v4/posts",
            Some(
                &serde_json::to_vec(&serde_json::json!({
                    "channel_id": channel,
                    "message": "!consume-ctx on create",
                }))
                .expect("a body"),
            ),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    assert_eq!(gb["message"], "consumed with context: on create");
    assert_eq!(gb["channel_id"], "", "taken whole");
    assert!(
        gb["metadata"].is_object(),
        "the prepared metadata is put back: {gb}"
    );
    same_post(&gb, &rb, "a create consumed with context");
    let fired = pair
        .quiet_hooks("a create consumed with context", false)
        .await;
    assert_eq!(
        names(&fired),
        [
            "MessageHasBeenPosted",
            "MessageWillBePosted",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
        ],
        "sorted by name"
    );

    // 12. A reaction, one post per host: `SaveReactionForPost` reads the post through
    //     `GetSinglePost` first. One post each, because the second host to react to a shared
    //     post reads the first host's reaction back — `HasReactions` — and the transcripts
    //     would differ for a reason that is not the hook's.
    for (base, id) in [(&go.base, &go_created), (&rust.base, &rs_created)] {
        let reaction = serde_json::to_vec(&serde_json::json!({
            "user_id": me,
            "post_id": id,
            "emoji_name": "+1",
        }))
        .expect("a body");
        let (status, body, _) = request_raw(
            &client,
            base,
            reqwest::Method::POST,
            Some(&admin),
            "/api/v4/reactions",
            Some(&reaction),
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
    }
    let fired = pair.quiet_hooks("a reaction", false).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
            "ReactionHasBeenAdded",
        ]
    );

    // 14. A patch, one post per host: the handler's two reads and `PatchPost`'s one — three
    //     consumed pairs — then the edit hooks and the consumed pair `UpdatePost` fires over
    //     the post it answers with.
    let patch =
        serde_json::to_vec(&serde_json::json!({ "message": "consumed on patch" })).expect("a body");
    let (gs, gb, _) = request_raw(
        &client,
        &go.base,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/posts/{go_created}/patch"),
        Some(&patch),
    )
    .await;
    let (rs, rb, served_by) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::PUT,
        Some(&admin),
        &format!("/api/v4/posts/{rs_created}/patch"),
        Some(&patch),
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
    let fired = pair.quiet_hooks("a patch", false).await;
    assert_eq!(
        names(&fired),
        [
            "MessageHasBeenUpdated",
            "MessageWillBeUpdated",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
            "MessagesWillBeConsumedWithContext",
            "MessagesWillBeConsumedWithContext",
            "MessagesWillBeConsumedWithContext",
        ],
        "sorted by name: three reads and the answered post"
    );
    let mut messages: Vec<String> = fired
        .iter()
        .filter(|e| e["hook"] == "MessagesWillBeConsumed")
        .map(|e| slice_messages(e, "A").join(""))
        .collect();
    messages.sort();
    assert_eq!(
        messages,
        [
            "consumed on create",
            "consumed on create",
            "consumed on create",
            "consumed on patch",
        ],
        "three reads of the old text, one of the new"
    );

    // 15. The edit history reads the current post through `GetSinglePost`.
    let (gs, gb, _) = request_raw(
        &client,
        &go.base,
        reqwest::Method::GET,
        Some(&admin),
        &format!("/api/v4/posts/{go_created}/edit_history"),
        None,
    )
    .await;
    let (rs, rb, served_by) = request_raw(
        &client,
        &rust.base,
        reqwest::Method::GET,
        Some(&admin),
        &format!("/api/v4/posts/{rs_created}/edit_history"),
        None,
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
    let fired = pair.quiet_hooks("the edit history", true).await;
    assert_eq!(
        names(&fired),
        [
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext"
        ]
    );

    // 16. A pin: the handler's read, `PatchPost`'s read, the edit hooks, the answered post.
    for (base, id) in [(&go.base, &go_created), (&rust.base, &rs_created)] {
        let (status, body, _) = request_raw(
            &client,
            base,
            reqwest::Method::POST,
            Some(&admin),
            &format!("/api/v4/posts/{id}/pin"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
    }
    let fired = pair.quiet_hooks("a pin", false).await;
    assert_eq!(
        names(&fired),
        [
            "MessageHasBeenUpdated",
            "MessageWillBeUpdated",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
            "MessagesWillBeConsumedWithContext",
            "MessagesWillBeConsumedWithContext",
        ],
        "sorted by name: two reads and the answered post"
    );

    // 17. A delete: the handler reads through `GetSinglePost`; `DeletePost` itself reads the
    //     store directly and fires nothing.
    for (base, id) in [(&go.base, &go_created), (&rust.base, &rs_created)] {
        let (status, body, _) = request_raw(
            &client,
            base,
            reqwest::Method::DELETE,
            Some(&admin),
            &format!("/api/v4/posts/{id}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
    }
    let fired = pair.quiet_hooks("a delete", false).await;
    assert_eq!(
        names(&fired),
        [
            "MessageHasBeenDeleted",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
        ],
        "sorted by name: one read, then the deletion"
    );

    drop(rust);
    drop(go);
    common::delete_channel(&client, &admin, &channel).await;
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
fn a_slice_of_posts_is_sorted_by_id_before_the_ids_become_tokens() {
    let mut value: Json = serde_json::json!({
        "hook": "MessagesWillBeConsumedWithContext",
        "args": {
            "A": { "RequestId": "r" },
            "B": [
                { "Id": "zzzzzzzzzzzzzzzzzzzzzzzzzz", "Message": "last" },
                { "Id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "Message": "first" }
            ]
        }
    });
    sort_slices_by_id(&mut value);
    normalise(&mut value);
    assert_eq!(value["args"]["B"][0]["Message"], "first");
    assert_eq!(value["args"]["B"][0]["Id"], "<id>");
    assert_eq!(
        value["args"]["A"]["RequestId"], "<id>",
        "an object is left alone"
    );

    // A slice whose elements carry no id — the preferences — keeps its order.
    let mut value: Json = serde_json::json!({
        "hook": "PreferencesHaveChanged",
        "args": { "B": [ { "Name": "b" }, { "Name": "a" } ] }
    });
    sort_slices_by_id(&mut value);
    assert_eq!(value["args"]["B"][0]["Name"], "b");
}

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

/// Both escaped forms of the signature, and its padding, go; the text around them stays.
#[test]
fn unsigned_page_drops_every_signature() {
    let page = r#"x = '/error?m\u003Dno\u0026s\u003DMEUC_x-y\u003D\u003D'; url=/error?m=no&amp;s=MEUC_x-y%3D%3D">"#;
    assert_eq!(
        unsigned_page(page),
        r#"x = '/error?m\u003Dno\u0026s\u003D<sig>'; url=/error?m=no&amp;s=<sig>">"#
    );
}

/// A line the plugin has not finished writing is not read yet — neither parsed nor counted.
#[test]
fn a_torn_last_line_waits_for_its_newline() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-unit-torn");
    std::fs::create_dir_all(&dir).expect("the directory");
    let path = dir.join("hooks.jsonl");
    std::fs::write(&path, "{\"hook\":\"One\"}\n{\"hook\":\"Tw").expect("the file");
    assert_eq!(names(&transcript(&path)), ["One"]);
    std::fs::write(&path, "{\"hook\":\"One\"}\n{\"hook\":\"Two\"}\n").expect("the file");
    assert_eq!(names(&transcript(&path)), ["One", "Two"]);
}

/// A transcript that is not there yet is empty, not a panic: both servers are polled before
/// either has answered a request.
#[test]
fn a_missing_transcript_is_empty() {
    assert!(transcript(Path::new("/nonexistent/hooks.jsonl")).is_empty());
}
