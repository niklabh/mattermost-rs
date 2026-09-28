//! Cross-server parity for the **plugin database driver** (db_rpc.go and app/plugin_db_driver.go;
//! `mm_app::plugin_driver` over the `gopq` crate).
//!
//! ```sh
//! scripts/parity.sh --test parity plugin_driver
//! ```
//!
//! One plugin binary — `mm-plugin`'s `examples/driver_script` — runs under each host. On
//! activation it drives the host's `Driver` through a fixed script (every one of the twenty
//! methods; NULLs and every column type lib/pq decodes; binary and text result formats; a
//! `*pq.Error` with its position; a rolled-back, a read-only and a failed transaction; several
//! result sets; rows closed part-way; a query while rows are open; a connection lib/pq marks bad
//! and `database/sql` then closes; the pool at its `MaxOpenConns`, and a session reused from it;
//! and last, a `timestamptz` gob cannot carry) and writes each
//! reply as gob carried it. The two transcripts are compared line for line.
//!
//! The hosts run **one after the other**, each against a freshly planted `mmrs_plugin_driver`,
//! because the script writes to the table. The Go host is its own server (Go's port + 59, a
//! run directory under this suite's scratch directory, the shared database and configuration),
//! the Rust host a second mm-api on :8123.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value as Json;

use crate::common;

use super::plugin_hooks::{repo, start_go};
use common::{GO, SecondServer, client, go_minted_token, stack_enabled};

/// The Rust host; see `second_server_ports`.
pub(super) const HOST_PORT: u16 = 8123;
/// Its Go server sits at Go's port plus this, :8124 on stack 0. Every port a suite starts must
/// stay under Go's port + 100, which is where the next stack's servers begin.
pub(super) const GO_OFFSET: u16 = 59;
const PLUGIN_ID: &str = "mmrs.driverscript";
/// The pool both hosts are started with, which the script's last steps exhaust: four
/// connections, and two seconds to wait for a fifth.
const POOL: [(&str, &str); 2] = [
    ("MM_SQLSETTINGS_MAXOPENCONNS", "4"),
    ("MM_SQLSETTINGS_QUERYTIMEOUT", "2"),
];
/// The table the script reads and writes.
const TABLE: &str = "mmrs_plugin_driver";

/// `examples/driver_script`, built here for the reason `plugin_hooks::hook_recorder` gives.
fn driver_script() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "mm-plugin", "--example", "driver_script"])
                .current_dir(repo())
                .status()
                .expect("cargo runs");
            assert!(status.success(), "building examples/driver_script failed");
            let exe = std::env::current_exe().expect("the test binary");
            let profile = exe
                .parent()
                .and_then(|deps| deps.parent())
                .expect("target/<profile>");
            profile.join("examples").join("driver_script")
        })
        .clone()
}

/// The bundle as a tar.gz, which each host's `syncPlugins` installs from its file store.
fn bundle(scratch: &Path) -> PathBuf {
    let stage = scratch.join("bundle").join(PLUGIN_ID);
    std::fs::create_dir_all(&stage).expect("the staging directory");
    std::fs::write(
        stage.join("plugin.json"),
        format!(
            r#"{{"id": "{PLUGIN_ID}", "name": "Driver Script", "version": "0.1.0", "server": {{"executable": "plugin"}}}}"#
        ),
    )
    .expect("the manifest");
    std::fs::copy(driver_script(), stage.join("plugin")).expect("the executable");
    let tarball = scratch.join(format!("{PLUGIN_ID}.tar.gz"));
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "tar -c -C {} {PLUGIN_ID} | gzip -1 > {}",
            scratch.join("bundle").display(),
            tarball.display()
        ))
        .status()
        .expect("tar runs");
    assert!(status.success(), "packing the bundle failed");
    tarball
}

/// A run directory with the bundle in its file store; the transcript's path.
fn lay_out(run: &Path, tarball: &Path) -> PathBuf {
    let _ = std::fs::remove_dir_all(run);
    for dir in ["data/plugins", "plugins", "client", "logs"] {
        std::fs::create_dir_all(run.join(dir)).expect("the run directory");
    }
    let _ = std::os::unix::fs::symlink(
        repo().join("reference/mattermost/server/i18n"),
        run.join("i18n"),
    );
    std::fs::copy(
        tarball,
        run.join("data/plugins").join(format!("{PLUGIN_ID}.tar.gz")),
    )
    .expect("the bundle reaches the file store");
    run.join("driver.jsonl")
}

/// Drop and plant the table: every type the script reads, NULLs in each nullable column, an
/// empty (not NULL) `bytea`, and floats whose text form is not their shortest one.
async fn plant_table() {
    let pool = common::fixture_pool().await.expect("the stack database");
    sqlx::query(&format!("DROP TABLE IF EXISTS {TABLE}"))
        .execute(&pool)
        .await
        .expect("drop");
    sqlx::query(&format!(
        "CREATE TABLE {TABLE} (id int4 PRIMARY KEY, name text NOT NULL, amount numeric(10,2), \
         data bytea, flag bool, big int8, ratio float8, note varchar(20), small int2, \
         created timestamptz)"
    ))
    .execute(&pool)
    .await
    .expect("create");
    sqlx::query(&format!(
        "INSERT INTO {TABLE} VALUES \
         (1, 'alpha', 12.50, '\\x0001ff', true, 9007199254740993, 0.1, NULL, 7, '2026-01-02 03:04:05.678+00'), \
         (2, 'beta', NULL, NULL, false, -1, -2.5e-10, 'n2', NULL, NULL), \
         (3, 'gamma', 0, '\\x', NULL, 0, 1e300, '', -32768, '1999-12-31 23:59:59+00')"
    ))
    .execute(&pool)
    .await
    .expect("plant");
}

async fn drop_table() {
    if let Some(pool) = common::fixture_pool().await {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {TABLE}"))
            .execute(&pool)
            .await;
    }
}

/// Set (or, with `None`, remove) this suite's id in the shared `PluginStates`, through main Go.
async fn plant_state(client: &reqwest::Client, admin: &str, enable: bool) {
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
        if enable {
            map.insert(PLUGIN_ID.to_owned(), serde_json::json!({ "Enable": true }));
        } else {
            map.remove(PLUGIN_ID);
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

fn transcript(path: &Path) -> Vec<Json> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The transcript once its `done` line is written.
async fn finished(path: &Path, side: &str) -> Vec<Json> {
    for _ in 0..600 {
        let lines = transcript(path);
        if lines.last().is_some_and(|l| l["step"] == "done") {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{side}: the script never finished; {} lines at {}",
        transcript(path).len(),
        path.display()
    );
}

/// End every plugin process started from under `scratch`; a killed host never stops its own.
fn kill_plugins(scratch: &Path) {
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(format!("{}/", scratch.display()))
        .status();
}

#[tokio::test]
async fn the_driver_answers_as_go_answers() {
    use futures_util::FutureExt as _;

    if !stack_enabled() {
        return;
    }
    let _states = common::PLUGIN_STATES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let outcome = std::panic::AssertUnwindSafe(run_the_script(&client, &admin))
        .catch_unwind()
        .await;
    plant_state(&client, &admin, false).await;
    drop_table().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn run_the_script(client: &reqwest::Client, admin: &str) {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugin-driver");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("the scratch directory");
    let tarball = bundle(&scratch);
    let (go_run, rs_run) = (scratch.join("go"), scratch.join("rust"));
    let go_log = lay_out(&go_run, &tarball);
    let rust_log = lay_out(&rs_run, &tarball);
    plant_state(client, admin, true).await;
    let dsn = format!(
        "{}?sslmode=disable&connect_timeout=10",
        std::env::var("DATABASE_URL").expect("parity.sh sets DATABASE_URL")
    );

    plant_table().await;
    let go_transcript = go_log.to_string_lossy().into_owned();
    let go = start_go(
        &go_run,
        &[
            ("DRIVER_SCRIPT_TRANSCRIPT", go_transcript.as_str()),
            POOL[0],
            POOL[1],
        ],
        GO_OFFSET,
    )
    .await;
    let go_lines = finished(&go_log, "Go").await;
    drop(go);
    kill_plugins(&go_run);

    plant_table().await;
    let s = |p: &str| rs_run.join(p).to_string_lossy().into_owned();
    let (dir, client_dir, data) = (s("plugins"), s("client"), format!("{}/", s("data")));
    let rust_transcript = rust_log.to_string_lossy().into_owned();
    let rust = SecondServer::start_in(
        HOST_PORT,
        &rs_run,
        &[
            ("MMRS_PLUGIN_HOST", "rust"),
            ("MM_PLUGINSETTINGS_DIRECTORY", dir.as_str()),
            ("MM_PLUGINSETTINGS_CLIENTDIRECTORY", client_dir.as_str()),
            ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
            ("MM_SQLSETTINGS_DATASOURCE", dsn.as_str()),
            POOL[0],
            POOL[1],
            ("DRIVER_SCRIPT_TRANSCRIPT", rust_transcript.as_str()),
        ],
    )
    .await
    .expect("the Rust host starts");
    let rust_lines = finished(&rust_log, "Rust").await;
    drop(rust);
    kill_plugins(&rs_run);

    for (index, (g, r)) in go_lines.iter().zip(&rust_lines).enumerate() {
        assert_eq!(g, r, "line {index} differs");
    }
    assert_eq!(go_lines.len(), rust_lines.len(), "the transcripts' lengths");
    // The script really ran: a sample of what must be there, so that two empty or two
    // identically broken runs cannot pass.
    let step = |label: &str| {
        go_lines
            .iter()
            .find(|l| l["step"] == label)
            .unwrap_or_else(|| panic!("no step {label:?}"))
            .clone()
    };
    assert!(go_lines.len() > 150, "{} lines", go_lines.len());
    assert!(step("Conn conn1")["reply"]["A"] == "conn1");
    assert!(
        step("RowsNext rows13 a timestamptz")
            .get("rpc_error")
            .is_some(),
        "the time value crossed"
    );
}
