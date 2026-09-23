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
/// The Rust host of the onboarding tranche, `OnInstall`.
const ONBOARDING_HOST_PORT: u16 = 8135;
/// Its Go server.
const ONBOARDING_GO_OFFSET: u16 = 85;
/// The Rust host of the scheduled-post tranche, `ScheduledPostWillBeCreated`.
const SCHEDULED_HOST_PORT: u16 = 8136;
/// Its Go server — the **licensed** build, since the scheduled-post routes refuse without one.
const SCHEDULED_GO_OFFSET: u16 = 86;
/// The Rust host of the support-packet tranche, `GenerateSupportData`.
const SUPPORT_HOST_PORT: u16 = 8137;
/// Its Go server — licensed too: the packet is refused without a licence.
const SUPPORT_GO_OFFSET: u16 = 87;
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
            pack_bundle(
                "plugin-hooks-bundle",
                &format!(
                    r#"{{"id": "{PLUGIN_ID}", "name": "Hook Recorder", "version": "0.1.0", "server": {{"executable": "plugin"}}}}"#
                ),
            )
        })
        .clone()
}

/// The recorder with `manifest` as its `plugin.json`, packed under `scratch` in the test's
/// temporary directory. Each variant has its own staging directory, so no variant touches the
/// plain bundle the older tours install.
fn pack_bundle(scratch: &str, manifest: &str) -> PathBuf {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(scratch);
    let stage = scratch.join(PLUGIN_ID);
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&stage).expect("the staging directory");
    std::fs::write(stage.join("plugin.json"), manifest).expect("the manifest");
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
}

/// [`bundle`] with the manifest carrying a `support_packet` prop — the System Console's
/// checkbox, which makes `GenerateSupportData` ask for the plugin to be ticked.
fn bundle_with_support_prop() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            pack_bundle(
                "plugin-hooks-bundle-support",
                &format!(
                    r#"{{"id": "{PLUGIN_ID}", "name": "Hook Recorder", "version": "0.1.0", "server": {{"executable": "plugin"}}, "props": {{"support_packet": "The hook recorder's transcript"}}}}"#
                ),
            )
        })
        .clone()
}

/// [`bundle`] with a settings schema: defaults at the top level and in a section, and a secret
/// in each, for the configuration tranche.
fn bundle_with_settings_schema() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let manifest = serde_json::json!({
                "id": PLUGIN_ID,
                "name": "Hook Recorder",
                "version": "0.1.0",
                "server": { "executable": "plugin" },
                "settings_schema": {
                    "settings": [
                        { "key": "Plain", "type": "text", "default": "plain-default" },
                        { "key": "SecretKey", "type": "text", "secret": true, "default": "secret-default" },
                        { "key": "OnlyDefault", "type": "number", "default": 7 },
                        { "key": "NoDefault", "type": "text" }
                    ],
                    "sections": [{ "key": "Advanced", "settings": [
                        { "key": "SectionSecret", "type": "text", "secret": true },
                        { "key": "SectionDefault", "type": "bool", "default": true }
                    ]}]
                }
            });
            pack_bundle("plugin-hooks-bundle-settings", &manifest.to_string())
        })
        .clone()
}

/// [`lay_out`] with `tarball` in the file store instead of the plain bundle.
fn lay_out_bundle(run: &Path, tarball: &Path) -> PathBuf {
    let transcript = lay_out(run);
    std::fs::copy(
        tarball,
        run.join("data/plugins").join(format!("{PLUGIN_ID}.tar.gz")),
    )
    .expect("the bundle reaches the file store");
    transcript
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
    /// The tour's scratch directory: the parent of both hosts' run directories.
    scratch: PathBuf,
}

/// Killed on drop, and so is every plugin process either host started under this tour.
///
/// A SIGKILLed server never shuts its plugins down, so each tour used to leave its recorder
/// processes behind, reparented to init — 1,050 of them across four worktrees, measured
/// 2026-09-23. The Rust host is always dropped first (it is declared after this server, and every
/// tour drops it first by hand), so by the time this runs both hosts are gone and whatever still
/// executes from under the scratch directory is an orphan. `pkill` is run directly, not through
/// a shell, so its pattern cannot match the process doing the killing.
impl Drop for GoServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("pkill")
            .arg("-f")
            .arg(format!("{}/", self.scratch.display()))
            .status();
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
    start_go_binary("mattermost", run, env, offset).await
}

/// [`start_go`] with the binary under `reference/.build/` named — `mattermost-licensed`, the
/// enterprise-ready build `scripts/go-licensed.sh` makes, for a tranche whose routes need a
/// licence. The licence itself still has to arrive in `env`.
async fn start_go_binary(name: &str, run: &Path, env: &[(&str, &str)], offset: u16) -> GoServer {
    let binary = repo().join("reference/.build").join(name);
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
        scratch: run
            .parent()
            .map_or_else(|| run.to_path_buf(), Path::to_path_buf),
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
///
/// **Except an `*AppError`'s `Id`**, which is the error's translation key, not a minted id — the
/// one thing about an error most worth comparing. An object carrying `StatusCode` is an app error
/// (gob omits a zero field, and no app error has status 0). Until 2026-09-23 this masked it, so
/// every tranche compared error messages but not error ids.
fn normalise(value: &mut Json) {
    match value {
        Json::Array(items) => items.iter_mut().for_each(normalise),
        Json::Object(map) => {
            let is_app_error = map.contains_key("StatusCode");
            for (key, entry) in map.iter_mut() {
                if is_app_error && key == "Id" {
                    continue;
                }
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

/// The post-family hooks the system message a membership change writes fires — Go's
/// `postAddToChannelMessage` and its eighteen siblings go through the whole of `a.CreatePost`.
///
/// Compared like every other hook, but left out of what [`MemberPair::hooks`] hands back, so each
/// step's own assertions stay about the hook the step is for.
const SYSTEM_POST_HOOKS: [&str; 2] = ["MessageWillBePosted", "MessageHasBeenPosted"];

fn is_system_post_hook(entry: &Json) -> bool {
    entry["hook"]
        .as_str()
        .is_some_and(|h| SYSTEM_POST_HOOKS.contains(&h))
        && entry["args"]["B"]["Type"]
            .as_str()
            .is_some_and(|t| t.starts_with("system_"))
}

/// A step's entries sorted by hook name and then by rendering, so two hosts that interleave detached hooks
/// differently still compare equal when they fired the same hooks with the same arguments.
fn in_canonical_order(entries: &[Json]) -> Vec<Json> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_cached_key(|e| {
        (
            e["hook"].as_str().unwrap_or_default().to_owned(),
            e.to_string(),
        )
    });
    sorted
}

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
        let read = || {
            (
                transcript_of(&self.go_log, &self.go_scrub),
                transcript_of(&self.rust_log, &self.rust_scrub),
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
        // A step's hooks are compared as a set: the notification hooks run on detached tasks on
        // both hosts, and a system post's two run on yet another, so their interleaving is not
        // a promise either server makes.
        let (go_step, rust_step) = (
            in_canonical_order(&go[self.seen.min(go.len())..]),
            in_canonical_order(&rust[self.seen.min(rust.len())..]),
        );
        assert_eq!(
            names(&go_step),
            names(&rust_step),
            "{what}: the hooks that fired\n  go:   {:?}\n  rust: {:?}",
            names(&go),
            names(&rust)
        );
        for (index, (g, r)) in go_step.iter().zip(rust_step.iter()).enumerate() {
            assert_eq!(g, r, "{what}: hook {index} differs");
        }
        let fresh = go[self.seen..]
            .iter()
            .filter(|e| !is_system_post_hook(e))
            .cloned()
            .collect();
        self.seen = go.len();
        fresh
    }

    /// Wait for quiescence and assert **nothing** fired — for the two rejections, where the plugin
    /// is called and the request then dies before any notification hook.
    async fn no_more_hooks(&mut self, what: &str) {
        tokio::time::sleep(QUIET).await;
        let go = transcript_of(&self.go_log, &self.go_scrub);
        let rust = transcript_of(&self.rust_log, &self.rust_scrub);
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
    let batch_team = common::create_team(&client, &admin, "hookbt").await;

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
    // It needs `team_admin` on the four teams to add and remove members there.
    for team in [&join_team, &reject_team, &admin_team, &batch_team] {
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

    // 5b. A batch add. `AddTeamMembers` passes the session as the requestor, so the default
    //     channels get "added to the team by" and "added to the channel by" notices rather than
    //     the "joined" ones step 5 writes — the only step where the requestor reaches them.
    let batch = |user: &str| {
        (
            format!("/api/v4/teams/{batch_team}/members/batch"),
            Some(
                serde_json::to_vec(
                    &serde_json::json!([{ "team_id": batch_team, "user_id": user }]),
                )
                .expect("a body"),
            ),
        )
    };
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::POST,
            &actor_token,
            batch(&go_user.id),
            batch(&rs_user.id),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    let fired = pair.hooks("a batch team add").await;
    assert_eq!(
        &names(&fired)[..2],
        ["TeamMemberWillBeAdded", "UserHasJoinedTeam"],
        "the whole sequence was {:?}",
        names(&fired)
    );
    let notice_types: Vec<String> = transcript_of(&pair.go_log, &pair.go_scrub)
        .iter()
        .filter(|e| e["hook"] == "MessageWillBePosted")
        .filter_map(|e| e["args"]["B"]["Type"].as_str().map(str::to_owned))
        .collect();
    assert!(
        notice_types.iter().any(|t| t == "system_add_to_team"),
        "Go wrote an added-to-the-team notice: {notice_types:?}"
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
/// side's server — the path `ChannelHasBeenCreated` is about, and the creator's join post, whose
/// two message hooks fire on both.
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
// The onboarding tranche: OnInstall
// ---------------------------------------------------------------------------------------------

/// The armored private half of `pluginSigTestKey`, read out of the Go source that holds it
/// (`reference/dump/behaviour_plugin_signature.go`). Its public half is the fixture's
/// `keys.armored`, which `parity::marketplace::plant_key` puts where
/// `SignaturePublicKeyFiles` points. A TEST KEY: nothing trusts it outside this repository.
fn test_signing_key() -> pgp::composed::SignedSecretKey {
    use pgp::composed::Deserializable as _;

    let source =
        std::fs::read_to_string(repo().join("reference/dump/behaviour_plugin_signature.go"))
            .expect("the signature oracle's source");
    let start = source
        .find("const pluginSigTestKey = `")
        .expect("the test key's declaration")
        + "const pluginSigTestKey = `".len();
    let end = start + source[start..].find('`').expect("the key's closing quote");
    let (key, _) = pgp::composed::SignedSecretKey::from_string(&source[start..end])
        .expect("the test key parses");
    key
}

/// A run directory for the onboarding tranche: the file store **empty**, so start-up installs
/// nothing, and the signed bundle in `prepackaged_plugins`, which is where a Marketplace install
/// finds it with the remote Marketplace off (plugin_install.go:268). Neither server installs a
/// prepackaged plugin at start-up unless `PluginStates` enables it, and the tranche leaves it
/// unset.
fn lay_out_prepackaged(run: &Path, signature: &[u8]) -> PathBuf {
    let transcript = lay_out(run);
    std::fs::remove_file(run.join("data/plugins").join(format!("{PLUGIN_ID}.tar.gz")))
        .expect("the file store's bundle goes");
    let prepackaged = run.join("prepackaged_plugins");
    std::fs::create_dir_all(&prepackaged).expect("the prepackaged directory");
    let file = prepackaged.join(format!("{PLUGIN_ID}.tar.gz"));
    std::fs::copy(bundle(), &file).expect("the prepackaged bundle");
    std::fs::write(
        prepackaged.join(format!("{PLUGIN_ID}.tar.gz.sig")),
        signature,
    )
    .expect("its signature");
    transcript
}

/// Wait until `path` holds `expected` entries, then return them.
async fn transcript_reaches(path: &Path, expected: usize, side: &str) -> Vec<Json> {
    for _ in 0..300 {
        let entries = transcript(path);
        if entries.len() >= expected {
            return entries;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{side}: waited for {expected} hooks, saw {:?}",
        names(&transcript(path))
    );
}

/// Cross-server parity for `OnInstall` (docs/PLUGIN_PLAN.md, Phase 5; [D-932]), whose one call
/// site is `POST /api/v4/system/onboarding/complete` naming plugins: each is installed from the
/// Marketplace, enabled, and then told `OnInstall` — on its own goroutine, after the response.
///
/// The recorder's bundle is signed here with the signature oracle's test key and offered as a
/// prepackaged plugin, so both hosts install the same bytes through the same verification. The
/// two onboardings run one after the other, with `PluginStates` cleared between them, so each
/// host starts from a plugin that is not enabled.
#[tokio::test]
async fn the_install_hook_fires_as_go_fires_it() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_onboarding_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn run_the_onboarding_tour(client: &reqwest::Client, admin: &str) {
    use pgp::ser::Serialize as _;

    super::marketplace::plant_key(&super::marketplace::fixture()).await;
    plant_state(client, admin, None).await;
    let signature = pgp::composed::DetachedSignature::sign_binary_data(
        rand08::thread_rng(),
        &test_signing_key().primary_key,
        &pgp::types::Password::empty(),
        pgp::crypto::hash::HashAlgorithm::Sha256,
        std::fs::File::open(bundle()).expect("the bundle"),
    )
    .expect("the bundle is signed")
    // The framed packet, header and all: the inner `Signature` serialises its body alone,
    // which Go's reader refuses at the first byte.
    .to_bytes()
    .expect("the signature serialises");

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-onboarding");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out_prepackaged(&go_run, &signature);
    let rust_log = lay_out_prepackaged(&rs_run, &signature);
    let settings = [
        (
            "MM_PLUGINSETTINGS_SIGNATUREPUBLICKEYFILES",
            "mmrs-marketplace-test.plugin.asc",
        ),
        ("MM_PLUGINSETTINGS_ENABLEREMOTEMARKETPLACE", "false"),
    ];

    let go_transcript = go_log.to_string_lossy().into_owned();
    let mut go_env = vec![("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())];
    go_env.extend(settings);
    let go = start_go(&go_run, &go_env, ONBOARDING_GO_OFFSET).await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let mut rust_env = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    rust_env.extend(settings);
    let rust = SecondServer::start_in(ONBOARDING_HOST_PORT, &rs_run, &rust_env)
        .await
        .expect("the Rust host starts");

    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let admin_id = me["id"].as_str().expect("an id").to_owned();
    let body = serde_json::to_vec(&serde_json::json!({
        "organization": "Hook Recorder Org",
        "install_plugins": [PLUGIN_ID],
    }))
    .expect("a body");

    let mut answers = Vec::new();
    let mut fired = Vec::new();
    for (base, log, side) in [
        (go.base.as_str(), go_log.as_path(), "Go"),
        (rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        assert!(
            transcript(log).is_empty(),
            "{side}: the recorder ran before onboarding installed it"
        );
        let (status, answer, served_by) = request_raw(
            client,
            base,
            reqwest::Method::POST,
            Some(admin),
            "/api/v4/system/onboarding/complete",
            Some(&body),
        )
        .await;
        if side == "Rust" {
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "the onboarding was forwarded"
            );
        }
        answers.push((status, String::from_utf8_lossy(&answer).into_owned()));
        wait_until_running(client, admin, base).await;
        let entries = transcript_reaches(log, 1, side).await;
        // Nothing else follows: the recorder is told once, and by nobody but onboarding.
        tokio::time::sleep(QUIET).await;
        let entries_after = transcript(log);
        assert_eq!(entries, entries_after, "{side}: a hook after OnInstall");
        fired.push(entries);
        // Each host starts from a plugin that is not enabled.
        plant_state(client, admin, None).await;
    }

    assert_eq!(answers[0], answers[1], "the onboarding answer");
    assert_eq!(answers[0].0, 200, "{}", answers[0].1);
    assert_eq!(names(&fired[0]), ["OnInstall"]);
    assert_eq!(fired[0], fired[1], "what OnInstall was handed");
    let args = &fired[1][0]["args"];
    assert_eq!(
        args["B"]["UserId"],
        admin_id.as_str(),
        "the event names the onboarding user"
    );
    assert!(
        args["A"]["SessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the plugin.Context carries the session: {args}"
    );
    assert_eq!(args["A"]["RequestId"], "<id>", "a request id is minted");

    // A failed install ends that plugin's goroutine: no enable, no `OnInstall`. The bundle is
    // taken away, so the prepackaged entry both hosts loaded at start-up no longer opens — and
    // the recorder, installed by the first round, would be enabled and told again by a host
    // that carried on.
    for (run, base, log, side) in [
        (&go_run, go.base.as_str(), go_log.as_path(), "Go"),
        (&rs_run, rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        std::fs::remove_file(
            run.join("prepackaged_plugins")
                .join(format!("{PLUGIN_ID}.tar.gz")),
        )
        .expect("the prepackaged bundle goes");
        let (status, _, _) = request_raw(
            client,
            base,
            reqwest::Method::POST,
            Some(admin),
            "/api/v4/system/onboarding/complete",
            Some(&body),
        )
        .await;
        assert_eq!(status, 200, "{side}: a failed install is only logged");
        tokio::time::sleep(QUIET * 3).await;
        assert_eq!(
            names(&transcript(log)),
            ["OnInstall"],
            "{side}: a failed install went on to OnInstall"
        );
    }

    drop(rust);
    drop(go);
}

// ---------------------------------------------------------------------------------------------
// The scheduled-post tranche: ScheduledPostWillBeCreated
// ---------------------------------------------------------------------------------------------

/// The channel the scheduled-post tour writes into, for the cleanup.
static SCHEDULED_CHANNEL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Cross-server parity for `ScheduledPostWillBeCreated` (docs/PLUGIN_PLAN.md, Phase 5; [D-932]),
/// which `SaveScheduledPost` and `UpdateScheduledPost` both run after their validation and
/// before the store.
///
/// The scheduled-post routes refuse on an unlicensed server, so **both** hosts carry the licence
/// the licensed oracle uses — `MM_LICENSE` and the key that verifies it, in the environment, never
/// in the shared `Licenses` table — and the Go side is the enterprise-ready build. The recorder
/// answers off the post's message, as it does for drafts.
#[tokio::test]
async fn the_scheduled_post_hook_fires_as_go_fires_it() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_scheduled_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    let channel = SCHEDULED_CHANNEL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let (Some(channel), Some(pool)) = (channel, common::fixture_pool().await) {
        // The partial replacement is saved with no user and no channel, so it is found by the
        // message the recorder gave it rather than by the channel.
        let _ = sqlx::query(
            "DELETE FROM scheduledposts WHERE channelid = $1
                OR (userid = '' AND message = 'rewritten by the hook recorder')",
        )
        .bind(&channel)
        .execute(&pool)
        .await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn run_the_scheduled_tour(client: &reqwest::Client, admin: &str) {
    let client = client.clone();
    let admin = admin.to_owned();

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-scheduled");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(&client, &admin, Some(true)).await;
    let team = common::create_team(&client, &admin, "hooksp").await;
    let channel = common::create_channel(&client, &admin, &team, "hooksp").await;
    *SCHEDULED_CHANNEL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(channel.clone());

    let (signed, key_file) = common::stack_license_files();
    let licence = [
        ("MM_LICENSE", signed.as_str()),
        ("MMRS_LICENSE_PUBLIC_KEY_FILE", key_file.as_str()),
    ];
    let go_transcript = go_log.to_string_lossy().into_owned();
    let mut go_env = vec![("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())];
    go_env.extend(licence);
    let go = start_go_binary("mattermost-licensed", &go_run, &go_env, SCHEDULED_GO_OFFSET).await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let mut rust_env = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    rust_env.extend(licence);
    let rust = SecondServer::start_in(SCHEDULED_HOST_PORT, &rs_run, &rust_env)
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
    let scheduled_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
        + 3_600_000;
    let create = |message: &str| {
        let b = Some(
            serde_json::to_vec(&serde_json::json!({
                "channel_id": channel,
                "message": message,
                "scheduled_at": scheduled_at,
            }))
            .expect("a body"),
        );
        (
            ("/api/v4/posts/schedule".to_owned(), b.clone()),
            ("/api/v4/posts/schedule".to_owned(), b),
        )
    };
    let same_error = |gb: &Json, rb: &Json, what: &str| {
        common::assert_error_bodies_match_except_known_gaps(
            &serde_json::to_vec(gb).expect("bytes"),
            &serde_json::to_vec(rb).expect("bytes"),
            what,
        );
    };

    // 1. A create the recorder lets through: the hook sees the post after `PreSave`, with its id.
    let (g, r) = create("a scheduled post the recorder lets through");
    let ((gs, go_post), (rs, rs_post)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {go_post} / Rust {rs_post}");
    same_post(&go_post, &rs_post, "a scheduled post");
    let fired = pair.hooks("a scheduled post").await;
    assert_eq!(names(&fired), ["ScheduledPostWillBeCreated"]);
    let seen = &fired[0]["args"]["B"];
    assert_eq!(
        seen["Id"], "<id>",
        "PreSave minted the id before the hook: {seen}"
    );
    assert_eq!(seen["Draft"]["ChannelId"], channel.as_str());
    assert_eq!(seen["ScheduledAt"], scheduled_at);

    // 2. Refused: the reason is a parameter of the save's own id.
    let (g, r) = create("!reject-scheduled not this one");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "app.scheduled_post.save.rejected_by_plugin");
    same_error(&gb, &rb, "a refused scheduled post");
    pair.hooks("a refused scheduled post").await;

    // 3. Rewritten whole.
    let (g, r) = create("!rewrite-scheduled");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a rewritten scheduled post");
    assert_eq!(gb["message"], "rewritten by the hook recorder");
    pair.hooks("a rewritten scheduled post").await;

    // 4. A replacement carrying only a message is taken whole and saved unvalidated: no user,
    //    no channel, no send time — and an id and timestamps from the store's second `PreSave`.
    let (g, r) = create("!partial-scheduled");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a partial scheduled post");
    assert_eq!(gb["user_id"], "", "taken whole: {gb}");
    assert_eq!(gb["scheduled_at"], 0, "taken whole: {gb}");
    pair.hooks("a partial scheduled post").await;

    // 5. No message: `IsValid` refuses before the hook is reached.
    let (g, r) = create("");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::POST, &admin, g, r).await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    same_error(&gb, &rb, "an empty scheduled post");
    pair.no_more_hooks("an empty scheduled post").await;

    // Updates, each side on its own post from step 1. The hook sees the post after
    // `RestoreNonUpdatableFields`, with `error_code` and `processed_at` reset.
    let update = |post: &Json, message: &str| {
        let mut post = post.clone();
        post["message"] = serde_json::json!(message);
        let id = post["id"].as_str().expect("an id").to_owned();
        (
            format!("/api/v4/posts/schedule/{id}"),
            Some(serde_json::to_vec(&post).expect("a body")),
        )
    };
    let edit = |message: &str| (update(&go_post, message), update(&rs_post, message));

    // 6. An edit the recorder lets through.
    let (g, r) = edit("an edit the recorder lets through");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "an edited scheduled post");
    let fired = pair.hooks("an edited scheduled post").await;
    assert_eq!(names(&fired), ["ScheduledPostWillBeCreated"]);
    assert_eq!(
        fired[0]["args"]["B"]["Draft"]["Message"],
        "an edit the recorder lets through"
    );

    // 7. An edit refused, with the update's own id.
    let (g, r) = edit("!reject-scheduled no edits");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!((gs, rs), (400, 400), "Go {gb} / Rust {rb}");
    assert_eq!(gb["id"], "app.scheduled_post.update.rejected_by_plugin");
    same_error(&gb, &rb, "a refused edit");
    pair.hooks("a refused edit").await;

    // 8. An edit rewritten.
    let (g, r) = edit("!rewrite-scheduled");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a rewritten edit");
    assert_eq!(gb["message"], "rewritten by the hook recorder");
    pair.hooks("a rewritten edit").await;

    // 9. An edit answered with a partial post: taken whole, id and all, so the store's update
    //    names no row and succeeds.
    let (g, r) = edit("!partial-scheduled");
    let ((gs, gb), (rs, rb)) = pair.each(reqwest::Method::PUT, &admin, g, r).await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a partial edit");
    assert_eq!(gb["id"], "", "taken whole: {gb}");
    pair.hooks("a partial edit").await;

    // 10. A delete asks no plugin.
    let del = |post: &Json| {
        (
            format!(
                "/api/v4/posts/schedule/{}",
                post["id"].as_str().expect("an id")
            ),
            None,
        )
    };
    let ((gs, gb), (rs, rb)) = pair
        .each(
            reqwest::Method::DELETE,
            &admin,
            del(&go_post),
            del(&rs_post),
        )
        .await;
    assert_eq!((gs, rs), (201, 201), "Go {gb} / Rust {rb}");
    same_post(&gb, &rb, "a deleted scheduled post");
    // The partial edit named no row: the post is still the admin's, as the rewrite left it. Had
    // it landed on the real id, the owner check would have refused this delete with a 403.
    assert_eq!(gb["message"], "rewritten by the hook recorder");
    assert_eq!(
        gb["user_id"], go_post["user_id"],
        "the partial edit wrote nothing"
    );
    assert_eq!(gb["scheduled_at"], scheduled_at);
    pair.no_more_hooks("a delete").await;

    drop(rust);
    drop(go);
}

// ---------------------------------------------------------------------------------------------
// The support-packet tranche: GenerateSupportData
// ---------------------------------------------------------------------------------------------

/// Cross-server parity for `GenerateSupportData` (docs/PLUGIN_PLAN.md, Phase 5; [D-932]), the
/// plugin loop at the end of `GenerateSupportPacket` — against the licensed Go build, with the
/// recorder's manifest declaring the `support_packet` prop, so its checkbox decides.
///
/// Three packets, each with the logs off so the only warnings are the plugin's: one the recorder
/// is not ticked for (the hook must not fire), one it is (it fires, and its two files join the
/// zip), and one whose `User-Agent` makes it answer an error alongside a file (the warning is
/// written and the file dropped).
#[tokio::test]
async fn the_support_data_hook_fires_as_go_fires_it() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_support_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn run_the_support_tour(client: &reqwest::Client, admin: &str) {
    use crate::parity::support_packet::{read_zip, warning_points};

    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-support");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out_bundle(&go_run, &bundle_with_support_prop());
    let rust_log = lay_out_bundle(&rs_run, &bundle_with_support_prop());

    plant_state(client, admin, Some(true)).await;
    let (signed, key_file) = common::stack_license_files();
    let licence = [
        ("MM_LICENSE", signed.as_str()),
        ("MMRS_LICENSE_PUBLIC_KEY_FILE", key_file.as_str()),
    ];
    let go_transcript = go_log.to_string_lossy().into_owned();
    let mut go_env = vec![("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())];
    go_env.extend(licence);
    let go = start_go_binary("mattermost-licensed", &go_run, &go_env, SUPPORT_GO_OFFSET).await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let mut rust_env = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    rust_env.extend(licence);
    let rust = SecondServer::start_in(SUPPORT_HOST_PORT, &rs_run, &rust_env)
        .await
        .expect("the Rust host starts");
    wait_until_running(client, admin, &go.base).await;
    wait_until_running(client, admin, &rust.base).await;

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
    let packet = |base: String, query: &'static str, agent: &'static str| {
        let client = client.clone();
        let admin = admin.to_owned();
        async move {
            let response = client
                .get(format!("{base}/api/v4/system/support_packet{query}"))
                .bearer_auth(&admin)
                .header("User-Agent", agent)
                .timeout(Duration::from_secs(120))
                .send()
                .await
                .expect("the server answers");
            let status = response.status().as_u16();
            let served_by = response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = response.bytes().await.expect("a body").to_vec();
            assert_eq!(
                status,
                200,
                "{base}{query}: {}",
                String::from_utf8_lossy(&body)
            );
            (read_zip(&body), served_by)
        }
    };
    let plugin_files = |entries: &[(String, Vec<u8>)]| -> Vec<(String, Vec<u8>)> {
        entries
            .iter()
            .filter(|(name, _)| name.starts_with("mmrs.hookrecorder/"))
            .cloned()
            .collect()
    };
    let file = |entries: &[(String, Vec<u8>)], name: &str| {
        entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
    };

    // 1. Not ticked: the prop is declared and the id is not in `plugin_packets`.
    let untick = "?basic_server_logs=false&plugin_packets=someone.else";
    let (go_zip, _) = packet(go.base.clone(), untick, "mmrs-parity").await;
    let (rs_zip, served_by) = packet(rust.base.clone(), untick, "mmrs-parity").await;
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "the Rust host builds the packet"
    );
    pair.no_more_hooks("a packet the recorder is not ticked for")
        .await;
    assert!(plugin_files(&go_zip).is_empty() && plugin_files(&rs_zip).is_empty());
    // The environment half: the recorder is listed, enabled, on both.
    let (gp, rp) = (file(&go_zip, "plugins.json"), file(&rs_zip, "plugins.json"));
    assert_eq!(rp, gp, "plugins.json");
    assert!(gp.is_some_and(|p| p.contains(PLUGIN_ID)));
    assert_eq!(file(&go_zip, "warning.txt"), None);
    assert_eq!(file(&rs_zip, "warning.txt"), None);

    // 2. Ticked: the hook fires, and its files join the packet in the order it sent them.
    let tick = "?basic_server_logs=false&plugin_packets=mmrs.hookrecorder";
    let (go_zip, _) = packet(go.base.clone(), tick, "mmrs-parity").await;
    let (rs_zip, _) = packet(rust.base.clone(), tick, "mmrs-parity").await;
    let fired = pair.hooks("a ticked packet").await;
    assert_eq!(names(&fired), ["GenerateSupportData"]);
    assert_eq!(fired[0]["args"]["A"]["UserAgent"], "mmrs-parity");
    let (go_files, rs_files) = (plugin_files(&go_zip), plugin_files(&rs_zip));
    assert_eq!(rs_files, go_files, "the plugin's files");
    assert_eq!(
        rs_files.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        [
            "mmrs.hookrecorder/recorded.txt",
            "mmrs.hookrecorder/raw.bin"
        ]
    );
    let names_of = |entries: &[(String, Vec<u8>)]| -> Vec<String> {
        entries.iter().map(|(n, _)| n.clone()).collect()
    };
    let rs_names = names_of(&rs_zip);
    assert_eq!(
        rs_names.last().map(String::as_str),
        Some("mmrs.hookrecorder/raw.bin"),
        "after every file the server wrote: {rs_names:?}"
    );
    assert_eq!(file(&rs_zip, "warning.txt"), None);

    // 3. The plugin answers an error with a file: a warning, and no file.
    let (go_zip, _) = packet(go.base.clone(), tick, "mmrs-parity hookfail").await;
    let (rs_zip, _) = packet(rust.base.clone(), tick, "mmrs-parity hookfail").await;
    let fired = pair.hooks("a failing plugin").await;
    assert_eq!(names(&fired), ["GenerateSupportData"]);
    assert!(plugin_files(&go_zip).is_empty(), "Go drops the files");
    assert!(plugin_files(&rs_zip).is_empty(), "so does the Rust host");
    let (gw, rw) = (
        file(&go_zip, "warning.txt").expect("Go writes the warning"),
        file(&rs_zip, "warning.txt").expect("so does the Rust host"),
    );
    assert_eq!(warning_points(&rw), warning_points(&gw));
    assert!(
        gw.contains("the hook recorder has nothing to report"),
        "{gw}"
    );

    // A protected property field whose source plugin is **installed** is its plugin's alone:
    // `checkFieldDeleteAccess` asks the plugin host (`pluginChecker`, server.go:328), and both
    // servers here host the recorder. A field naming a plugin neither hosts is anyone's with the
    // field permission. Planted, because the REST API cannot create a protected field ([D-542]).
    {
        use crate::parity::cpa_licensed::{plant_field, planted_id};
        let _rows = common::PROPERTY_ROWS.lock().await;
        let attrs = |source: &str| {
            format!(r#"{{"protected":true,"source_plugin_id":"{source}","visibility":"always"}}"#)
        };
        let mut planted = Vec::new();
        for (side, base) in [("go", &go.base), ("rs", &rust.base)] {
            for (tag, source, expected) in
                [("inst", PLUGIN_ID, 403), ("gone", "mmrs.absentsource", 200)]
            {
                let id = planted_id(&format!("hk{side}{tag}"));
                plant_field(&id, &format!("hook {side} {tag}"), "text", &attrs(source)).await;
                planted.push(id.clone());
                let (status, body, served_by) = request_raw(
                    client,
                    base,
                    reqwest::Method::DELETE,
                    Some(admin),
                    &format!("/api/v4/custom_profile_attributes/fields/{id}"),
                    None,
                )
                .await;
                if side == "rs" {
                    assert_eq!(
                        served_by.as_deref(),
                        Some("rust"),
                        "the delete was forwarded"
                    );
                }
                assert_eq!(
                    status,
                    expected,
                    "{side}: deleting a field whose source plugin is {tag}: {}",
                    String::from_utf8_lossy(&body)
                );
                if expected == 403 {
                    let error: Json = serde_json::from_slice(&body).expect("an error body");
                    assert_eq!(error["id"], "app.property.access_denied.app_error");
                }
            }
        }
        let pool = common::fixture_pool().await.expect("the stack database");
        for id in planted {
            sqlx::query("DELETE FROM propertyfields WHERE id = $1")
                .bind(&id)
                .execute(&pool)
                .await
                .expect("the planted field goes");
        }
    }

    drop(rust);
    drop(go);
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

    // 18. A system post: a header patch writes the "updated the channel header" notice through
    //     `CreatePost`, so it is offered to the plugins on its way to the `posted` event like any
    //     other post ([D-950]). Each host patches a fresh channel of its own, made by the stack's
    //     Go server, so the two channel rows and the two notices differ only in id and name.
    let mut notices = Vec::new();
    for (base, log, tag) in [
        (&go.base, &pair.go_log, "hookcsgo"),
        (&rust.base, &pair.rust_log, "hookcsrs"),
    ] {
        let id = common::create_channel(&client, &admin, &team, tag).await;
        let (status, body, _) = request_raw(
            &client,
            base,
            reqwest::Method::PUT,
            Some(&admin),
            &format!("/api/v4/channels/{id}/patch"),
            Some(br#"{"header": "a consumed header"}"#),
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        notices.push((
            log.clone(),
            vec![
                (id, "<ch>".to_owned()),
                (tag.to_owned(), "<tag>".to_owned()),
            ],
        ));
    }
    tokio::time::sleep(QUIET * 2).await;
    let fresh: Vec<Vec<Json>> = notices
        .iter()
        .map(|(log, scrub)| in_canonical_order(&transcript_of(log, scrub)[pair.seen..]))
        .collect();
    assert_eq!(
        names(&fresh[0]),
        names(&fresh[1]),
        "a system post: the hooks that fired"
    );
    for (index, (g, r)) in fresh[0].iter().zip(fresh[1].iter()).enumerate() {
        assert_eq!(g, r, "a system post: hook {index} differs");
    }
    assert_eq!(
        names(&fresh[0]),
        [
            "ChannelWillBeUpdated",
            "MessageHasBeenPosted",
            "MessageWillBePosted",
            "MessagesWillBeConsumed",
            "MessagesWillBeConsumedWithContext",
        ],
        "sorted by name: the update, then the notice's four"
    );
    let consumed = fresh[0]
        .iter()
        .find(|e| e["hook"] == "MessagesWillBeConsumed")
        .expect("the consumed hook");
    assert_eq!(
        consumed["args"]["A"][0]["Type"], "system_header_change",
        "the notice is what the plugins are offered: {consumed}"
    );
    pair.seen = transcript(&pair.go_log).len();
    for (_, scrub) in &notices {
        common::delete_channel(&client, &admin, &scrub[0].0).await;
    }

    drop(rust);
    drop(go);
    common::delete_channel(&client, &admin, &channel).await;
}

// ---------------------------------------------------------------------------------------------
// The plugin API tranche: the KV store, logging and server information (Phase 6)
// ---------------------------------------------------------------------------------------------

/// The Rust host of the plugin API tranche; see `second_server_ports`.
const KV_HOST_PORT: u16 = 8142;
/// Its Go server.
const KV_GO_OFFSET: u16 = 88;
/// Another plugin's id, whose planted row the script must neither see nor delete.
const KV_NEIGHBOUR: &str = "mmrs.kvneighbour";

/// Cross-server parity for the first plugin API methods (docs/PLUGIN_PLAN.md, Phase 6;
/// `mm_app::plugin_api`): the nine `KV*` methods, the four `Log*`, and `GetServerVersion`,
/// `GetDiagnosticId` and `GetSystemInstallDate`.
///
/// A post whose message is `!kv-script` makes the recorder run a fixed script of API calls from
/// inside `MessageWillBePosted` and write down every answer (`examples/recorder/kv.rs`). The
/// transcripts are compared entry for entry, and so are the `PluginKeyValueStore` rows each host
/// left behind.
///
/// # The two sides run in sequence, not side by side
///
/// Both hosts serve the one plugin id, so they share its rows. So each side starts from the same
/// planted fixture — purged and replanted between them — and its rows are read before the other
/// side runs. The fixture holds what the API cannot write: the pre-5.6 hashed spellings of two
/// keys, two expired values, a NULL `ExpireAt`, a value that expires far in the future, and a row
/// of **another** plugin's that delete-all must spare.
#[tokio::test]
async fn the_plugin_api_kv_methods_answer_as_go_answers() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_kv_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    purge_kv_rows().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn purge_kv_rows() {
    let pool = common::fixture_pool().await.expect("the stack database");
    sqlx::query("DELETE FROM PluginKeyValueStore WHERE PluginId = $1 OR PluginId = $2")
        .bind(PLUGIN_ID)
        .bind(KV_NEIGHBOUR)
        .execute(&pool)
        .await
        .expect("the KV rows go");
}

/// The fixture both sides start from, written around both servers.
async fn plant_kv_rows() {
    purge_kv_rows().await;
    let pool = common::fixture_pool().await.expect("the stack database");
    let far_future = 4_102_444_800_000_i64; // 2100-01-01
    let hash = mm_app::plugin_key_value_store::key_hash;
    let rows: [(&str, String, &[u8], Option<i64>); 8] = [
        (PLUGIN_ID, hash("legacy"), b"from before 5.6", Some(0)),
        (PLUGIN_ID, hash("hashed-only"), b"hashed", Some(0)),
        (PLUGIN_ID, "expired".into(), b"stale", Some(1)),
        (PLUGIN_ID, "expired-old".into(), b"stale", Some(1)),
        (PLUGIN_ID, "null-expiry".into(), b"null", None),
        (PLUGIN_ID, "future-old".into(), b"stale", Some(far_future)),
        (PLUGIN_ID, "zz-kept".into(), b"kept", Some(0)),
        (KV_NEIGHBOUR, "shared-key".into(), b"neighbour", Some(0)),
    ];
    for (plugin, key, value, expire_at) in rows {
        sqlx::query(
            "INSERT INTO PluginKeyValueStore (PluginId, PKey, PValue, ExpireAt) VALUES ($1, $2, $3, $4)",
        )
        .bind(plugin)
        .bind(key)
        .bind(value)
        .bind(expire_at)
        .execute(&pool)
        .await
        .expect("a KV row is planted");
    }
}

/// Both plugins' rows, by plugin and key, with `ExpireAt` as what can be compared across two runs
/// a few seconds apart: never, NULL, past, or the whole minutes left.
async fn kv_rows() -> Vec<(String, String, String, String)> {
    let pool = common::fixture_pool().await.expect("the stack database");
    type Row = (String, String, Option<Vec<u8>>, Option<i64>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT PluginId, PKey, PValue, ExpireAt FROM PluginKeyValueStore
         WHERE PluginId = $1 OR PluginId = $2 ORDER BY PluginId, PKey",
    )
    .bind(PLUGIN_ID)
    .bind(KV_NEIGHBOUR)
    .fetch_all(&pool)
    .await
    .expect("the KV rows");
    let now = mm_model::utils::get_millis();
    rows.into_iter()
        .map(|(plugin, key, value, expire_at)| {
            let expiry = match expire_at {
                None => "null".to_owned(),
                Some(0) => "never".to_owned(),
                Some(at) if at < now => "past".to_owned(),
                Some(at) => format!("in {} min", (at - now + 30_000) / 60_000),
            };
            let value = value.map_or_else(
                || "<nil>".to_owned(),
                |v| String::from_utf8_lossy(&v).into_owned(),
            );
            (plugin, key, value, expiry)
        })
        .collect()
}

async fn run_the_kv_tour(client: &reqwest::Client, admin: &str) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-kv");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(client, admin, Some(true)).await;
    let team = common::create_team(client, admin, "hookkv").await;
    let channel = common::create_channel(client, admin, &team, "hookkv").await;

    let go = start_go(
        &go_run,
        &[("HOOK_RECORDER_TRANSCRIPT", &go_log.to_string_lossy())],
        KV_GO_OFFSET,
    )
    .await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust = SecondServer::start_in(
        KV_HOST_PORT,
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
    wait_until_running(client, admin, &go.base).await;
    wait_until_running(client, admin, &rust.base).await;

    let body = serde_json::to_vec(&serde_json::json!({
        "channel_id": channel,
        "message": "!kv-script",
    }))
    .expect("the post");

    // Each side from the same fixture, its rows read before the other side runs.
    let mut sides = Vec::new();
    for (base, log, side) in [
        (go.base.as_str(), go_log.as_path(), "Go"),
        (rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        plant_kv_rows().await;
        let (status, _, served_by) = request_raw(
            client,
            base,
            reqwest::Method::POST,
            Some(admin),
            "/api/v4/posts",
            Some(&body),
        )
        .await;
        assert_eq!(status, 201, "{side}: the post");
        if side == "Rust" {
            assert_eq!(served_by.as_deref(), Some("rust"), "the post was forwarded");
        }
        let entries = transcript_reaches(log, 3, side).await;
        sides.push((entries, kv_rows().await));
    }
    let (go_side, rust_side) = (&sides[0], &sides[1]);

    assert_eq!(
        names(&go_side.0),
        ["MessageWillBePosted", "KVScript", "MessageHasBeenPosted"],
        "the script runs inside the hook"
    );
    let go_calls = go_side.0[1]["calls"].as_array().expect("the calls");
    assert!(
        go_calls.iter().all(|c| c.get("error").is_none()),
        "Go implements every method the script calls: {go_calls:?}"
    );
    let rust_calls = rust_side.0[1]["calls"].as_array().expect("the calls");
    assert_eq!(
        go_calls.len(),
        rust_calls.len(),
        "the script ran to the end"
    );
    for (index, (g, r)) in go_calls.iter().zip(rust_calls).enumerate() {
        assert_eq!(g, r, "call {index} ({})", g["call"]);
    }
    for (index, (g, r)) in go_side.0.iter().zip(&rust_side.0).enumerate() {
        assert_eq!(g, r, "transcript entry {index}");
    }

    // A few answers the parity alone would not pin, because both sides could agree on a wrong one:
    // the hashed spelling is found, an expired or NULL-expiry value is not, and the rows prove
    // delete-all spared the neighbour.
    let returned = |index: usize| go_calls[index]["returns"].clone();
    assert_ne!(
        returned(0),
        returned(1),
        "the hashed `legacy` is found, `missing` is not"
    );
    assert_eq!(
        returned(1),
        returned(2),
        "an expired value reads as nothing"
    );
    assert_eq!(returned(1), returned(3), "so does a NULL expiry");
    assert_eq!(
        go_side.1,
        vec![
            (
                PLUGIN_ID.into(),
                "final-a".into(),
                "A".into(),
                "never".into()
            ),
            (
                PLUGIN_ID.into(),
                "final-b".into(),
                "B".into(),
                "in 60 min".into()
            ),
            (
                PLUGIN_ID.into(),
                "final-c".into(),
                "C".into(),
                "past".into()
            ),
            (
                KV_NEIGHBOUR.into(),
                "shared-key".into(),
                "neighbour".into(),
                "never".into()
            ),
        ],
        "Go's rows"
    );
    assert_eq!(go_side.1, rust_side.1, "the rows each host left");

    // Go's log is the oracle for how a pair and a dangling argument are logged
    // (`mm_app::plugin_api::log_fields`); this server's log format is its own.
    let go_text = std::fs::read_to_string(go_run.join("go.log")).unwrap_or_default();
    let info = go_text
        .lines()
        .find(|l| l.contains("hook recorder info"))
        .expect("Go logged the plugin's info line");
    assert!(
        info.contains(r#""plugin_id":"mmrs.hookrecorder""#) && info.contains(r#""script":"kv""#),
        "{info}"
    );
    assert!(
        go_text
            .lines()
            .any(|l| l.contains("invalid key/value pair") && l.contains(r#""arg":"dangling""#)),
        "Go complains about the dangling argument"
    );

    drop(rust);
    drop(go);
    common::delete_channel(client, admin, &channel).await;
}

// ---------------------------------------------------------------------------------------------
// The plugin API tranche: the plugin's configuration and the licence (Phase 6, D-990)
// ---------------------------------------------------------------------------------------------

/// The Rust host of the configuration tranche; see `second_server_ports`.
const CONFIG_HOST_PORT: u16 = 8143;
/// Its Go server — the licensed build, so `GetLicense` has a licence to answer with.
const CONFIG_GO_OFFSET: u16 = 89;
/// A plugin no bundle installs, whose stored settings `GetConfig` must drop and
/// `GetUnsanitizedConfig` must keep.
const CONFIG_ABSENT: &str = "mmrs.configabsent";
/// A plugin no bundle installs either, with an **empty** entry, which sanitising keeps.
const CONFIG_EMPTY: &str = "mmrs.configempty";

/// Cross-server parity for the plugin API's configuration and licence methods
/// (`mm_app::plugin_api_config`): `GetConfig`, `GetUnsanitizedConfig`, `GetPluginConfig`,
/// `LoadPluginConfiguration`, `SavePluginConfig` three ways, `GetLicense`, `GetPluginID`,
/// `GetTelemetryId`, `GetCloudLimits`, `GetBundlePath` and `IsEnterpriseReady`.
///
/// A `!config-script` post makes the recorder run `examples/recorder/config.rs` inside
/// `MessageWillBePosted`; the answers that carry a map are decoded dynamically there, so an empty
/// map that was sent and a nil one that was not render differently. Both hosts are licensed with
/// the stack's signed licence and given the **same** configuration environment — relative
/// directories, one site URL, one data source — so the two configurations they answer with are
/// the one document, and every answer is compared whole.
///
/// The recorder starts with **no** entry in `PluginSettings.Plugins` (the one state a patch
/// cannot produce, so it is made in the row), and its own saves then give it a map, another map,
/// an empty map and a nil one. Two other entries are planted: settings for a plugin no bundle
/// installs, and an empty entry.
///
/// # Which server saves
///
/// `SavePluginConfig` saves the whole configuration. Under the Go host that is the Go host's
/// own copy, as old as its start; under this server it is a patch through the **main** Go server
/// (`mm_app::peer_config`). So the two sides run in sequence, and between them main Go saves its
/// own current copy — undoing anything the Go host's stale copy reverted — before the recorder's
/// entry is taken out of the row again. The `Plugins` map each side left is compared too.
#[tokio::test]
async fn the_plugin_api_config_methods_answer_as_go_answers() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_config_tour(&client, &admin))
        .catch_unwind()
        .await;
    remove_planted_plugin_settings(&client, &admin).await;
    plant_state(&client, &admin, None).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Put entries into `PluginSettings.Plugins` through main Go, which keeps every entry the patch
/// does not name.
async fn patch_plugin_settings(client: &reqwest::Client, admin: &str, plugins: Json) {
    let status = client
        .put(format!("{GO}/api/v4/config/patch"))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "PluginSettings": { "Plugins": plugins } }))
        .send()
        .await
        .expect("Go answers")
        .status();
    assert!(status.is_success(), "patching Plugins: {status}");
}

/// The active document's `PluginSettings.Plugins`.
async fn stored_plugin_settings() -> Json {
    let pool = common::fixture_pool().await.expect("the stack database");
    let (value,): (String,) =
        sqlx::query_as("SELECT Value FROM Configurations WHERE Active LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("the active configuration");
    let document: Json = serde_json::from_str(&value).expect("JSON");
    document["PluginSettings"]["Plugins"].clone()
}

/// A patch cannot delete an entry, so the three planted ones are taken out of the active row
/// directly, and main Go is told to reload it.
async fn remove_planted_plugin_settings(client: &reqwest::Client, admin: &str) {
    remove_plugin_settings(client, admin, &[PLUGIN_ID, CONFIG_ABSENT, CONFIG_EMPTY]).await;
}

/// Take `ids` out of the active row's `PluginSettings.Plugins`, first having main Go save its
/// own copy (so a secondary Go server's stale save is undone), then reload main Go from the row.
async fn remove_plugin_settings(client: &reqwest::Client, admin: &str, ids: &[&str]) {
    patch_plugin_settings(client, admin, serde_json::json!({})).await;
    let pool = common::fixture_pool().await.expect("the stack database");
    let (id, value): (String, String) =
        sqlx::query_as("SELECT Id, Value FROM Configurations WHERE Active LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("the active configuration");
    let mut document: Json = serde_json::from_str(&value).expect("JSON");
    if let Some(plugins) = document["PluginSettings"]["Plugins"].as_object_mut() {
        for id in ids {
            plugins.remove(*id);
        }
    }
    sqlx::query("UPDATE Configurations SET Value = $1 WHERE Id = $2")
        .bind(document.to_string())
        .bind(&id)
        .execute(&pool)
        .await
        .expect("the planted settings go");
    let status = client
        .post(format!("{GO}/api/v4/config/reload"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .status();
    assert!(
        status.is_success(),
        "reloading Go's configuration: {status}"
    );
}

async fn run_the_config_tour(client: &reqwest::Client, admin: &str) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-config");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let tarball = bundle_with_settings_schema();
    let go_log = lay_out_bundle(&go_run, &tarball);
    let rust_log = lay_out_bundle(&rs_run, &tarball);

    plant_state(client, admin, Some(true)).await;
    patch_plugin_settings(
        client,
        admin,
        serde_json::json!({
            CONFIG_ABSENT: { "Kept": "only unsanitised" },
            CONFIG_EMPTY: {},
        }),
    )
    .await;
    remove_plugin_settings(client, admin, &[PLUGIN_ID]).await;
    let team = common::create_team(client, admin, "hookcfg").await;
    let channel = common::create_channel(client, admin, &team, "hookcfg").await;

    // One configuration environment for both hosts, so both answer with one document: the Go
    // host's port in the site URL and listen address, the directories relative to each run
    // directory, and Go's data source.
    let go_port = go_port() + CONFIG_GO_OFFSET;
    let site_url = format!("http://localhost:{go_port}");
    let listen = format!(":{go_port}");
    let dsn = format!(
        "{}?sslmode=disable&connect_timeout=10",
        std::env::var("DATABASE_URL").expect("parity.sh sets DATABASE_URL")
    );
    let (signed, key_file) = common::stack_license_files();
    let shared: Vec<(&str, &str)> = vec![
        ("MM_SERVICESETTINGS_SITEURL", site_url.as_str()),
        ("MM_SERVICESETTINGS_LISTENADDRESS", listen.as_str()),
        ("MM_SERVICESETTINGS_ENABLELOCALMODE", "false"),
        ("MM_SQLSETTINGS_DRIVERNAME", "postgres"),
        ("MM_SQLSETTINGS_DATASOURCE", dsn.as_str()),
        ("MM_SQLSETTINGS_MAXIDLECONNS", "2"),
        ("MM_SQLSETTINGS_MAXOPENCONNS", "5"),
        ("MM_JOBSETTINGS_RUNJOBS", "false"),
        ("MM_JOBSETTINGS_RUNSCHEDULER", "false"),
        ("MM_FILESETTINGS_DIRECTORY", "./data/"),
        ("MM_PLUGINSETTINGS_DIRECTORY", "./plugins"),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", "./client"),
        ("MM_LICENSE", signed.as_str()),
        ("MMRS_LICENSE_PUBLIC_KEY_FILE", key_file.as_str()),
    ];
    let go_transcript = go_log.to_string_lossy().into_owned();
    let mut go_env = shared.clone();
    go_env.push(("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str()));
    let go = start_go_binary("mattermost-licensed", &go_run, &go_env, CONFIG_GO_OFFSET).await;
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let mut rust_env = shared.clone();
    rust_env.push(("MMRS_PLUGIN_HOST", "rust"));
    rust_env.push(("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()));
    let rust = SecondServer::start_in(CONFIG_HOST_PORT, &rs_run, &rust_env)
        .await
        .expect("the Rust host starts");
    wait_until_running(client, admin, &go.base).await;
    wait_until_running(client, admin, &rust.base).await;

    let body = serde_json::to_vec(&serde_json::json!({
        "channel_id": channel,
        "message": "!config-script",
    }))
    .expect("the post");

    let mut sides = Vec::new();
    for (base, log, side) in [
        (go.base.as_str(), go_log.as_path(), "Go"),
        (rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        // No entry for the recorder before each side, and main Go's own copy in the row.
        remove_plugin_settings(client, admin, &[PLUGIN_ID]).await;
        let (status, _, served_by) = request_raw(
            client,
            base,
            reqwest::Method::POST,
            Some(admin),
            "/api/v4/posts",
            Some(&body),
        )
        .await;
        assert_eq!(status, 201, "{side}: the post");
        if side == "Rust" {
            assert_eq!(served_by.as_deref(), Some("rust"), "the post was forwarded");
        }
        let entries = transcript_reaches(log, 3, side).await;
        sides.push((entries, stored_plugin_settings().await));
    }
    let (go_side, rust_side) = (&sides[0], &sides[1]);

    assert_eq!(
        names(&go_side.0),
        [
            "MessageWillBePosted",
            "ConfigScript",
            "MessageHasBeenPosted"
        ],
        "the script runs inside the hook"
    );
    let go_calls = go_side.0[1]["calls"].as_array().expect("the calls");
    assert!(
        go_calls.iter().all(|c| c.get("error").is_none()),
        "Go implements every method the script calls: {go_calls:?}"
    );
    let rust_calls = rust_side.0[1]["calls"].as_array().expect("the calls");
    assert_eq!(
        go_calls.len(),
        rust_calls.len(),
        "the script ran to the end"
    );

    // Two answers are facts about the process, not the port: the bundle path is under each
    // host's own run directory, and `IsEnterpriseReady` is the build's flag — the licensed Go
    // build says true, this binary says what `MM_BUILD_ENTERPRISE_READY` was at compile time.
    let bundle_path = |calls: &[Json], run: &Path| -> String {
        let call = calls
            .iter()
            .find(|c| c["call"] == "GetBundlePath")
            .expect("GetBundlePath");
        call["returns"]["A"]
            .as_str()
            .expect("a path")
            .replace(&run.to_string_lossy().into_owned(), "<run>")
    };
    assert_eq!(
        bundle_path(go_calls, &go_run),
        format!("<run>/plugins/{PLUGIN_ID}"),
        "Go's bundle path"
    );
    assert_eq!(
        bundle_path(go_calls, &go_run),
        bundle_path(rust_calls, &rs_run),
        "the bundle path"
    );
    let enterprise_ready = |calls: &[Json]| -> Json {
        calls
            .iter()
            .find(|c| c["call"] == "IsEnterpriseReady")
            .expect("IsEnterpriseReady")["returns"]
            .clone()
    };
    assert_eq!(
        enterprise_ready(go_calls),
        serde_json::json!({ "A": true }),
        "the licensed Go build"
    );
    let built_ready = matches!(
        mm_model::version::BUILD_ENTERPRISE_READY,
        "1" | "t" | "T" | "TRUE" | "true" | "True"
    );
    let expected: Json = if built_ready {
        serde_json::json!({ "A": true })
    } else {
        serde_json::json!({})
    };
    assert_eq!(enterprise_ready(rust_calls), expected, "this build");

    for (index, (g, r)) in go_calls.iter().zip(rust_calls).enumerate() {
        if matches!(
            g["call"].as_str(),
            Some("GetBundlePath" | "IsEnterpriseReady")
        ) {
            continue;
        }
        assert_eq!(g, r, "call {index} ({})", g["call"]);
    }
    assert_eq!(go_side.0[0], rust_side.0[0], "MessageWillBePosted");

    // What parity alone would not pin, because both sides could agree on a wrong answer.
    let returned = |name: &str, nth: usize| -> Json {
        go_calls
            .iter()
            .filter(|c| c["call"] == name)
            .nth(nth)
            .unwrap_or_else(|| panic!("{name} #{nth}"))["returns"]
            .clone()
    };
    let plugins_of = |config: &Json| config["A"]["PluginSettings"]["Plugins"]["$map"].clone();
    let sanitized = plugins_of(&returned("GetConfig", 0));
    let unsanitized = plugins_of(&returned("GetUnsanitizedConfig", 0));
    assert!(
        sanitized.get(CONFIG_ABSENT).is_none(),
        "GetConfig drops an uninstalled plugin's settings"
    );
    assert!(
        unsanitized.get(CONFIG_ABSENT).is_some(),
        "GetUnsanitizedConfig keeps them"
    );
    assert_eq!(
        sanitized[CONFIG_EMPTY],
        serde_json::json!({ "$map": {} }),
        "an empty entry is kept, and sent"
    );
    let fake = serde_json::json!({ "$iface": "string", "value": mm_model::utils::FAKE_SETTING });
    assert_eq!(
        sanitized[PLUGIN_ID]["$map"]["secretkey"], fake,
        "a secret, matched without case"
    );
    assert_eq!(
        sanitized[PLUGIN_ID]["$map"]["SectionSecret"], fake,
        "a section's secret"
    );
    assert_eq!(
        unsanitized[PLUGIN_ID]["$map"]["secretkey"],
        serde_json::json!({ "$iface": "string", "value": "hunter2" }),
        "the unsanitised secret"
    );
    let empty_map = serde_json::json!({ "A": { "$map": {} } });
    assert_eq!(
        returned("GetPluginConfig", 0),
        empty_map,
        "no entry: an empty map, sent"
    );
    assert_eq!(
        returned("GetPluginConfig", 1)["A"]["$map"]["secretkey"],
        fake,
        "GetPluginConfig is sanitised"
    );
    assert!(
        returned("GetPluginConfig", 2)["A"]["$map"]
            .get("Plain")
            .is_none(),
        "a save replaces the entry whole"
    );
    assert_eq!(
        returned("GetPluginConfig", 3),
        empty_map,
        "an empty map is sent"
    );
    assert_eq!(
        returned("GetPluginConfig", 4),
        serde_json::json!({}),
        "a nil one is not"
    );
    assert_eq!(
        plugins_of(&returned("GetConfig", 1))[PLUGIN_ID],
        serde_json::json!({ "$map": {} }),
        "a nil entry crosses as an empty map, being a map's element"
    );
    assert!(
        returned("GetLicense", 0)["A"]["Features"].is_object(),
        "the licensed host has a licence"
    );
    let loaded = |nth: usize| -> String {
        go_calls
            .iter()
            .filter(|c| c["call"] == "LoadPluginConfiguration")
            .nth(nth)
            .expect("LoadPluginConfiguration")["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(
        loaded(0).contains(r#""secretkey":"secret-default""#),
        "{}",
        loaded(0)
    );
    assert!(
        loaded(1).contains(r#""secretkey":"hunter2""#),
        "{}",
        loaded(1)
    );
    assert!(loaded(1).contains(r#""onlydefault":7"#), "{}", loaded(1));
    assert!(
        loaded(1).contains(r#""large":100000000000000000000"#),
        "Go writes 1e20 in full: {}",
        loaded(1)
    );

    assert_eq!(
        go_side.1[PLUGIN_ID],
        Json::Null,
        "the last save, of a nil map, left null"
    );
    assert_eq!(go_side.1, rust_side.1, "the Plugins each host left");

    drop(rust);
    drop(go);
    common::delete_channel(client, admin, &channel).await;
}

// ---------------------------------------------------------------------------------------------
// The plugin API tranche: users, teams, channels, posts, permissions, bots and websocket events
// (Phase 6)
// ---------------------------------------------------------------------------------------------

/// The Rust host of the core tranche; see `second_server_ports`.
const CORE_HOST_PORT: u16 = 8144;
/// Its Go server.
const CORE_GO_OFFSET: u16 = 90;
/// The recorder's well-formed id that names nothing (`examples/recorder/core.rs`).
const CORE_MISSING: &str = "coremissingcoremissingcore";
/// Each side's tag: in its users' names, its bots' names and the channel it creates.
const CORE_SIDES: [&str; 2] = ["sidego", "siders"];

/// The plain users and the channels the core tour makes, for the cleanup.
static CORE_USERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Cross-server parity for the plugin API methods a plugin calls after activation
/// (`mm_app::plugin_api`, `mm_app::plugin_api_wire`): the user, team, channel, member, post,
/// thread and session reads; the three permission checks; `CreatePost`, `UpdatePost`,
/// `DeletePost`, the three ephemeral methods, the two reaction methods, `AddChannelMember`,
/// `CreateChannel`, `GetDirectChannel` and `GetGroupChannel`; the bot methods and
/// `EnsureBotUser`; and `PublishWebSocketEvent`.
///
/// A `!core-script` post, made by each side's **own** user in its **own** channel, makes the
/// recorder run `examples/recorder/core.rs` inside `MessageWillBePosted`. Three things are
/// compared: every answer, in order; every hook the script's writes fired on the recorder itself
/// (a `CreatePost` from inside a hook fires `MessageWillBePosted` again, on the same plugin); and
/// every websocket frame the own user's socket on that host received.
///
/// # What differs between the sides, and how it is taken out
///
/// Reads go to a reader and a channel both sides share and nobody writes. Writes go to what each
/// side owns — its own two users, its own channel, bots and a channel named with its tag — so the
/// ids and names differ, and they are **scrubbed**: the side's users, channel, token and session,
/// then every id the script learned from an answer (its bots, posts, ephemeral post, channel, DM,
/// GM and their names), each to one token. What is left is masked, and each mask is named:
/// [`normalise`]'s id and time keys, any other 26-character id outside the shared set as
/// `<minted>` (system posts, history rows, CSRF tokens), and a session's `ExpiresAt` and
/// `LastActivityAt`, which follow each side's own login.
#[tokio::test]
async fn the_plugin_api_core_methods_answer_as_go_answers() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    purge_core_rows().await;
    let outcome = std::panic::AssertUnwindSafe(run_the_core_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    purge_core_rows().await;
    let users = std::mem::take(
        &mut *CORE_USERS
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

/// The recorder's bots (and their users), its KV rows, and the channels the script created —
/// none of which carries the suite's name prefix, so the fixture purge never reaches them.
async fn purge_core_rows() {
    let pool = common::fixture_pool().await.expect("the stack database");
    for statement in [
        "DELETE FROM users WHERE id IN (SELECT userid FROM bots WHERE ownerid = $1)",
        "DELETE FROM bots WHERE ownerid = $1",
        "DELETE FROM pluginkeyvaluestore WHERE pluginid = $1",
    ] {
        sqlx::query(statement)
            .bind(PLUGIN_ID)
            .execute(&pool)
            .await
            .expect("the recorder's rows go");
    }
    // By username too: a bot whose `Bots` row went without its `Users` row (a mutation did
    // exactly that) is invisible to the owner filter above and keeps its username taken.
    let usernames: Vec<String> = CORE_SIDES
        .iter()
        .flat_map(|side| {
            ["corebot", "corebotb", "ensured"]
                .into_iter()
                .map(move |prefix| format!("{prefix}{side}"))
        })
        .collect();
    for statement in [
        "DELETE FROM bots WHERE userid IN (SELECT id FROM users WHERE username = ANY($1))",
        "DELETE FROM users WHERE username = ANY($1)",
    ] {
        sqlx::query(statement)
            .bind(&usernames)
            .execute(&pool)
            .await
            .expect("the script's bot users go");
    }
    for side in CORE_SIDES {
        let name = format!("core{side}");
        for statement in [
            "DELETE FROM publicchannels WHERE name = $1",
            "DELETE FROM channelmembers WHERE channelid IN (SELECT id FROM channels WHERE name = $1)",
            "DELETE FROM channels WHERE name = $1",
        ] {
            sqlx::query(statement)
                .bind(&name)
                .execute(&pool)
                .await
                .expect("the script's channel goes");
        }
    }
}

/// One side of the core tour: its users, its channel, and what it recorded.
struct CoreSide {
    tag: &'static str,
    own: common::PlainUser,
    other: common::PlainUser,
    channel: String,
}

impl CoreSide {
    /// What the host passes down to the recorder for this side.
    fn env(&self, shared: &CoreShared) -> Vec<(&'static str, String)> {
        vec![
            ("HOOK_RECORDER_CORE_READER", shared.reader.id.clone()),
            (
                "HOOK_RECORDER_CORE_READ_CHANNEL",
                shared.read_channel.clone(),
            ),
            ("HOOK_RECORDER_CORE_ADMIN", shared.admin_id.clone()),
            ("HOOK_RECORDER_CORE_OWN", self.own.id.clone()),
            ("HOOK_RECORDER_CORE_OTHER", self.other.id.clone()),
            ("HOOK_RECORDER_CORE_SIDE", self.tag.to_owned()),
        ]
    }
}

/// What both sides read and never write.
struct CoreShared {
    reader: common::PlainUser,
    read_channel: String,
    admin_id: String,
    team: String,
}

impl CoreShared {
    /// The ids both sides may legitimately carry, which the `<minted>` mask leaves alone.
    fn ids(&self) -> Vec<String> {
        vec![
            self.reader.id.clone(),
            self.read_channel.clone(),
            self.admin_id.clone(),
            self.team.clone(),
            CORE_MISSING.to_owned(),
        ]
    }
}

/// The value at `pointer` in the `n`th answer (0-based) of the call named `name`.
fn learned(calls: &[Json], name: &str, n: usize, pointer: &str) -> Option<String> {
    calls
        .iter()
        .filter(|c| c["call"] == name)
        .nth(n)
        .and_then(|c| c.pointer(pointer))
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// This side's scrub pairs, the ones the script's answers taught it first — a DM's name holds
/// both users' ids, so it has to go before they do.
fn core_pairs(side: &CoreSide, calls: &[Json]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (name, n, pointer, token) in [
        ("GetDirectChannel", 0, "/returns/A/Name", "<dm-name>"),
        ("GetGroupChannel", 0, "/returns/A/Name", "<gm-name>"),
        ("GetDirectChannel", 0, "/returns/A/Id", "<dm>"),
        ("GetGroupChannel", 0, "/returns/A/Id", "<gm>"),
        ("CreateChannel", 0, "/returns/A/Id", "<created-channel>"),
        ("CreateBot", 0, "/returns/A/UserId", "<bot>"),
        ("EnsureBotUser", 2, "/returns/A", "<ensured>"),
        ("CreatePost", 0, "/returns/A/Id", "<root>"),
        ("CreatePost", 1, "/returns/A/Id", "<reply>"),
        ("SendEphemeralPost", 0, "/returns/A/Id", "<ephemeral>"),
        ("GetSession", 0, "/args/A", "<session>"),
    ] {
        if let Some(value) = learned(calls, name, n, pointer) {
            pairs.push((value, token.to_owned()));
        }
    }
    pairs.extend([
        (side.own.id.clone(), "<own>".to_owned()),
        (side.other.id.clone(), "<other>".to_owned()),
        (side.channel.clone(), "<own-channel>".to_owned()),
        (side.own.token.clone(), "<token>".to_owned()),
        (side.tag.to_owned(), "<side>".to_owned()),
    ]);
    pairs
}

fn is_minted_id(text: &str) -> bool {
    text.len() == 26
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The masks left after the scrub: any 26-character id outside `shared` is `<minted>`, and a
/// session's two clocks, which follow each side's own login, are `<time>` when set.
fn mask_core(value: &mut Json, shared: &[String]) {
    match value {
        Json::Array(items) => items.iter_mut().for_each(|v| mask_core(v, shared)),
        Json::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if matches!(
                    key.as_str(),
                    "ExpiresAt" | "LastActivityAt" | "expires_at" | "last_activity_at"
                ) && entry.as_i64().is_some_and(|n| n != 0)
                {
                    *entry = Json::String("<time>".to_owned());
                    continue;
                }
                // `multiple_channels_viewed`: each channel's view time, stamped by each side's own
                // trigger post.
                if key == "channel_times" {
                    if let Some(times) = entry.as_object_mut() {
                        times
                            .values_mut()
                            .for_each(|t| *t = Json::String("<time>".to_owned()));
                    }
                }
                mask_core(entry, shared);
            }
        }
        Json::String(text) => *text = mask_minted_in(text, shared),
        _ => {}
    }
}

/// Every 26-character id in `text` — the whole string, or a word of it such as the `id=…` a
/// validation error's `DetailedError` carries — that is not in `shared`, as `<minted>`.
fn mask_minted_in(text: &str, shared: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if is_minted_id(word) && !shared.contains(word) {
            out.push_str("<minted>");
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// A websocket frame as it is compared: scrubbed, `seq` dropped, each JSON document carried as a
/// string (a post) parsed so its ids and times can be masked, then masked like a body.
fn core_frame(raw: &str, pairs: &[(String, String)], shared: &[String]) -> Option<Json> {
    let mut frame: Json = serde_json::from_str(&scrub(raw, pairs)).ok()?;
    if frame["event"] == "hello" || frame.get("event").is_none() {
        return None;
    }
    if let Some(map) = frame.as_object_mut() {
        map.remove("seq");
    }
    if let Some(data) = frame.get_mut("data").and_then(Json::as_object_mut) {
        for value in data.values_mut() {
            if let Some(text) = value.as_str() {
                if text.starts_with('{') || text.starts_with('[') {
                    if let Ok(parsed) = serde_json::from_str::<Json>(text) {
                        *value = parsed;
                    }
                }
            }
        }
    }
    // `group_added`'s ids are in **id** order (`GetGroupNameFromUserIds` sorts the slice in place),
    // which each side's own ids decide differently; the suite asserts the raw order is sorted and
    // compares the scrubbed ones as a set.
    if let Some(ids) = frame
        .pointer_mut("/data/teammate_ids")
        .and_then(Json::as_array_mut)
    {
        ids.sort_by_key(Json::to_string);
    }
    normalise_body(&mut frame);
    mask_core(&mut frame, shared);
    Some(frame)
}

/// Whether every `group_added` frame's `teammate_ids` is in ascending id order, as Go sends it.
fn group_added_ids_are_sorted(raw: &[String]) -> bool {
    raw.iter()
        .filter_map(|r| serde_json::from_str::<Json>(r).ok())
        .filter(|f| f["event"] == "group_added")
        .all(|f| {
            let ids: Vec<String> = f["data"]["teammate_ids"]
                .as_str()
                .and_then(|t| serde_json::from_str(t).ok())
                .unwrap_or_default();
            !ids.is_empty() && ids.windows(2).all(|w| w[0] <= w[1])
        })
}

/// Wait until the transcript holds the script's entry and then stops growing: the script's
/// writes fire detached hooks, which land on their own schedule on both hosts.
async fn core_transcript_settles(path: &Path, side: &str) {
    let mut last = usize::MAX;
    let mut quiet_since = None;
    for _ in 0..400 {
        let entries = transcript(path);
        let done = entries.iter().any(|e| e["hook"] == "CoreScript");
        if done && entries.len() == last {
            let since = *quiet_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= QUIET * 2 {
                return;
            }
        } else {
            quiet_since = None;
        }
        last = entries.len();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{side}: the core script never settled: {:?}",
        names(&transcript(path))
    );
}

/// Who the reply's row says deleted it: `DeletePost` passes the plugin's id, which the store
/// writes into the props as `deleteBy` and no answer or hook carries.
async fn core_deleted_by(calls: &[Json]) -> Option<String> {
    let id = learned(calls, "CreatePost", 1, "/returns/A/Id")?;
    let pool = common::fixture_pool().await.expect("the stack database");
    let (by,): (Option<String>,) =
        sqlx::query_as("SELECT props->>'deleteBy' FROM posts WHERE id = $1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .expect("the deleted reply");
    by
}

/// How many bots the script made, and how many of their `Users` rows are left. Read from the
/// database, because Go's `GetUser` answers a permanently deleted bot from its user cache.
async fn core_bot_user_rows(calls: &[Json]) -> (usize, i64) {
    let ids: Vec<String> = [
        learned(calls, "CreateBot", 0, "/returns/A/UserId"),
        learned(calls, "EnsureBotUser", 2, "/returns/A"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let pool = common::fixture_pool().await.expect("the stack database");
    let (left,): (i64,) = sqlx::query_as("SELECT count(*) FROM users WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_one(&pool)
        .await
        .expect("the bot users");
    (ids.len(), left)
}

/// Read the socket until a whole window passes with nothing new.
async fn core_frames_settle(probe: &mut common::SocketProbe) {
    for _ in 0..20 {
        let before = probe.raw.len();
        probe.collect_for(Duration::from_millis(900)).await;
        if probe.raw.len() == before {
            return;
        }
    }
}

async fn run_the_core_tour(client: &reqwest::Client, admin: &str) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-core");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(client, admin, Some(true)).await;

    // Every fixture is made through **main** Go, which hosts no plugins.
    let team = common::create_team(client, admin, "hookcore").await;
    let read_channel = common::create_channel(client, admin, &team, "hookcoreread").await;
    let reader = common::create_plain_user(client, admin, &team, "corerd").await;
    common::add_user_to_channel(client, admin, &read_channel, &reader.id).await;
    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let shared = CoreShared {
        reader,
        read_channel,
        admin_id: me["id"].as_str().expect("an id").to_owned(),
        team: team.clone(),
    };
    CORE_USERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(shared.reader.id.clone());

    let mut sides = Vec::new();
    for tag in CORE_SIDES {
        let own = common::create_plain_user(client, admin, &team, &format!("coreown{tag}")).await;
        let other = common::create_plain_user(client, admin, &team, &format!("coreoth{tag}")).await;
        CORE_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([own.id.clone(), other.id.clone()]);
        let channel = common::create_channel(client, admin, &team, &format!("core{tag}")).await;
        common::add_user_to_channel(client, admin, &channel, &own.id).await;
        sides.push(CoreSide {
            tag,
            own,
            other,
            channel,
        });
    }

    let go_env = sides[0].env(&shared);
    let mut env: Vec<(&str, &str)> = vec![("HOOK_RECORDER_TRANSCRIPT", "")];
    let go_transcript = go_log.to_string_lossy().into_owned();
    env[0].1 = &go_transcript;
    env.extend(go_env.iter().map(|(k, v)| (*k, v.as_str())));
    let go = start_go(&go_run, &env, CORE_GO_OFFSET).await;

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let rust_env = sides[1].env(&shared);
    let mut env: Vec<(&str, &str)> = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    env.extend(rust_env.iter().map(|(k, v)| (*k, v.as_str())));
    let rust = SecondServer::start_in(CORE_HOST_PORT, &rs_run, &env)
        .await
        .expect("the Rust host starts");
    wait_until_running(client, admin, &go.base).await;
    wait_until_running(client, admin, &rust.base).await;

    // Each side in turn: its socket open, its trigger, its transcript and frames settled.
    let shared_ids = shared.ids();
    let mut recorded = Vec::new();
    let mut bot_rows = Vec::new();
    for (side, base, log, host) in [
        (&sides[0], go.base.as_str(), go_log.as_path(), "Go"),
        (&sides[1], rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        let mut probe = common::SocketProbe::connect(base, &side.own.token).await;
        let body = serde_json::to_vec(&serde_json::json!({
            "channel_id": side.channel,
            "message": "!core-script",
        }))
        .expect("the post");
        let (status, answer, served_by) = request_raw(
            client,
            base,
            reqwest::Method::POST,
            Some(&side.own.token),
            "/api/v4/posts",
            Some(&body),
        )
        .await;
        assert_eq!(
            status,
            201,
            "{host}: the trigger: {}",
            String::from_utf8_lossy(&answer)
        );
        if host == "Rust" {
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "the trigger was forwarded"
            );
        }
        core_transcript_settles(log, host).await;
        core_frames_settle(&mut probe).await;

        // The ids are learned from the lines as written, before anything is normalised.
        let calls = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Json>(line).ok())
            .find(|e| e["hook"] == "CoreScript")
            .and_then(|e| e["calls"].as_array().cloned())
            .unwrap_or_default();
        let pairs = core_pairs(side, &calls);
        let mut entries = transcript_of(log, &pairs);
        entries.iter_mut().for_each(|e| mask_core(e, &shared_ids));
        let mut frames: Vec<Json> = probe
            .raw
            .iter()
            .filter_map(|raw| core_frame(raw, &pairs, &shared_ids))
            .collect();
        frames.sort_by_key(Json::to_string);
        recorded.push((entries, frames, probe.raw.clone()));
        bot_rows.push(core_bot_user_rows(&calls).await);
        assert_eq!(
            core_deleted_by(&calls).await.as_deref(),
            Some(PLUGIN_ID),
            "{host}: the plugin is recorded as the reply's deleter"
        );
    }
    let (go_side, rust_side) = (&recorded[0], &recorded[1]);

    // The answers, in order.
    let script = |entries: &[Json]| -> Vec<Json> {
        entries
            .iter()
            .find(|e| e["hook"] == "CoreScript")
            .and_then(|e| e["calls"].as_array().cloned())
            .expect("the script ran")
    };
    let (go_calls, rust_calls) = (script(&go_side.0), script(&rust_side.0));
    assert!(
        go_calls.iter().all(|c| c.get("error").is_none()),
        "Go implements every method the script calls: {:?}",
        go_calls
            .iter()
            .filter(|c| c.get("error").is_some())
            .collect::<Vec<_>>()
    );
    for (index, (g, r)) in go_calls.iter().zip(&rust_calls).enumerate() {
        assert_eq!(g, r, "call {index} ({})", g["call"]);
    }
    assert_eq!(
        go_calls.len(),
        rust_calls.len(),
        "the script ran to the end"
    );

    // The hooks the script's writes fired, in canonical order: the detached ones land when they
    // land on both hosts.
    let hooks = |entries: &[Json]| -> Vec<Json> {
        let rest: Vec<Json> = entries
            .iter()
            .filter(|e| e["hook"] != "CoreScript")
            .cloned()
            .collect();
        in_canonical_order(&rest)
    };
    let (go_hooks, rust_hooks) = (hooks(&go_side.0), hooks(&rust_side.0));
    assert_eq!(names(&go_hooks), names(&rust_hooks), "the hooks that fired");
    for (index, (g, r)) in go_hooks.iter().zip(&rust_hooks).enumerate() {
        assert_eq!(g, r, "hook {index} ({})", g["hook"]);
    }

    // Every frame the own user's socket received, in canonical order.
    let event_names = |frames: &[Json]| -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f["event"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(
        event_names(&go_side.1),
        event_names(&rust_side.1),
        "the events the own user received"
    );
    for (index, (g, r)) in go_side.1.iter().zip(&rust_side.1).enumerate() {
        assert_eq!(g, r, "frame {index} ({})", g["event"]);
    }

    for (side, host) in [(go_side, "Go"), (rust_side, "Rust")] {
        assert!(
            group_added_ids_are_sorted(&side.2),
            "{host}: group_added's teammate ids are in id order"
        );
    }

    // What parity alone would not pin, because both hosts could agree on a wrong answer.
    let answer = |name: &str, n: usize| -> Json {
        go_calls
            .iter()
            .filter(|c| c["call"] == name)
            .nth(n)
            .map(|c| c["returns"].clone())
            .unwrap_or(Json::Null)
    };
    let permissions: Vec<Json> = (0..3)
        .map(|n| answer("HasPermissionTo", n)["A"].clone())
        .chain((0..4).map(|n| answer("HasPermissionToTeam", n)["A"].clone()))
        .chain((0..4).map(|n| answer("HasPermissionToChannel", n)["A"].clone()))
        .collect();
    let (t, f) = (Json::Bool(true), Json::Null);
    assert_eq!(
        permissions,
        [
            t.clone(),
            f.clone(),
            t.clone(),
            t.clone(),
            f.clone(),
            f.clone(),
            t.clone(),
            t.clone(),
            f.clone(),
            f.clone(),
            f,
        ],
        "the permission answers (false is gob's omitted zero)"
    );
    assert_eq!(
        answer("GetUser", 1)["B"]["StatusCode"],
        404,
        "a missing user is Go's 404"
    );
    assert_eq!(
        answer("CreateBot", 1)["B"]["Id"],
        "plugin_api.bot_cant_create_bot",
        "a bot cannot own a bot"
    );
    assert_eq!(
        answer("GetBot", 0)["A"]["OwnerId"],
        PLUGIN_ID,
        "a bot with no owner is the plugin's"
    );
    assert_eq!(
        answer("EnsureBotUser", 2)["A"],
        answer("EnsureBotUser", 3)["A"],
        "the second EnsureBot finds the first bot"
    );
    assert_eq!(
        answer("CreatePost", 0)["A"]["Props"]["$map"]["from_plugin"]["value"],
        "true",
        "a plugin's post says so"
    );
    assert_eq!(
        answer("CreatePost", 2)["A"]["Props"]["$map"]["silent_notification"]["value"],
        true,
        "a plugin may post silently as a human"
    );
    assert_eq!(
        answer("GetChannelMember", 2)["B"]["StatusCode"],
        404,
        "a channel a plugin creates has no members"
    );
    for (host, bots) in [("Go", &bot_rows[0]), ("Rust", &bot_rows[1])] {
        assert_eq!(
            bots,
            &(2, 0),
            "{host}: both bots were made, and PermanentDeleteBot left no Users row behind"
        );
    }
    let custom = |name: &str| -> usize {
        go_side
            .1
            .iter()
            .filter(|f| f["event"] == format!("custom_{PLUGIN_ID}_{name}"))
            .count()
    };
    assert_eq!(
        (
            custom("to_user"),
            custom("to_channel"),
            custom("omitted"),
            custom("empty")
        ),
        (1, 1, 0, 1),
        "who received the plugin's events: {:?}",
        event_names(&go_side.1)
    );

    drop(rust);
    drop(go);
    for side in &sides {
        common::delete_channel(client, admin, &side.channel).await;
    }
    common::delete_channel(client, admin, &shared.read_channel).await;
}

// ---------------------------------------------------------------------------------------------
// The slash-command tranche
// ---------------------------------------------------------------------------------------------

/// The Rust host of the slash-command tranche; see `second_server_ports`.
const COMMAND_HOST_PORT: u16 = 8145;
/// Its Go server.
const COMMAND_GO_OFFSET: u16 = 91;
/// Each side's tag, in its users' names and its channels'.
const COMMAND_SIDES: [&str; 2] = ["cmdsidego", "cmdsiders"];

/// The plain users the command tour makes, for the cleanup.
static COMMAND_USERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Cross-server parity for **plugin slash commands** under each host (`mm_app::plugin_commands`,
/// the execute handler in `mm_api::commands`): the recorder registers its commands on activation
/// (`examples/recorder/commands.rs`, switched on by `HOOK_RECORDER_COMMANDS`), and each side's
/// own user runs the same commands through `POST /api/v4/commands/execute` in its own channel.
///
/// Compared: the registrations' answers; every command's status and body; every hook the
/// recorder saw (the `ExecuteCommand`s, and the message hooks an in-channel response fires); the
/// command script's plugin API answers; every websocket frame the own user received; the posts
/// the responses wrote; and the autocomplete list and suggestions. Masked: ids minted per side
/// (`<minted>`), the trigger id (checked for shape first), the session id, the site URL's host.
///
/// Also pinned, because both hosts could agree on a wrong answer: a plugin's `/shrug` answers
/// instead of the built-in's; an unknown trigger and a plugin's "nothing" are **forwarded** by
/// the Rust host and 404 on both; and a plugin disabled through the Rust host takes its commands
/// with it.
#[tokio::test]
async fn plugin_slash_commands_run_as_go_runs_them() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_command_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    let users = std::mem::take(
        &mut *COMMAND_USERS
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

/// One side of the command tour.
struct CommandSide {
    tag: &'static str,
    own: common::PlainUser,
    channel: String,
    /// An open channel the own user is not in, for the forbidden redirect.
    closed: String,
}

impl CommandSide {
    fn pairs(&self, base: &str) -> Vec<(String, String)> {
        let host = base.trim_start_matches("http://").to_owned();
        vec![
            (self.own.id.clone(), "<own>".to_owned()),
            (self.channel.clone(), "<own-channel>".to_owned()),
            (self.closed.clone(), "<closed-channel>".to_owned()),
            (self.own.token.clone(), "<token>".to_owned()),
            (host, "<host>".to_owned()),
            (self.tag.to_owned(), "<side>".to_owned()),
        ]
    }

    /// The commands the tour runs, in order.
    fn commands(&self) -> Vec<String> {
        vec![
            format!(
                "/hookrec ephemeral <!channel> <!here> <@{}> hi @{} ~{}",
                self.own.id,
                common::plain_username(&format!("cmdown{}", self.tag)),
                format!("mmrs-parity-cmd{}", self.tag),
            ),
            format!("/hookrec in_channel posted for <@{}>", self.own.id),
            "/hookrec plain no type".to_owned(),
            "/hookrec skip <!channel> stays as it is".to_owned(),
            "/hookrec props".to_owned(),
            "/hookrec goto".to_owned(),
            "/hookrec extra".to_owned(),
            format!("/hookrec forbidden {}", self.closed),
            "/hookrec system".to_owned(),
            "/hookrec error".to_owned(),
            "/hookrec error-skip".to_owned(),
            "/hookrec error-status".to_owned(),
            "/HookRec ephemeral upper".to_owned(),
            "/hookreccase".to_owned(),
            "/hookrecteam ephemeral team".to_owned(),
            "/shrug the plugin's".to_owned(),
            "/hookrec script".to_owned(),
            "/hookrec nothing".to_owned(),
            "/nosuchhookrec at all".to_owned(),
        ]
    }
}

/// The commands the Rust host must hand on to Go: no plugin answers them.
const COMMAND_FORWARDED: [&str; 2] = ["/hookrec nothing", "/nosuchhookrec at all"];

/// Every `TriggerId` in an `ExecuteCommand` entry, decoded: `<client id>:<user id>:<millis>:
/// <signature>`. Read before anything is masked.
fn trigger_ids(log: &Path) -> Vec<Vec<String>> {
    use base64::Engine as _;
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Json>(line).ok())
        .filter(|e| e["hook"] == "ExecuteCommand")
        .filter_map(|e| e["args"]["B"]["TriggerId"].as_str().map(str::to_owned))
        .map(|raw| {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(raw.as_bytes())
                .unwrap_or_default();
            String::from_utf8_lossy(&decoded)
                .split(':')
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

/// The masks this tranche adds to [`mask_core`]'s: a trigger id and a session id, which each
/// side mints; a site URL, whose host is each side's own.
fn mask_command(value: &mut Json) {
    match value {
        Json::Array(items) => items.iter_mut().for_each(mask_command),
        Json::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if matches!(key.as_str(), "TriggerId" | "SessionId" | "SiteURL")
                    && entry.as_str().is_some_and(|s| !s.is_empty())
                {
                    *entry = Json::String(format!("<{key}>"));
                    continue;
                }
                mask_command(entry);
            }
        }
        _ => {}
    }
}

/// Wait until the transcript holds `hook` and then stops growing.
async fn command_transcript_settles(path: &Path, hook: &str, side: &str) {
    let mut last = usize::MAX;
    let mut quiet_since = None;
    for _ in 0..400 {
        let entries = transcript(path);
        let done = entries.iter().any(|e| e["hook"] == hook);
        if done && entries.len() == last {
            let since = *quiet_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= QUIET * 2 {
                return;
            }
        } else {
            quiet_since = None;
        }
        last = entries.len();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{side}: the transcript never settled after {hook}: {:?}",
        names(&transcript(path))
    );
}

/// The own user's posts in the own channel, oldest first: type, message and props, which is what
/// a command response decides.
async fn command_posts(channel: &str, user: &str) -> Vec<Json> {
    let pool = common::fixture_pool().await.expect("the stack database");
    let rows: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT type, message, props::text, rootid <> '' FROM posts
         WHERE channelid = $1 AND userid = $2 ORDER BY createat, id",
    )
    .bind(channel)
    .bind(user)
    .fetch_all(&pool)
    .await
    .expect("the posts");
    rows.into_iter()
        .map(|(kind, message, props, reply)| {
            let props: Json = props
                .and_then(|p| serde_json::from_str(&p).ok())
                .unwrap_or(Json::Null);
            serde_json::json!({ "type": kind, "message": message, "props": props, "reply": reply })
        })
        .collect()
}

async fn run_the_command_tour(client: &reqwest::Client, admin: &str) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-commands");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(client, admin, Some(true)).await;

    // Every fixture is made through **main** Go, which hosts no plugins.
    let team = common::create_team(client, admin, "hookcmd").await;
    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let shared: Vec<String> = vec![team.clone(), me["id"].as_str().expect("an id").to_owned()];
    let mut sides = Vec::new();
    for tag in COMMAND_SIDES {
        let own = common::create_plain_user(client, admin, &team, &format!("cmdown{tag}")).await;
        COMMAND_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(own.id.clone());
        let channel = common::create_channel(client, admin, &team, &format!("cmd{tag}")).await;
        common::add_user_to_channel(client, admin, &channel, &own.id).await;
        let closed = common::create_channel(client, admin, &team, &format!("cmdx{tag}")).await;
        sides.push(CommandSide {
            tag,
            own,
            channel,
            closed,
        });
    }

    let go_transcript = go_log.to_string_lossy().into_owned();
    let go = start_go(
        &go_run,
        &[
            ("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str()),
            ("HOOK_RECORDER_COMMANDS", "1"),
            ("HOOK_RECORDER_COMMAND_TEAM", team.as_str()),
        ],
        COMMAND_GO_OFFSET,
    )
    .await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    // `ExecuteSlashCommand` hands the plugin `GetSiteURL()`, the configured one; the Go host has
    // it from `start_go`, so the Rust host gets its own too (masked, but set on both).
    let offset: u16 = std::env::var("MMRS_PORT_OFFSET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let site_url = format!("http://localhost:{}", COMMAND_HOST_PORT + offset);
    let rust = SecondServer::start_in(
        COMMAND_HOST_PORT,
        &rs_run,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
            ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
            ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
            ("HOOK_RECORDER_COMMANDS", "1"),
            ("HOOK_RECORDER_COMMAND_TEAM", team.as_str()),
            ("MM_SERVICESETTINGS_SITEURL", site_url.as_str()),
        ],
    )
    .await
    .expect("the Rust host starts");
    wait_until_running(client, admin, &go.base).await;
    wait_until_running(client, admin, &rust.base).await;
    command_transcript_settles(&go_log, "OnActivate", "Go").await;
    command_transcript_settles(&rust_log, "OnActivate", "Rust").await;
    let activated = (transcript(&go_log).len(), transcript(&rust_log).len());

    // Each side in turn: its socket open, its commands, its transcript and frames settled.
    let mut answers = Vec::new();
    let mut recorded = Vec::new();
    for (side, base, log, host) in [
        (&sides[0], go.base.as_str(), go_log.as_path(), "Go"),
        (&sides[1], rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        let pairs = side.pairs(base);
        let mut probe = common::SocketProbe::connect(base, &side.own.token).await;
        let mut side_answers = Vec::new();
        let mut client_trigger_ids = Vec::new();
        for command in side.commands() {
            let body = serde_json::to_vec(&serde_json::json!({
                "channel_id": side.channel,
                "team_id": team,
                "command": command,
            }))
            .expect("the body");
            let (status, answer, served_by) = request_raw(
                client,
                base,
                reqwest::Method::POST,
                Some(&side.own.token),
                "/api/v4/commands/execute",
                Some(&body),
            )
            .await;
            let scrubbed = scrub(&String::from_utf8_lossy(&answer), &pairs);
            let mut value: Json = serde_json::from_str(&scrubbed)
                .unwrap_or_else(|_| Json::String(scrubbed.trim_end().to_owned()));
            if let Some(id) = value["trigger_id"].as_str().filter(|s| !s.is_empty()) {
                client_trigger_ids.push(id.to_owned());
            }
            value.as_object_mut().map(|m| m.remove("request_id"));
            mask_core(&mut value, &shared);
            let scrubbed_command = scrub(&command, &pairs);
            if host == "Rust" {
                let forwarded = COMMAND_FORWARDED.contains(&command.as_str());
                assert_eq!(
                    served_by.as_deref() == Some("rust"),
                    !forwarded,
                    "{scrubbed_command}: served by {served_by:?}"
                );
            }
            side_answers.push((scrubbed_command, status, value));
        }
        command_transcript_settles(log, "CommandScript", host).await;
        core_frames_settle(&mut probe).await;

        // The trigger ids the plugin was handed: the user's, and each response's client half.
        let triggers = trigger_ids(log);
        assert!(
            triggers
                .iter()
                .all(|t| t.len() == 4 && t[0].len() == 26 && t[1] == side.own.id),
            "{host}: every trigger id is <client>:<user>:<millis>:<signature>: {triggers:?}"
        );
        for id in &client_trigger_ids {
            assert!(
                triggers.iter().any(|t| &t[0] == id),
                "{host}: the response's trigger id {id} is the one the plugin was handed"
            );
        }

        let mut entries = transcript_of(log, &pairs);
        entries.iter_mut().for_each(|e| {
            mask_core(e, &shared);
            mask_command(e);
        });
        let mut frames: Vec<Json> = probe
            .raw
            .iter()
            .filter_map(|raw| core_frame(raw, &pairs, &shared))
            .collect();
        frames.sort_by_key(Json::to_string);
        let mut posts = command_posts(&side.channel, &side.own.id).await;
        for post in &mut posts {
            *post = serde_json::from_str(&scrub(&post.to_string(), &pairs)).expect("json");
            mask_core(post, &shared);
        }
        answers.push(side_answers);
        recorded.push((entries, frames, posts));
    }

    // The registrations on activation.
    let (go_side, rust_side) = (&recorded[0], &recorded[1]);
    let activation = |entries: &[Json]| -> Json {
        entries
            .iter()
            .find(|e| e["hook"] == "OnActivate")
            .cloned()
            .expect("the recorder activated")
    };
    let go_activation = activation(&go_side.0);
    assert_eq!(go_activation, activation(&rust_side.0), "the registrations");
    let refusals: Vec<Json> = go_activation["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|c| c["returns"]["A"].clone())
        .collect();
    assert_eq!(
        refusals.iter().filter(|r| r.is_null()).count(),
        4,
        "four registrations are accepted: {refusals:?}"
    );
    let refusal_text = serde_json::to_string(&refusals).expect("json");
    for text in [
        "invalid command",
        "invalid autocomplete data in command: Command should be lowercase",
    ] {
        assert!(
            refusal_text.contains(text),
            "Go refuses with {text:?}: {refusal_text}"
        );
    }
    assert_eq!(
        activated.0, activated.1,
        "both hosts wrote the same entries on activation"
    );

    // Every command's answer.
    for ((command, gs, gb), (_, rs, rb)) in answers[0].iter().zip(&answers[1]) {
        assert_eq!((gs, gb), (rs, rb), "{command}");
    }
    assert_eq!(answers[0].len(), answers[1].len());

    // The command script's answers, in order.
    let script = |entries: &[Json]| -> Vec<Json> {
        entries
            .iter()
            .find(|e| e["hook"] == "CommandScript")
            .and_then(|e| e["calls"].as_array().cloned())
            .expect("the script ran")
    };
    let (go_calls, rust_calls) = (script(&go_side.0), script(&rust_side.0));
    assert!(
        go_calls.iter().all(|c| c.get("error").is_none()),
        "Go implements every method the script calls: {go_calls:?}"
    );
    for (index, (g, r)) in go_calls.iter().zip(&rust_calls).enumerate() {
        assert_eq!(g, r, "script call {index} ({})", g["call"]);
    }
    assert_eq!(
        go_calls.len(),
        rust_calls.len(),
        "the script ran to the end"
    );

    // Every other hook, in canonical order: the message hooks are detached on both hosts.
    let hooks = |entries: &[Json]| -> Vec<Json> {
        let rest: Vec<Json> = entries
            .iter()
            .filter(|e| e["hook"] != "CommandScript" && e["hook"] != "OnActivate")
            .cloned()
            .collect();
        in_canonical_order(&rest)
    };
    let (go_hooks, rust_hooks) = (hooks(&go_side.0), hooks(&rust_side.0));
    assert_eq!(names(&go_hooks), names(&rust_hooks), "the hooks that fired");
    for (index, (g, r)) in go_hooks.iter().zip(&rust_hooks).enumerate() {
        assert_eq!(g, r, "hook {index} ({})", g["hook"]);
    }

    // The frames and the posts.
    let events = |frames: &[Json]| -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f["event"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(events(&go_side.1), events(&rust_side.1), "the events");
    for event in ["ephemeral_message", "posted"] {
        assert!(
            events(&go_side.1).iter().any(|e| e == event),
            "the own user received {event}: {:?}",
            events(&go_side.1)
        );
    }
    // Eighteen commands reach the plugin through the route (all but the unknown trigger), and
    // three more through `ExecuteSlashCommand`.
    assert_eq!(
        names(&go_hooks)
            .iter()
            .filter(|n| *n == "ExecuteCommand")
            .count(),
        21,
        "the ExecuteCommand calls: {:?}",
        names(&go_hooks)
    );
    assert!(
        names(&go_hooks).iter().any(|n| n == "MessageHasBeenPosted"),
        "an in-channel response fires the message hooks"
    );
    for (index, (g, r)) in go_side.1.iter().zip(&rust_side.1).enumerate() {
        assert_eq!(g, r, "frame {index} ({})", g["event"]);
    }
    assert_eq!(go_side.2, rust_side.2, "the posts the responses wrote");

    // What parity alone would not pin.
    let answer = |prefix: &str| -> (u16, Json) {
        answers[0]
            .iter()
            .find(|(c, _, _)| c.starts_with(prefix))
            .map(|(_, s, b)| (*s, b.clone()))
            .unwrap_or_else(|| panic!("no answer for {prefix}"))
    };
    assert_eq!(
        answer("/shrug").1["text"],
        "hookrec: /shrug the plugin's",
        "a plugin's /shrug overrides the built-in"
    );
    let first = answer("/hookrec ephemeral <!channel>").1;
    assert!(
        first["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("@channel @here @mmrsplaincmdown<side> hi")),
        "the response text is Slack-processed: {first}"
    );
    assert_eq!(
        answer("/hookrec skip").1["text"],
        "<!channel> stays as it is"
    );
    assert_eq!(
        answer("/hookrec goto").1["goto_location"],
        "https://example.com/hookrec"
    );
    for (prefix, status, id) in [
        (
            "/hookrec error-skip",
            409,
            "mmrs.hookrecorder.command_error_skip",
        ),
        (
            "/hookrec error-status",
            500,
            "mmrs.hookrecorder.command_error_status",
        ),
        ("/hookrec error", 418, "mmrs.hookrecorder.command_error"),
        (
            "/hookrec forbidden",
            500,
            "api.command.execute_command.create_post_failed.app_error",
        ),
        (
            "/hookrec system",
            500,
            "api.command.execute_command.create_post_failed.app_error",
        ),
        (
            "/hookrec nothing",
            404,
            "api.command.execute_command.not_found.app_error",
        ),
        (
            "/nosuchhookrec",
            404,
            "api.command.execute_command.not_found.app_error",
        ),
    ] {
        let (s, body) = answer(prefix);
        assert_eq!((s, body["id"].as_str()), (status, Some(id)), "{prefix}");
    }
    assert_eq!(
        answer("/hookrec error-skip").1["message"],
        "the hook recorder refuses this command",
        "a plugin error that skips translation keeps its message"
    );
    let in_channel: Vec<&Json> = go_side
        .2
        .iter()
        .filter(|p| p["message"] == "posted for @mmrsplaincmdown<side>")
        .collect();
    assert_eq!(
        in_channel.len(),
        1,
        "the in-channel response is one post: {:?}",
        go_side.2
    );
    assert!(
        go_side.2.iter().any(|p| p["message"] == "the second"),
        "an extra in-channel response is posted too"
    );
    let script_posts = go_side
        .2
        .iter()
        .filter(|p| p["message"] == "posted by the script")
        .count();
    assert_eq!(
        script_posts, 1,
        "ExecuteSlashCommand's in-channel response is posted"
    );

    // The autocomplete list and suggestions, the plugin half included.
    for path in [
        format!("/api/v4/teams/{team}/commands/autocomplete"),
        format!(
            "/api/v4/teams/{team}/commands/autocomplete_suggestions?user_input=%2Fhookrec%20&channel_id={}",
            sides[0].channel
        ),
        format!(
            "/api/v4/teams/{team}/commands/autocomplete_suggestions?user_input=%2Fhookrec%20pick%20&channel_id={}",
            sides[0].channel
        ),
    ] {
        let (gs, gb, _) = request_raw(
            client,
            &go.base,
            reqwest::Method::GET,
            Some(&sides[0].own.token),
            &path,
            None,
        )
        .await;
        let (rs, rb, served_by) = request_raw(
            client,
            &rust.base,
            reqwest::Method::GET,
            Some(&sides[0].own.token),
            &path,
            None,
        )
        .await;
        assert_eq!(served_by.as_deref(), Some("rust"), "{path} was forwarded");
        let decode = |bytes: &[u8]| -> Json {
            let mut value: Json = serde_json::from_slice(bytes).unwrap_or(Json::Null);
            // The built-ins are in Go's map order.
            if let Some(items) = value.as_array_mut() {
                items.sort_by_key(Json::to_string);
            }
            value
        };
        assert_eq!((gs, decode(&gb)), (rs, decode(&rb)), "{path}");
        assert_eq!(gs, 200, "{path}");
        if path.ends_with("/autocomplete") {
            let list = decode(&gb);
            let hookrec = list
                .as_array()
                .and_then(|l| l.iter().find(|c| c["trigger"] == "hookrec"))
                .expect("the plugin's command is listed");
            assert_eq!(
                hookrec["autocomplete_data"]["SubCommands"][2]["Arguments"][0]["Data"]["FetchURL"],
                "/plugins/mmrs.hookrecorder/suggest/fetch",
                "a relative fetch URL is rooted at the plugin"
            );
            assert!(
                list.as_array()
                    .is_some_and(|l| l.iter().any(|c| c["trigger"] == "hookrecteam")),
                "the team's own plugin command is listed"
            );
        }
    }
    drop(go);

    // A plugin disabled through the Rust host takes its commands with it: the trigger is then
    // nobody's, and Go answers it.
    let (status, body, _) = request_raw(
        client,
        &rust.base,
        reqwest::Method::POST,
        Some(admin),
        &format!("/api/v4/plugins/{PLUGIN_ID}/disable"),
        None,
    )
    .await;
    assert_eq!(status, 200, "disable: {}", String::from_utf8_lossy(&body));
    let body = serde_json::to_vec(&serde_json::json!({
        "channel_id": sides[1].channel,
        "team_id": team,
        "command": "/hookrec ephemeral after the disable",
    }))
    .expect("the body");
    let (status, answer, served_by) = request_raw(
        client,
        &rust.base,
        reqwest::Method::POST,
        Some(&sides[1].own.token),
        "/api/v4/commands/execute",
        Some(&body),
    )
    .await;
    let answer: Json = serde_json::from_slice(&answer).unwrap_or(Json::Null);
    assert_eq!(
        (
            status,
            answer["id"].as_str(),
            served_by.as_deref() == Some("rust")
        ),
        (
            404,
            Some("api.command.execute_command.not_found.app_error"),
            false
        ),
        "a disabled plugin's command is nobody's: {answer}"
    );

    drop(rust);
    for side in &sides {
        common::delete_channel(client, admin, &side.channel).await;
        common::delete_channel(client, admin, &side.closed).await;
    }
}

// ---------------------------------------------------------------------------------------------
// The plugin API tranche: users, statuses, preferences and teams (Phase 6)
// ---------------------------------------------------------------------------------------------

/// The Rust host of the users tranche; see `second_server_ports`. Not a Go offset port: + 73,
/// + 74 and + 80 to + 92 are taken by this file's Go servers.
const USERS_HOST_PORT: u16 = 8158;
/// Its Go server.
const USERS_GO_OFFSET: u16 = 92;
/// Each side's tag: in its users' names, the user and the team its script makes.
const USERS_SIDES: [&str; 2] = ["pusidego", "pusiders"];
/// The picture planted in each host's file store for its own user, so `GetProfileImage` reads a
/// stored file on both (a missing one is Go's generated avatar, which is not reproduced).
const USERS_PICTURE: &[u8] = b"not a png: the plugin users tranche";

/// The plain users the users tour makes, for the cleanup.
static USERS_USERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Cross-server parity for the plugin API's user, status, preference and team methods
/// (`mm_app::plugin_api::users`).
///
/// A `!users-script` post, made by each side's own user in its own channel, runs
/// `examples/recorder/users.rs` inside `MessageWillBePosted`. As in the core tranche, every
/// answer is compared in order, every hook the script's writes fired, and every websocket frame
/// the own user's socket received.
///
/// # What differs between the sides, and how it is taken out
///
/// Each side has its own team, channel and two users, made through main Go before either host
/// starts, and the admin leaves the side team so that nothing in it is shared-stack state. The
/// side's ids and its tag are scrubbed, then the ids the script learned — the user and team it
/// made. What is left is masked, and each mask is named: [`normalise`]'s id and time keys,
/// [`mask_core`]'s `<minted>` ids (invite ids, the made team's default channels, system posts) and
/// session clocks, a `Password` hash as `<hash>`, since each side's users were made with their
/// own salt, and a frame's `last_password_update`, stamped by each side's own user creation.
///
/// # Seven answers are compared as sets
///
/// `GetUsersByIds` (`GetMany` has no `ORDER BY`), `GetTeamsUnreadForUser` (teams in the order
/// the unread query's rows first name them, and it has no `ORDER BY`), `GetTeamMembers`
/// (user-id order, and each side's ids are its own), `GetTeamMembersForUser` and
/// `GetTeamsForUser` (no `ORDER BY`), `GetUserStatusesByIds` (cache hits first in Go) and
/// `GetPreferencesForUser` (no `ORDER BY`) have their lists sorted after the scrub. Every other
/// list keeps its order.
///
/// # One frame is Go's coin toss
///
/// The own user joins the made team itself, so its socket hears its own default-channel joins
/// ([`OwnJoins`]). Whether Go's hears off-topic's `user_added` depends on a random `select` in
/// Go's hub ([D-1032], `App::should_send_event`); that one frame is asserted on its own and
/// left out of Go's list. Everything else about the joins is compared and asserted.
#[tokio::test]
async fn the_plugin_api_user_methods_answer_as_go_answers() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    purge_users_rows().await;
    let outcome = std::panic::AssertUnwindSafe(run_the_users_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    let users = std::mem::take(
        &mut *USERS_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for id in users {
        common::delete_plain_user(&client, &admin, &id).await;
    }
    purge_users_rows().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// What the script made that carries no suite prefix the fixture purge reaches mid-run: the user
/// and the team each side makes, and the team's channels and memberships. Deactivated through Go
/// first would leave the username taken for the next run, so the rows go.
async fn purge_users_rows() {
    let pool = common::fixture_pool().await.expect("the stack database");
    let usernames: Vec<String> = USERS_SIDES
        .iter()
        .map(|side| format!("mmrsplainusrnew{side}"))
        .collect();
    let teams: Vec<String> = USERS_SIDES
        .iter()
        .map(|side| format!("mmrs-parity-plugusers-{side}"))
        .collect();
    for statement in [
        "DELETE FROM preferences WHERE userid IN (SELECT id FROM users WHERE username = ANY($1))",
        "DELETE FROM teammembers WHERE userid IN (SELECT id FROM users WHERE username = ANY($1))",
        "DELETE FROM channelmembers WHERE userid IN (SELECT id FROM users WHERE username = ANY($1))",
        "DELETE FROM status WHERE userid IN (SELECT id FROM users WHERE username = ANY($1))",
        "DELETE FROM users WHERE username = ANY($1)",
    ] {
        sqlx::query(statement)
            .bind(&usernames)
            .execute(&pool)
            .await
            .expect("the script's user goes");
    }
    for statement in [
        "DELETE FROM channelmembers WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM posts WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM publicchannels WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM channels WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM sidebarcategories WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM teammembers WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM teams WHERE name = ANY($1)",
    ] {
        sqlx::query(statement)
            .bind(&teams)
            .execute(&pool)
            .await
            .expect("the script's team goes");
    }
}

/// One side of the users tour.
struct UsersSide {
    tag: &'static str,
    own: common::PlainUser,
    other: common::PlainUser,
    team: String,
    channel: String,
}

impl UsersSide {
    /// What the host passes down to the recorder for this side.
    fn env(&self, reader: &str, shared_team: &str) -> Vec<(&'static str, String)> {
        vec![
            ("HOOK_RECORDER_USERS_READER", reader.to_owned()),
            ("HOOK_RECORDER_USERS_SHARED_TEAM", shared_team.to_owned()),
            ("HOOK_RECORDER_USERS_OWN", self.own.id.clone()),
            ("HOOK_RECORDER_USERS_OTHER", self.other.id.clone()),
            ("HOOK_RECORDER_USERS_TEAM", self.team.clone()),
            ("HOOK_RECORDER_USERS_SIDE", self.tag.to_owned()),
        ]
    }

    /// This side's scrub pairs: what the script's answers taught it first, then the fixture.
    fn pairs(&self, calls: &[Json]) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for (name, pointer, token) in [
            ("CreateUser", "/returns/A/Id", "<new-user>"),
            ("CreateTeam", "/returns/A/Id", "<new-team>"),
        ] {
            if let Some(value) = learned(calls, name, 0, pointer) {
                pairs.push((value, token.to_owned()));
            }
        }
        pairs.extend([
            (self.own.id.clone(), "<own>".to_owned()),
            (self.other.id.clone(), "<other>".to_owned()),
            (self.team.clone(), "<side-team>".to_owned()),
            (self.channel.clone(), "<own-channel>".to_owned()),
            (self.own.token.clone(), "<token>".to_owned()),
            (self.tag.to_owned(), "<side>".to_owned()),
        ]);
        pairs
    }
}

/// The calls whose answer list has no order either server promises; see the test's doc.
const USERS_UNORDERED: [&str; 7] = [
    "GetUsersByIds",
    "GetTeamsUnreadForUser",
    "GetTeamMembers",
    "GetTeamMembersForUser",
    "GetTeamsForUser",
    "GetUserStatusesByIds",
    "GetPreferencesForUser",
];

/// A bcrypt hash, salted per user: `"<hash>"` when set, so that set-or-not is still compared.
fn mask_password(value: &mut Json) {
    match value {
        Json::Array(items) => items.iter_mut().for_each(mask_password),
        Json::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if key == "Password" && entry.as_str().is_some_and(|p| !p.is_empty()) {
                    *entry = Json::String("<hash>".to_owned());
                    continue;
                }
                mask_password(entry);
            }
        }
        _ => {}
    }
}

/// A frame's `last_password_update`, which each side's own user got from its own creation — the
/// JSON name of the `LastPasswordUpdate` that [`normalise`] already masks in the transcript.
fn mask_password_update(value: &mut Json) {
    match value {
        Json::Array(items) => items.iter_mut().for_each(mask_password_update),
        Json::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if key == "last_password_update" && entry.as_i64().is_some_and(|n| n != 0) {
                    *entry = Json::String("<set>".to_owned());
                    continue;
                }
                mask_password_update(entry);
            }
        }
        _ => {}
    }
}

/// Sort the answer lists of [`USERS_UNORDERED`] calls, once everything else is a token.
fn sort_unordered_answers(entries: &mut [Json]) {
    for entry in entries.iter_mut() {
        let Some(calls) = entry.get_mut("calls").and_then(Json::as_array_mut) else {
            continue;
        };
        for call in calls.iter_mut() {
            let unordered = call["call"]
                .as_str()
                .is_some_and(|name| USERS_UNORDERED.contains(&name));
            if !unordered {
                continue;
            }
            if let Some(items) = call.pointer_mut("/returns/A").and_then(Json::as_array_mut) {
                items.sort_by_key(Json::to_string);
            }
        }
    }
}

/// Wait until the transcript holds the script's entry under `hook` and then stops growing.
async fn script_transcript_settles(path: &Path, hook: &str, side: &str) {
    let mut last = usize::MAX;
    let mut quiet_since = None;
    for _ in 0..600 {
        let entries = transcript(path);
        let done = entries.iter().any(|e| e["hook"] == hook);
        if done && entries.len() == last {
            let since = *quiet_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= QUIET * 2 {
                return;
            }
        } else {
            quiet_since = None;
        }
        last = entries.len();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{side}: the {hook} never settled: {:?}",
        names(&transcript(path))
    );
}

async fn run_the_users_tour(client: &reqwest::Client, admin: &str) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-users");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(client, admin, Some(true)).await;

    // Every fixture is made through **main** Go, which hosts no plugins.
    let shared_team = common::create_team(client, admin, "hookusers").await;
    let reader = common::create_plain_user(client, admin, &shared_team, "usrrd").await;
    USERS_USERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(reader.id.clone());
    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let admin_id = me["id"].as_str().expect("an id").to_owned();

    let mut sides = Vec::new();
    for tag in USERS_SIDES {
        let team = common::create_team(client, admin, &format!("hookusers{tag}")).await;
        let own = common::create_plain_user(client, admin, &team, &format!("usrown{tag}")).await;
        let other = common::create_plain_user(client, admin, &team, &format!("usroth{tag}")).await;
        USERS_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([own.id.clone(), other.id.clone()]);
        let channel = common::create_channel(client, admin, &team, &format!("users{tag}")).await;
        common::add_user_to_channel(client, admin, &channel, &own.id).await;
        common::add_user_to_channel(client, admin, &channel, &other.id).await;
        // The admin made the team and the channel, so it is in both; leaving the team takes it
        // out of the channel too, and nothing the script reads is shared-stack state.
        common::remove_user_from_team(client, admin, &team, &admin_id).await;
        sides.push(UsersSide {
            tag,
            own,
            other,
            team,
            channel,
        });
    }

    // Each host's own user has a stored picture in that host's file store.
    for (run, side) in [(&go_run, &sides[0]), (&rs_run, &sides[1])] {
        let dir = run.join("data/users").join(&side.own.id);
        std::fs::create_dir_all(&dir).expect("the picture's directory");
        std::fs::write(dir.join("profile.png"), USERS_PICTURE).expect("the picture");
    }

    let go_env = sides[0].env(&reader.id, &shared_team);
    let go_transcript = go_log.to_string_lossy().into_owned();
    let mut env: Vec<(&str, &str)> = vec![("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())];
    env.extend(go_env.iter().map(|(k, v)| (*k, v.as_str())));
    let go = start_go(&go_run, &env, USERS_GO_OFFSET).await;

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let rust_env = sides[1].env(&reader.id, &shared_team);
    let mut env: Vec<(&str, &str)> = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    env.extend(rust_env.iter().map(|(k, v)| (*k, v.as_str())));
    let rust = SecondServer::start_in(USERS_HOST_PORT, &rs_run, &env)
        .await
        .expect("the Rust host starts");
    wait_until_running(client, admin, &go.base).await;
    wait_until_running(client, admin, &rust.base).await;

    let shared_ids = vec![
        reader.id.clone(),
        shared_team.clone(),
        admin_id.clone(),
        CORE_MISSING.to_owned(),
    ];
    let mut recorded = Vec::new();
    for (side, base, log, host) in [
        (&sides[0], go.base.as_str(), go_log.as_path(), "Go"),
        (&sides[1], rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        let mut probe = common::SocketProbe::connect(base, &side.own.token).await;
        let body = serde_json::to_vec(&serde_json::json!({
            "channel_id": side.channel,
            "message": "!users-script",
        }))
        .expect("the post");
        let (status, answer, served_by) = request_raw(
            client,
            base,
            reqwest::Method::POST,
            Some(&side.own.token),
            "/api/v4/posts",
            Some(&body),
        )
        .await;
        assert_eq!(
            status,
            201,
            "{host}: the trigger: {}",
            String::from_utf8_lossy(&answer)
        );
        if host == "Rust" {
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "the trigger was forwarded"
            );
        }
        script_transcript_settles(log, "UsersScript", host).await;
        core_frames_settle(&mut probe).await;

        let calls = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Json>(line).ok())
            .find(|e| e["hook"] == "UsersScript")
            .and_then(|e| e["calls"].as_array().cloned())
            .unwrap_or_default();
        let pairs = side.pairs(&calls);
        let mut entries = transcript_of(log, &pairs);
        for entry in entries.iter_mut() {
            mask_core(entry, &shared_ids);
            mask_password(entry);
        }
        sort_unordered_answers(&mut entries);
        let joins = own_joins(&probe.raw, &calls, &side.own.id).await;
        let tolerated = joins.assert_goes_as_go_can(host);
        if let Some(log) = std::env::var_os("MMRS_JOIN_ORDER_LOG") {
            use std::io::Write as _;
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .expect("the join-order log");
            writeln!(
                log,
                "{host}: off-topic user_added heard: {}",
                joins.off_added.is_some()
            )
            .expect("the join-order log");
        }
        let mut frames: Vec<Json> = probe
            .raw
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != tolerated)
            .filter_map(|(_, raw)| core_frame(raw, &pairs, &shared_ids))
            .collect();
        frames.iter_mut().for_each(mask_password_update);
        frames.sort_by_key(Json::to_string);
        recorded.push((entries, frames, calls));
    }
    let (go_side, rust_side) = (&recorded[0], &recorded[1]);

    // The answers, in order.
    let script = |entries: &[Json]| -> Vec<Json> {
        entries
            .iter()
            .find(|e| e["hook"] == "UsersScript")
            .and_then(|e| e["calls"].as_array().cloned())
            .expect("the script ran")
    };
    let (go_calls, rust_calls) = (script(&go_side.0), script(&rust_side.0));
    assert!(
        go_calls.iter().all(|c| c.get("error").is_none()),
        "Go implements every method the script calls: {:?}",
        go_calls
            .iter()
            .filter(|c| c.get("error").is_some())
            .collect::<Vec<_>>()
    );
    // Every differing call at once: a tranche this long is otherwise debugged one run per call.
    let differing: Vec<String> = go_calls
        .iter()
        .zip(&rust_calls)
        .enumerate()
        .filter(|(_, (g, r))| g != r)
        .map(|(index, (g, r))| format!("call {index} ({}):\n  go:   {g}\n  rust: {r}", g["call"]))
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));
    assert_eq!(
        go_calls.len(),
        rust_calls.len(),
        "the script ran to the end"
    );

    // The hooks the script's writes fired, in canonical order.
    let hooks = |entries: &[Json]| -> Vec<Json> {
        let rest: Vec<Json> = entries
            .iter()
            .filter(|e| e["hook"] != "UsersScript")
            .cloned()
            .collect();
        in_canonical_order(&rest)
    };
    let (go_hooks, rust_hooks) = (hooks(&go_side.0), hooks(&rust_side.0));
    assert_eq!(names(&go_hooks), names(&rust_hooks), "the hooks that fired");
    let differing: Vec<String> = go_hooks
        .iter()
        .zip(&rust_hooks)
        .enumerate()
        .filter(|(_, (g, r))| g != r)
        .map(|(index, (g, r))| format!("hook {index} ({}):\n  go:   {g}\n  rust: {r}", g["hook"]))
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));

    // Every frame the own user's socket received, in canonical order.
    let event_names = |frames: &[Json]| -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f["event"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(
        event_names(&go_side.1),
        event_names(&rust_side.1),
        "the events the own user received:\n  go:   {:?}\n  rust: {:?}",
        go_side.1,
        rust_side.1
    );
    let differing: Vec<String> = go_side
        .1
        .iter()
        .zip(&rust_side.1)
        .enumerate()
        .filter(|(_, (g, r))| g != r)
        .map(|(index, (g, r))| format!("frame {index} ({}):\n  go:   {g}\n  rust: {r}", g["event"]))
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));

    assert_users_answers_are_gos(&go_calls, &go_hooks);

    drop(rust);
    drop(go);
    for side in &sides {
        common::delete_channel(client, admin, &side.channel).await;
    }
}

/// Which of the own user's socket frames were its own default-channel joins in the team the
/// script made, by index into the raw frames ([D-1032]): each channel's join post and its
/// `user_added` naming the own user.
#[derive(Debug, Default)]
struct OwnJoins {
    town_post: Option<usize>,
    town_added: Option<usize>,
    off_post: Option<usize>,
    off_added: Option<usize>,
}

impl OwnJoins {
    /// What either host must have heard, and the one frame Go may or may not have: returns the
    /// index of Go's off-topic `user_added` when it was heard, for the comparison to leave out.
    ///
    /// Town-square's two are heard, since the join post's decision loaded the cache after
    /// town-square was saved; off-topic's join post never is. Off-topic's `user_added` is
    /// Go's coin toss (`App::should_send_event`): heard in 13 runs of 26 when this was measured.
    /// This server's hub never falls behind, so it gives Go's other answer, always.
    fn assert_goes_as_go_can(&self, host: &str) -> Option<usize> {
        assert!(
            self.town_post.is_some() && self.town_added.is_some(),
            "{host}: the own user hears its town-square join: {self:?}"
        );
        assert!(
            self.off_post.is_none(),
            "{host}: the own user never hears its off-topic join post: {self:?}"
        );
        if host == "Go" {
            return self.off_added;
        }
        assert!(
            self.off_added.is_none(),
            "{host}: off-topic's user_added is decided against the cache town-square's join post \
             loaded, as Go's is whenever its hub keeps up: {self:?}"
        );
        None
    }
}

/// Classify `raw` against the made team's two default channels, read from the database (the
/// team is archived by then, and its channels with it, but the rows stay).
async fn own_joins(raw: &[String], calls: &[Json], own: &str) -> OwnJoins {
    let team = learned(calls, "CreateTeam", 0, "/returns/A/Id").expect("the script made a team");
    let pool = common::fixture_pool().await.expect("the stack database");
    let channels: Vec<(String, String)> =
        sqlx::query_as("SELECT id, name FROM channels WHERE teamid = $1")
            .bind(&team)
            .fetch_all(&pool)
            .await
            .expect("the made team's channels");
    let id_of = |name: &str| {
        channels
            .iter()
            .find(|(_, n)| n == name)
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| panic!("the made team has a {name}: {channels:?}"))
    };
    let (town, off) = (id_of("town-square"), id_of("off-topic"));

    let mut joins = OwnJoins::default();
    for (index, frame) in raw.iter().enumerate() {
        let Ok(frame) = serde_json::from_str::<Json>(frame) else {
            continue;
        };
        let channel = frame["broadcast"]["channel_id"]
            .as_str()
            .unwrap_or_default();
        let slot = match frame["event"].as_str() {
            Some("user_added") if frame["data"]["user_id"] == own => {
                if channel == town {
                    &mut joins.town_added
                } else if channel == off {
                    &mut joins.off_added
                } else {
                    continue;
                }
            }
            Some("posted") => {
                let post: Json = frame["data"]["post"]
                    .as_str()
                    .and_then(|p| serde_json::from_str(p).ok())
                    .unwrap_or_default();
                if post["user_id"] != own {
                    continue;
                }
                match (post["type"].as_str(), channel) {
                    (Some("system_join_team"), c) if c == town => &mut joins.town_post,
                    (Some("system_join_channel"), c) if c == off => &mut joins.off_post,
                    _ => continue,
                }
            }
            _ => continue,
        };
        assert!(slot.is_none(), "one frame of each kind: {frame}");
        *slot = Some(index);
    }
    joins
}

/// What parity alone would not pin, because both hosts could agree on a wrong answer: read off
/// Go's scrubbed answers.
fn assert_users_answers_are_gos(calls: &[Json], hooks: &[Json]) {
    let answer = |name: &str, n: usize| -> Json {
        calls
            .iter()
            .filter(|c| c["call"] == name)
            .nth(n)
            .map(|c| c["returns"].clone())
            .unwrap_or(Json::Null)
    };
    let own_in = |list: &Json| -> Json {
        list["A"]
            .as_array()
            .and_then(|users| {
                users
                    .iter()
                    .find(|u| u["Username"] == "mmrsplainusrown<side>")
            })
            .cloned()
            .unwrap_or(Json::Null)
    };

    // Sanitisation: `GetMany` none, the listing queries the credential scrub, search the admin
    // profile scrub.
    let by_ids = own_in(&answer("GetUsersByIds", 0));
    assert_eq!(by_ids["Password"], "<hash>", "GetUsersByIds is unsanitised");
    let listed = own_in(&answer("GetUsers", 0));
    assert!(
        listed["Password"].is_null() && listed["Email"].is_string(),
        "GetUsers drops the password and keeps the e-mail: {listed}"
    );
    let in_team = own_in(&answer("GetUsersInTeam", 0));
    assert!(
        in_team["Password"].is_null() && in_team["Email"].is_string(),
        "GetUsersInTeam likewise: {in_team}"
    );
    let found = own_in(&answer("SearchUsers", 0));
    assert!(
        found["Password"].is_null() && found["Email"].is_string(),
        "a plugin's search is sanitised as an admin: {found}"
    );
    assert!(
        answer("SearchUsers", 1)["A"].is_null(),
        "the e-mail is not searched"
    );
    assert!(
        answer("SearchUsers", 3)["A"].is_null(),
        "a zero limit is LIMIT 0"
    );
    assert_eq!(
        answer("GetUsersInChannel", 2)["B"]["Id"],
        "plugin.api.get_users_in_channel",
        "an unknown sort is refused"
    );
    let names = |list: &Json| -> Vec<String> {
        list["A"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| u["Username"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(
        names(&answer("GetUsersInChannel", 3)),
        ["mmrsplainusrown<side>", "mmrsplainusroth<side>"],
        "status order puts the online user before the dnd one, against username order"
    );

    // The writes.
    assert_eq!(
        answer("CreateUser", 0)["A"]["Roles"],
        "system_user",
        "a plugin's user gets system_user whatever it asked for"
    );
    assert_eq!(
        answer("UpdateUserRoles", 2)["B"]["StatusCode"],
        400,
        "a missing user is UpdateUserRoles' 400"
    );
    assert_eq!(
        answer("UpdateUserStatus", 4)["B"]["Id"],
        "plugin.api.update_user_status.bad_status",
        "ooo is not a status a plugin can set"
    );
    let timed = answer("SetUserStatusTimedDND", 0)["A"].clone();
    assert_eq!(
        (&timed["Status"], &timed["PrevStatus"], &timed["DNDEndTime"]),
        (
            &Json::from("dnd"),
            &Json::from("online"),
            &Json::from(4_102_444_800_i64)
        ),
        "the timed DND keeps the status to return to and truncates the end to the minute"
    );
    assert_eq!(
        answer("UpdatePreferencesForUser", 1)["A"]["StatusCode"],
        403,
        "another user's preference is refused"
    );
    let preference_hooks = hooks
        .iter()
        .filter(|h| h["hook"] == "PreferencesHaveChanged")
        .count();
    assert!(
        preference_hooks >= 1,
        "a plugin's own preference write fires PreferencesHaveChanged"
    );
    let left: Vec<Json> = answer("GetTeamMembersForUser", 0)["A"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        left.iter()
            .any(|m| m["TeamId"] == "<new-team>" && m["DeleteAt"] == "<set>"),
        "the left team's row is still listed: {left:?}"
    );
    assert!(
        answer("GetTeams", 1)["A"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["Name"] == "mmrs-parity-plugusers-<side>" && t["DeleteAt"] == "<set>"),
        "DeleteTeam archives the team"
    );
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

/// An app error's `Id` is its translation key and survives; a post's beside it does not.
#[test]
fn normalise_keeps_an_app_error_id() {
    let mut value: Json = serde_json::json!({
        "A": { "Id": "abcdefghijklmnopqrstuvwxyz" },
        "B": { "Id": "app.user.missing_account.const", "StatusCode": 404, "Where": "GetUser" },
    });
    normalise(&mut value);
    assert_eq!(value["A"]["Id"], "<id>");
    assert_eq!(value["B"]["Id"], "app.user.missing_account.const");
}

/// The core tranche's last mask: a 26-character id outside the shared set, and a session's two
/// clocks; everything else as it was.
#[test]
fn mask_core_names_what_it_masks() {
    let shared = vec!["sharedsharedsharedshared00".to_owned()];
    let mut value: Json = serde_json::json!({
        "a": "abcdefghijklmnopqrstuvwxyz",
        "b": "sharedsharedsharedshared00",
        "c": "short",
        "d": "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        "e": "id=abcdefghijklmnopqrstuvwxyz, sharedsharedsharedshared00",
        "f": "abcdefghijklmnopqrstuvwxyz0",
        "ExpiresAt": 5, "LastActivityAt": 0,
    });
    mask_core(&mut value, &shared);
    assert_eq!(
        value,
        serde_json::json!({
            "a": "<minted>", "b": "sharedsharedsharedshared00", "c": "short",
            "d": "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            "e": "id=<minted>, sharedsharedsharedshared00",
            "f": "abcdefghijklmnopqrstuvwxyz0",
            "ExpiresAt": "<time>", "LastActivityAt": 0,
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

// ---------------------------------------------------------------------------------------------
// The plugin API tranche: channels, members, sidebar, post lists, reactions and emoji (Phase 6)
// ---------------------------------------------------------------------------------------------

/// The Rust host of the channels tranche; see `second_server_ports`. Chosen clear of the ports
/// the users tranche and its siblings take, since a parallel branch appends tranches too.
const CHANNELS_HOST_PORT: u16 = 8164;
/// Its Go server.
const CHANNELS_GO_OFFSET: u16 = 97;
/// Each side's tag: in its users' names, its team, and the channel and posts its script makes.
const CHANNELS_SIDES: [&str; 2] = ["pchsidego", "pchsiders"];

/// The plain users the channels tour makes, for the cleanup.
static CHANNELS_USERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Cross-server parity for the plugin API's channel, member, sidebar, post-list, reaction and
/// emoji methods (`mm_app::plugin_api::channels`).
///
/// A `!channels-script` post, made by each side's own user in its own channel, runs
/// `examples/recorder/channels.rs` inside `MessageWillBePosted`. As in the users tranche, every
/// answer is compared in order, every hook the script's writes fired, and every websocket frame
/// the own user's socket received.
///
/// # What differs between the sides, and how it is taken out
///
/// Each side has its own team, channel and two users, made through main Go before either host
/// starts, and the admin leaves the side team. Two custom emoji are shared, read by both sides
/// and written by neither. The side's ids and tag are scrubbed, and the ids the script learned
/// first (the DM's name before its users' ids, the made channel, the category, the five posts).
/// What is left is masked, and each mask is named: [`normalise`]'s id and time keys,
/// [`mask_core`]'s `<minted>` ids and session clocks, a `Password` hash as `<hash>` (the leave
/// hook carries the user, salted per side), a frame's member clocks, and `GetPostsSince`'s own
/// time argument, read off each side's post as `<since>`.
///
/// # Four answers are compared as sets
///
/// `GetChannelMembers` (`GetMembers` has no `ORDER BY` without `UpdatedAfter`),
/// `GetChannelMembersByIds` (none), `GetChannelMembersForUser` (channel-id order, and each side's
/// ids are its own) and the `A` of `UpdateChannelSidebarCategories` (the request's order) are
/// sorted after the scrub. Every other list keeps its order.
#[tokio::test]
async fn the_plugin_api_channel_methods_answer_as_go_answers() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    purge_channels_rows().await;
    let outcome = std::panic::AssertUnwindSafe(run_the_channels_tour(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, None).await;
    let users = std::mem::take(
        &mut *CHANNELS_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for id in &users {
        common::delete_plain_user(&client, &admin, id).await;
    }
    purge_channels_rows().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// What the tour leaves that the fixture purge does not reach: each side team's channels (the
/// made one is archived, not removed), their members, posts and sidebar rows; the DMs between
/// the side users; and the two emoji.
async fn purge_channels_rows() {
    let pool = common::fixture_pool().await.expect("the stack database");
    let teams: Vec<String> = CHANNELS_SIDES
        .iter()
        .map(|side| format!("mmrs-parity-hookchans{side}"))
        .collect();
    let usernames: Vec<String> = CHANNELS_SIDES
        .iter()
        .flat_map(|side| {
            [
                common::plain_username(&format!("chown{side}")),
                common::plain_username(&format!("choth{side}")),
            ]
        })
        .collect();
    for statement in [
        "DELETE FROM channelmembers WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM reactions WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM threads WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM posts WHERE channelid IN (SELECT c.id FROM channels c JOIN teams t ON t.id = c.teamid WHERE t.name = ANY($1))",
        "DELETE FROM publicchannels WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM channels WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM sidebarcategories WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM teammembers WHERE teamid IN (SELECT id FROM teams WHERE name = ANY($1))",
        "DELETE FROM teams WHERE name = ANY($1)",
    ] {
        sqlx::query(statement)
            .bind(&teams)
            .execute(&pool)
            .await
            .expect("the side teams go");
    }
    // A DM's name is its two users' ids, `__`-joined.
    for statement in [
        "DELETE FROM channelmembers WHERE channelid IN (SELECT c.id FROM channels c JOIN users u ON c.type = 'D' AND c.name LIKE '%' || u.id || '%' WHERE u.username = ANY($1))",
        "DELETE FROM posts WHERE channelid IN (SELECT c.id FROM channels c JOIN users u ON c.type = 'D' AND c.name LIKE '%' || u.id || '%' WHERE u.username = ANY($1))",
        "DELETE FROM sidebarchannels WHERE userid IN (SELECT id FROM users WHERE username = ANY($1))",
        "DELETE FROM sidebarcategories WHERE userid IN (SELECT id FROM users WHERE username = ANY($1))",
        "DELETE FROM channels WHERE id IN (SELECT c.id FROM channels c JOIN users u ON c.type = 'D' AND c.name LIKE '%' || u.id || '%' WHERE u.username = ANY($1))",
    ] {
        sqlx::query(statement)
            .bind(&usernames)
            .execute(&pool)
            .await
            .expect("the side users' DMs go");
    }
    sqlx::query("DELETE FROM emoji WHERE name LIKE 'mmrsparitychems%'")
        .execute(&pool)
        .await
        .expect("the tour's emoji go");
}

/// One side of the channels tour.
struct ChannelsSide {
    tag: &'static str,
    own: common::PlainUser,
    other: common::PlainUser,
    team: String,
    channel: String,
    town: String,
    off: String,
}

impl ChannelsSide {
    /// What the host passes down to the recorder for this side.
    fn env(&self, emoji: &ChannelsEmoji) -> Vec<(&'static str, String)> {
        vec![
            ("HOOK_RECORDER_CHANNELS_OWN", self.own.id.clone()),
            ("HOOK_RECORDER_CHANNELS_OTHER", self.other.id.clone()),
            ("HOOK_RECORDER_CHANNELS_TEAM", self.team.clone()),
            ("HOOK_RECORDER_CHANNELS_TOWN", self.town.clone()),
            ("HOOK_RECORDER_CHANNELS_OFF", self.off.clone()),
            ("HOOK_RECORDER_CHANNELS_SIDE", self.tag.to_owned()),
            ("HOOK_RECORDER_CHANNELS_EMOJI_A", emoji.a.clone()),
            ("HOOK_RECORDER_CHANNELS_EMOJI_B", emoji.b.clone()),
            ("HOOK_RECORDER_CHANNELS_EMOJI_NAME", emoji.a_name.clone()),
        ]
    }

    /// This side's scrub pairs: what the script's answers taught it first — the DM's name holds
    /// both users' ids, so it goes before they do — then the fixture.
    fn pairs(&self, calls: &[Json]) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for (name, n, pointer, token) in [
            ("GetDirectChannel", 0, "/returns/A/Name", "<dm-name>"),
            ("GetDirectChannel", 0, "/returns/A/Id", "<dm>"),
            ("CreateChannel", 0, "/returns/A/Id", "<made>"),
            (
                "CreateChannelSidebarCategory",
                0,
                "/returns/A/SidebarCategory/Id",
                "<category>",
            ),
            ("CreatePost", 0, "/returns/A/Id", "<p1>"),
            ("CreatePost", 1, "/returns/A/Id", "<p2>"),
            ("CreatePost", 2, "/returns/A/Id", "<p3>"),
            ("CreatePost", 3, "/returns/A/Id", "<p4>"),
            ("CreatePost", 4, "/returns/A/Id", "<reply>"),
        ] {
            if let Some(value) = learned(calls, name, n, pointer) {
                pairs.push((value, token.to_owned()));
            }
        }
        pairs.extend([
            (self.own.id.clone(), "<own>".to_owned()),
            (self.other.id.clone(), "<other>".to_owned()),
            (self.team.clone(), "<side-team>".to_owned()),
            (self.channel.clone(), "<own-channel>".to_owned()),
            (self.town.clone(), "<town>".to_owned()),
            (self.off.clone(), "<off>".to_owned()),
            (self.own.token.clone(), "<token>".to_owned()),
            (self.tag.to_owned(), "<side>".to_owned()),
        ]);
        pairs
    }
}

/// The two shared emoji: `a` is made second and named first, so name order and insertion order
/// disagree.
struct ChannelsEmoji {
    a: String,
    a_name: String,
    b: String,
}

/// The calls whose answer list has no order either server promises; see the test's doc.
const CHANNELS_UNORDERED: [&str; 4] = [
    "GetChannelMembers",
    "GetChannelMembersByIds",
    "GetChannelMembersForUser",
    "UpdateChannelSidebarCategories",
];

/// `GetPostsSince`'s time, read off each side's own post: `<since>` unless it is the fixed
/// future time the script also asks for.
fn mask_since(entries: &mut [Json]) {
    for entry in entries.iter_mut() {
        let Some(calls) = entry.get_mut("calls").and_then(Json::as_array_mut) else {
            continue;
        };
        for call in calls.iter_mut() {
            if call["call"] != "GetPostsSince" {
                continue;
            }
            if let Some(since) = call.pointer_mut("/args/B") {
                if since.as_i64().is_some_and(|t| t != 4_102_444_800_000) {
                    *since = Json::String("<since>".to_owned());
                }
            }
        }
    }
}

/// A frame's `channelMember` clocks, stamped by each side's own write: the JSON names of the
/// `LastUpdateAt`/`LastViewedAt` that [`normalise`] already masks in the transcript.
fn mask_member_clocks(value: &mut Json) {
    match value {
        Json::Array(items) => items.iter_mut().for_each(mask_member_clocks),
        Json::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if matches!(key.as_str(), "last_update_at" | "last_viewed_at")
                    && entry.as_i64().is_some_and(|n| n != 0)
                {
                    *entry = Json::String("<set>".to_owned());
                    continue;
                }
                mask_member_clocks(entry);
            }
        }
        _ => {}
    }
}

/// Sort the answer lists of [`CHANNELS_UNORDERED`] calls, once everything else is a token.
fn sort_channels_unordered(entries: &mut [Json]) {
    for entry in entries.iter_mut() {
        let Some(calls) = entry.get_mut("calls").and_then(Json::as_array_mut) else {
            continue;
        };
        for call in calls.iter_mut() {
            let unordered = call["call"]
                .as_str()
                .is_some_and(|name| CHANNELS_UNORDERED.contains(&name));
            if !unordered {
                continue;
            }
            if let Some(items) = call.pointer_mut("/returns/A").and_then(Json::as_array_mut) {
                items.sort_by_key(Json::to_string);
            }
        }
    }
}

/// A default channel's id on `team`, through main Go.
async fn channel_by_name(client: &reqwest::Client, admin: &str, team: &str, name: &str) -> String {
    let found: Json = client
        .get(format!("{GO}/api/v4/teams/{team}/channels/name/{name}"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the channel");
    found["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the side team has a {name}: {found}"))
        .to_owned()
}

async fn run_the_channels_tour(client: &reqwest::Client, admin: &str) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-hooks-channels");
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run);
    let rust_log = lay_out(&rs_run);

    plant_state(client, admin, Some(true)).await;

    // Every fixture is made through **main** Go, which hosts no plugins.
    let me: Json = client
        .get(format!("{GO}/api/v4/users/me"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the admin");
    let admin_id = me["id"].as_str().expect("an id").to_owned();

    let mut sides = Vec::new();
    for tag in CHANNELS_SIDES {
        let team = common::create_team(client, admin, &format!("hookchans{tag}")).await;
        let own = common::create_plain_user(client, admin, &team, &format!("chown{tag}")).await;
        let other = common::create_plain_user(client, admin, &team, &format!("choth{tag}")).await;
        CHANNELS_USERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([own.id.clone(), other.id.clone()]);
        let channel = common::create_channel(client, admin, &team, &format!("chans{tag}")).await;
        common::add_user_to_channel(client, admin, &channel, &own.id).await;
        common::add_user_to_channel(client, admin, &channel, &other.id).await;
        let town = channel_by_name(client, admin, &team, "town-square").await;
        let off = channel_by_name(client, admin, &team, "off-topic").await;
        // The admin made the team and the channel; leaving the team takes it out of both.
        common::remove_user_from_team(client, admin, &team, &admin_id).await;
        sides.push(ChannelsSide {
            tag,
            own,
            other,
            team,
            channel,
            town,
            off,
        });
    }

    // Two emoji, the second-made named first; their image planted in each host's file store.
    let stamp = common::unique_emoji_name("");
    let b_name = stamp.replace("mmrsparity", "mmrsparitychemsb");
    let a_name = stamp.replace("mmrsparity", "mmrsparitychemsa");
    let b = common::create_custom_emoji(client, admin, &admin_id, &b_name).await;
    let a = common::create_custom_emoji(client, admin, &admin_id, &a_name).await;
    let emoji = ChannelsEmoji { a, a_name, b };
    for run in [&go_run, &rs_run] {
        for id in [&emoji.a, &emoji.b] {
            let dir = run.join("data/emoji").join(id);
            std::fs::create_dir_all(&dir).expect("the emoji's directory");
            std::fs::write(dir.join("image"), common::TINY_PNG).expect("the emoji's image");
        }
    }

    let go_env = sides[0].env(&emoji);
    let go_transcript = go_log.to_string_lossy().into_owned();
    let mut env: Vec<(&str, &str)> = vec![("HOOK_RECORDER_TRANSCRIPT", go_transcript.as_str())];
    env.extend(go_env.iter().map(|(k, v)| (*k, v.as_str())));
    let go = start_go(&go_run, &env, CHANNELS_GO_OFFSET).await;

    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let rust_env = sides[1].env(&emoji);
    let mut env: Vec<(&str, &str)> = vec![
        ("MMRS_PLUGIN_HOST", "rust"),
        ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
        ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
        ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
        ("HOOK_RECORDER_TRANSCRIPT", rust_transcript.as_str()),
    ];
    env.extend(rust_env.iter().map(|(k, v)| (*k, v.as_str())));
    let rust = SecondServer::start_in(CHANNELS_HOST_PORT, &rs_run, &env)
        .await
        .expect("the Rust host starts");
    wait_until_running(client, admin, &go.base).await;
    wait_until_running(client, admin, &rust.base).await;

    let shared_ids = vec![
        emoji.a.clone(),
        emoji.b.clone(),
        admin_id.clone(),
        CORE_MISSING.to_owned(),
    ];
    let mut recorded = Vec::new();
    for (side, base, log, host) in [
        (&sides[0], go.base.as_str(), go_log.as_path(), "Go"),
        (&sides[1], rust.base.as_str(), rust_log.as_path(), "Rust"),
    ] {
        let mut probe = common::SocketProbe::connect(base, &side.own.token).await;
        let body = serde_json::to_vec(&serde_json::json!({
            "channel_id": side.channel,
            "message": "!channels-script",
        }))
        .expect("the post");
        let (status, answer, served_by) = request_raw(
            client,
            base,
            reqwest::Method::POST,
            Some(&side.own.token),
            "/api/v4/posts",
            Some(&body),
        )
        .await;
        assert_eq!(
            status,
            201,
            "{host}: the trigger: {}",
            String::from_utf8_lossy(&answer)
        );
        if host == "Rust" {
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "the trigger was forwarded"
            );
        }
        script_transcript_settles(log, "ChannelsScript", host).await;
        core_frames_settle(&mut probe).await;

        let calls = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Json>(line).ok())
            .find(|e| e["hook"] == "ChannelsScript")
            .and_then(|e| e["calls"].as_array().cloned())
            .unwrap_or_default();
        let pairs = side.pairs(&calls);
        let mut entries = transcript_of(log, &pairs);
        for entry in entries.iter_mut() {
            mask_core(entry, &shared_ids);
            mask_password(entry);
        }
        mask_since(&mut entries);
        sort_channels_unordered(&mut entries);
        let mut frames: Vec<Json> = probe
            .raw
            .iter()
            .filter_map(|raw| core_frame(raw, &pairs, &shared_ids))
            .collect();
        frames.iter_mut().for_each(mask_member_clocks);
        frames.sort_by_key(Json::to_string);
        recorded.push((entries, frames));
    }
    let (go_side, rust_side) = (&recorded[0], &recorded[1]);

    // The answers, in order.
    let script = |entries: &[Json]| -> Vec<Json> {
        entries
            .iter()
            .find(|e| e["hook"] == "ChannelsScript")
            .and_then(|e| e["calls"].as_array().cloned())
            .expect("the script ran")
    };
    let (go_calls, rust_calls) = (script(&go_side.0), script(&rust_side.0));
    assert!(
        go_calls.iter().all(|c| c.get("error").is_none()),
        "Go implements every method the script calls: {:?}",
        go_calls
            .iter()
            .filter(|c| c.get("error").is_some())
            .collect::<Vec<_>>()
    );
    let differing: Vec<String> = go_calls
        .iter()
        .zip(&rust_calls)
        .enumerate()
        .filter(|(_, (g, r))| g != r)
        .map(|(index, (g, r))| format!("call {index} ({}):\n  go:   {g}\n  rust: {r}", g["call"]))
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));
    assert_eq!(
        go_calls.len(),
        rust_calls.len(),
        "the script ran to the end"
    );

    // The hooks the script's writes fired, in canonical order.
    let hooks = |entries: &[Json]| -> Vec<Json> {
        let rest: Vec<Json> = entries
            .iter()
            .filter(|e| e["hook"] != "ChannelsScript")
            .cloned()
            .collect();
        in_canonical_order(&rest)
    };
    let (go_hooks, rust_hooks) = (hooks(&go_side.0), hooks(&rust_side.0));
    assert_eq!(names(&go_hooks), names(&rust_hooks), "the hooks that fired");
    let differing: Vec<String> = go_hooks
        .iter()
        .zip(&rust_hooks)
        .enumerate()
        .filter(|(_, (g, r))| g != r)
        .map(|(index, (g, r))| format!("hook {index} ({}):\n  go:   {g}\n  rust: {r}", g["hook"]))
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));

    // Every frame the own user's socket received, in canonical order.
    let event_names = |frames: &[Json]| -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f["event"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(
        event_names(&go_side.1),
        event_names(&rust_side.1),
        "the events the own user received:\n  go:   {:?}\n  rust: {:?}",
        go_side.1,
        rust_side.1
    );
    let differing: Vec<String> = go_side
        .1
        .iter()
        .zip(&rust_side.1)
        .enumerate()
        .filter(|(_, (g, r))| g != r)
        .map(|(index, (g, r))| format!("frame {index} ({}):\n  go:   {g}\n  rust: {r}", g["event"]))
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));

    assert_channels_answers_are_gos(&go_calls, &go_hooks, &emoji);

    drop(rust);
    drop(go);
    for side in &sides {
        common::delete_channel(client, admin, &side.channel).await;
    }
    for id in [&emoji.a, &emoji.b] {
        common::delete_custom_emoji(client, admin, id).await;
    }
}

/// What parity alone would not pin, because both hosts could agree on a wrong answer: read off
/// Go's scrubbed answers.
fn assert_channels_answers_are_gos(calls: &[Json], hooks: &[Json], emoji: &ChannelsEmoji) {
    let answer = |name: &str, n: usize| -> Json {
        calls
            .iter()
            .filter(|c| c["call"] == name)
            .nth(n)
            .map(|c| c["returns"].clone())
            .unwrap_or(Json::Null)
    };
    let error_id = |name: &str, n: usize, key: &str| -> (Json, Json) {
        let returns = answer(name, n);
        (
            returns[key]["Id"].clone(),
            returns[key]["StatusCode"].clone(),
        )
    };

    // Go's bugs, kept.
    let stats = answer("GetChannelStats", 2)["A"].clone();
    assert_eq!(
        (&stats["MemberCount"], &stats["GuestCount"]),
        (&Json::from(2), &Json::from(2)),
        "GuestCount is a second member count: {stats}"
    );
    assert_eq!(
        answer("GetChannelMembersForUser", 0)["A"],
        answer("GetChannelMembersForUser", 1)["A"],
        "the team id is ignored"
    );
    assert!(
        answer("GetChannelMembersForUser", 0)["A"]
            .as_array()
            .is_some_and(|m| m.len() == 3),
        "the own user's three channels"
    );

    // The refusals the plugin path reaches and REST does not.
    assert_eq!(
        error_id("GetPostsForChannel", 2, "B"),
        (Json::from("app.post.get_posts.app_error"), Json::from(400)),
        "more than 1000 per page"
    );
    for n in [2, 3] {
        assert_eq!(
            error_id("GetPostsAfter", n, "B"),
            (
                Json::from("app.post.get_posts_around.get.app_error"),
                Json::from(400)
            ),
            "a negative page or size after"
        );
        assert_eq!(
            error_id("GetPostsBefore", n, "B"),
            (
                Json::from("app.post.get_posts_around.get.app_error"),
                Json::from(400)
            ),
            "a negative page or size before"
        );
    }
    let patch =
        |n: usize| -> Json { answer("PatchChannelMembersNotifications", n)["A"]["Id"].clone() };
    assert_eq!(
        [patch(0), patch(1), patch(2), patch(3)],
        [
            Json::from("app.channel.patch_channel_members_notify_props.too_many"),
            Json::from("app.channel.patch_channel_members_notify_props.app_error"),
            Json::from("model.channel_member.is_valid.notify_level.app_error"),
            Json::from("model.channel_member.is_valid.unread_level.app_error"),
        ],
        "every refusal before the patch's write"
    );
    assert_eq!(
        [patch(4), patch(5)],
        [Json::Null, Json::Null],
        "a patch, and one naming a non-member, both succeed"
    );
    let notify = answer("GetChannelMember", 0)["A"]["NotifyProps"]["$map"].clone();
    assert_eq!(
        (&notify["desktop"], &notify["push"]),
        (&Json::from("none"), &Json::from("mention")),
        "the second patch named a non-member and was rolled back: {notify}"
    );

    // The leaves.
    let leave = |n: usize| -> Json { answer("DeleteChannelMember", n)["A"]["Id"].clone() };
    assert_eq!(
        leave(0),
        Json::Null,
        "the other user leaves the made channel"
    );
    assert_eq!(
        leave(4),
        Json::from("api.channel.remove.default.app_error"),
        "town-square cannot be left"
    );
    assert_eq!(leave(5), Json::Null, "off-topic can");
    assert_eq!(
        leave(6),
        Json::from("api.channel.leave.direct.app_error"),
        "a DM cannot be left"
    );
    let left = hooks
        .iter()
        .filter(|h| h["hook"] == "UserHasLeftChannel")
        .count();
    assert_eq!(left, 2, "two leaves fire UserHasLeftChannel");
    let leave_posts = hooks
        .iter()
        .filter(|h| {
            h["hook"] == "MessageHasBeenPosted" && h.to_string().contains("system_leave_channel")
        })
        .count();
    assert_eq!(leave_posts, 2, "each leave posts its message");

    // Archiving.
    assert_eq!(
        answer("DeleteChannel", 1)["A"]["Id"],
        "api.channel.delete_channel.deleted.app_error"
    );
    assert_eq!(
        answer("DeleteChannel", 3)["A"]["Id"],
        "api.channel.delete_channel.cannot.app_error"
    );
    let listed = |n: usize| -> bool {
        answer("GetChannelsForTeamForUser", n)["A"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["Name"] == "plugchan-<side>")
    };
    assert!(
        listed(2) && !listed(3),
        "the archived channel is listed only when asked"
    );

    // Posts and search.
    let order = |name: &str, n: usize| -> Json { answer(name, n)["A"]["Order"].clone() };
    assert_eq!(
        order("GetPostsForChannel", 0),
        serde_json::json!(["<reply>", "<p4>", "<p3>"])
    );
    assert_eq!(
        order("GetPostsAfter", 0),
        serde_json::json!(["<p3>", "<p2>"])
    );
    assert_eq!(
        order("GetPostsBefore", 0),
        serde_json::json!(["<p3>", "<p2>"])
    );
    assert_eq!(order("GetPostsBefore", 1), serde_json::json!(["<p3>"]));
    assert_eq!(
        answer("SearchPostsInTeamForUser", 0)["A"]["PostList"]["Order"],
        serde_json::json!(["<p4>"]),
        "the search finds the one post with its word"
    );
    let reactions = answer("GetReactions", 0)["A"].clone();
    assert_eq!(
        reactions
            .as_array()
            .map(|r| r.iter().map(|x| x["EmojiName"].clone()).collect::<Vec<_>>()),
        Some(vec![Json::from("smile"), Json::from("thumbsup")]),
        "reactions in the order they were made"
    );

    // Emoji.
    let names_listed: Vec<Json> = answer("GetEmojiList", 0)["A"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| e["Name"].clone())
        .collect();
    assert_eq!(
        names_listed.first(),
        Some(&Json::from(emoji.a_name.as_str())),
        "name order puts the second-made emoji first: {names_listed:?}"
    );
    assert_eq!(names_listed.len(), 2, "both emoji listed");
    assert_eq!(answer("GetEmojiImage", 0)["B"], "png");
    assert_eq!(
        answer("GetEmojiByName", 2)["B"]["StatusCode"],
        404,
        "a system emoji is not a custom one"
    );
}
