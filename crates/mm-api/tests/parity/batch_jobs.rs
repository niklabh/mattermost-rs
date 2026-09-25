//! Cross-server parity for the batch workers: `delete_empty_drafts_migration`,
//! `delete_orphan_drafts_migration`, `delete_dms_preferences_migration` and `export_users_to_csv`
//! — Go's `jobs.BatchWorker` shape ([D-804]).
//!
//! ```sh
//! scripts/parity.sh --test parity batch_jobs
//! ```
//!
//! # Who runs which job
//!
//! Every Go process on the stack polls the shared `Jobs` table and claims a `pending` row of any
//! type it registers within a second or so. So each case plants the **same** state twice: once
//! with a job of the real type, which a Go worker claims and runs; and once with a job of a type
//! no Go server knows ([`PROBE`]), which **this** server's worker — the same `BatchWorker`,
//! re-keyed with `with_job_type` — claims through `poll_and_notify` and runs, in-process, against
//! the same database and file directory. Nothing a worker does reads the type.
//!
//! Each run is watched by polling the row every 50 ms, so the comparison is of what a client
//! polling `GET /jobs/{id}` would see: the statuses in order, the progress and the key set of
//! `Data` while it is `in_progress`, and the finished row — plus the rows, files and posts the job
//! was for.
//!
//! # Cancellation
//!
//! None of the four types can be cancelled through the API — `SessionHasPermissionToCreateJob`
//! names none of them, so both servers answer `POST /jobs/{id}/cancel` with the 400
//! `api.job.unable_to_create_job.incorrect_job_type`, asserted here. The only way a row reaches
//! `cancel_requested` is a write outside the API, which is what the cancellation case does; and a
//! batch worker has no cancellation watcher, so the job runs to `success` regardless — at the
//! progress and data it had when the request landed.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::common;

use common::{
    DRAFT_ROWS, GO, RUST, client, create_channel_typed, create_plain_user, create_team,
    fixture_pool, go_minted_token, logged_in_user_id, stack_enabled,
};
use mm_app::job_runtime::{self, BatchWorker, Workers};

/// The job type this server's worker runs under. Not registered by any Go worker.
const PROBE: &str = "mmrs_batch_parity";

/// Far past any real `CreateAt`, so the planted drafts are the tail of the `(CreateAt, UserId)`
/// order and the finished cursor names one of them on both runs.
const FUTURE: i64 = 9_000_000_000_000;

/// An `App` built the way the stack's mm-api builds its own — the configuration document from the
/// shared database — writing files where the stack's Go server does.
async fn rust_app() -> mm_app::App {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set on the stack");
    let store = mm_store::SqlStore::connect(&url, 4)
        .await
        .expect("connects to the shared Postgres");
    let mut config = mm_app::config::Config::load(store.config())
        .await
        .expect("the configuration document loads");
    config.file_directory = common::stack_data_dir();
    mm_app::App::with_config(store, config)
}

async fn plant_job(pool: &sqlx::PgPool, id: &str, job_type: &str, data: &serde_json::Value) {
    sqlx::query(
        "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status,
                           progress, data)
         VALUES ($1, $2, 0, $3, 0, 0, 'pending', 0, $4)",
    )
    .bind(id)
    .bind(job_type)
    .bind(now_millis())
    .bind(data)
    .execute(pool)
    .await
    .expect("the job row is written");
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// One observation of the row.
type Snapshot = (String, i64, serde_json::Value);

async fn job_row(pool: &sqlx::PgPool, id: &str) -> Snapshot {
    let (status, progress, data): (String, i64, Option<serde_json::Value>) =
        sqlx::query_as("SELECT status, progress, data FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("the job row");
    (status, progress, data.unwrap_or(serde_json::Value::Null))
}

/// Poll the row until it is terminal, keeping every change. `cancel_at_first_progress` writes
/// `cancel_requested` the first time the row is seen `in_progress`.
async fn watch(pool: &sqlx::PgPool, id: &str, cancel_at_first_progress: bool) -> Vec<Snapshot> {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut seen: Vec<Snapshot> = Vec::new();
    let mut cancelled = false;
    loop {
        let row = job_row(pool, id).await;
        if seen.last() != Some(&row) {
            seen.push(row.clone());
        }
        if cancel_at_first_progress && !cancelled && row.0 == "in_progress" {
            sqlx::query("UPDATE jobs SET status = 'cancel_requested' WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await
                .expect("the cancellation is written");
            cancelled = true;
        }
        if matches!(row.0.as_str(), "success" | "error" | "canceled" | "warning") {
            return seen;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job {id} did not finish: {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What a run looked like from outside, with `name` applied to every string in the data.
#[derive(Debug, PartialEq)]
struct RunSummary {
    /// Distinct statuses in order, `pending` dropped — whether the first poll saw it is a race.
    statuses: Vec<String>,
    /// Every progress value seen while `in_progress`, except the 100 `setJobSuccess` writes an
    /// instant before `success` — whether a poll lands between those two writes is timing.
    progress_in_flight: BTreeSet<i64>,
    /// Every key set `Data` had while `in_progress` (`null` is the empty set).
    data_keys_in_flight: BTreeSet<Vec<String>>,
    final_status: String,
    final_progress: i64,
    final_data: serde_json::Value,
}

fn summarise(seen: &[Snapshot], name: &dyn Fn(&str) -> String) -> RunSummary {
    let mut statuses: Vec<String> = Vec::new();
    for (status, _, _) in seen {
        if status != "pending" && statuses.last() != Some(status) {
            statuses.push(status.clone());
        }
    }
    let in_flight = seen.iter().filter(|(status, _, _)| status == "in_progress");
    let keys = |data: &serde_json::Value| -> Vec<String> {
        data.as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    };
    let (final_status, final_progress, final_data) = seen.last().cloned().expect("a row");
    let final_data = match final_data {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let v = v.as_str().map_or(v.clone(), |s| serde_json::json!(name(s)));
                    (k, v)
                })
                .collect(),
        ),
        other => other,
    };
    RunSummary {
        statuses,
        progress_in_flight: in_flight
            .clone()
            .map(|(_, p, _)| *p)
            .filter(|p| *p != 100)
            .collect(),
        data_keys_in_flight: in_flight.map(|(_, _, d)| keys(d)).collect(),
        final_status,
        final_progress,
        final_data,
    }
}

/// Run `id` (planted as the real type) on Go.
async fn go_run(
    pool: &sqlx::PgPool,
    job_type: &str,
    data: &serde_json::Value,
    cancel: bool,
) -> (String, Vec<Snapshot>) {
    let id = mm_model::utils::new_id();
    plant_job(pool, &id, job_type, data).await;
    let seen = watch(pool, &id, cancel).await;
    (id, seen)
}

/// Run the same job on this server's worker, under [`PROBE`].
async fn rust_run(
    pool: &sqlx::PgPool,
    app: &mm_app::App,
    worker: BatchWorker,
    data: &serde_json::Value,
    cancel: bool,
) -> (String, Vec<Snapshot>) {
    let id = mm_model::utils::new_id();
    plant_job(pool, &id, PROBE, data).await;
    let mut workers = Workers::new();
    workers.add_batch(worker.with_job_type(PROBE));
    assert_eq!(
        app.poll_and_notify(&workers).await,
        1,
        "the watcher hands this server's worker the one probe job"
    );
    let seen = watch(pool, &id, cancel).await;
    (id, seen)
}

/// Remove the jobs one test planted — by id, never by type: the tests of this module run at once.
async fn drop_jobs(pool: &sqlx::PgPool, ids: &[String]) {
    sqlx::query("DELETE FROM jobs WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .expect("the test's jobs are removed");
}

// ---------------------------------------------------------------------------------------------
// The two draft migrations
// ---------------------------------------------------------------------------------------------

/// The six kinds of draft the two migrations tell apart, as `(label, message, root)`.
const DRAFT_KINDS: [&str; 6] = [
    "empty_channel",
    "channel",
    "empty_live_thread",
    "live_thread",
    "deleted_thread",
    "missing_thread",
];

/// One user's six drafts, `CreateAt` `FUTURE + 1..=6` in [`DRAFT_KINDS`] order: a live root and a
/// deleted one are real posts, the missing root is an id no post has.
async fn plant_drafts(
    http: &reqwest::Client,
    pool: &sqlx::PgPool,
    admin: &str,
    team: &str,
    tag: &str,
) -> String {
    let user = create_plain_user(http, admin, team, tag).await;
    let channel = create_channel_typed(http, admin, team, tag, "O").await;
    common::add_user_to_channel(http, admin, &channel, &user.id).await;
    let live = common::post_message(http, &user.token, &channel, "live root", None).await;
    let deleted = common::post_message(http, &user.token, &channel, "deleted root", None).await;
    common::delete_post(http, &user.token, &deleted).await;
    let missing = mm_model::utils::new_id();
    let rows = [
        ("", ""),
        ("text", ""),
        ("", live.as_str()),
        ("text", live.as_str()),
        ("text", deleted.as_str()),
        ("text", missing.as_str()),
    ];
    for (k, (message, root)) in rows.iter().enumerate() {
        sqlx::query(
            "INSERT INTO drafts (createat, updateat, deleteat, userid, channelid, rootid, message,
                                 props, fileids, priority, type)
             VALUES ($1, $1, 0, $2, $3, $4, $5, '{}', '[]', NULL, '')",
        )
        .bind(FUTURE + 1 + i64::try_from(k).expect("small"))
        .bind(&user.id)
        // A channel of its own per draft: the key is (user, channel, root) and two kinds share
        // a root.
        .bind(mm_model::utils::new_id())
        .bind(*root)
        .bind(*message)
        .execute(pool)
        .await
        .expect("the draft is planted");
    }
    user.id
}

async fn remaining_kinds(pool: &sqlx::PgPool, user_id: &str) -> Vec<&'static str> {
    let at: Vec<i64> =
        sqlx::query_scalar("SELECT createat FROM drafts WHERE userid = $1 ORDER BY createat")
            .bind(user_id)
            .fetch_all(pool)
            .await
            .expect("the drafts");
    at.into_iter()
        .filter_map(|c| {
            usize::try_from(c - FUTURE - 1)
                .ok()
                .and_then(|k| DRAFT_KINDS.get(k).copied())
        })
        .collect()
}

async fn drop_drafts(pool: &sqlx::PgPool, user_id: &str) {
    sqlx::query("DELETE FROM drafts WHERE userid = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("the drafts are cleared");
}

/// Both draft migrations, one after the other, on each server: the empty one removes the two
/// empty drafts; the orphan one then removes every draft whose root is deleted, missing — or
/// empty, which is every channel draft — leaving only the reply under a live root. Each ends
/// `success` at 100 with the cursor on the last planted draft, after `in_progress` rows at 0
/// carrying `create_at` and `user_id`.
#[tokio::test]
async fn the_draft_migrations_delete_what_go_deletes_and_report_alike() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let _drafts = DRAFT_ROWS.write().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "batchdraft").await;
    let app = rust_app().await;

    let mut sides = Vec::new();
    let mut jobs = Vec::new();
    for side in ["go", "rs"] {
        let user = plant_drafts(&http, &pool, &admin, &team, &format!("batchdr{side}")).await;
        let name = |s: &str| s.replace(user.as_str(), "<user>");
        let mut steps = Vec::new();
        for (job_type, worker) in [
            (
                "delete_empty_drafts_migration",
                job_runtime::delete_empty_drafts_migration_worker(),
            ),
            (
                "delete_orphan_drafts_migration",
                job_runtime::delete_orphan_drafts_migration_worker(),
            ),
        ] {
            let (id, seen) = if side == "go" {
                go_run(&pool, job_type, &serde_json::json!({}), false).await
            } else {
                rust_run(&pool, &app, worker, &serde_json::json!({}), false).await
            };
            jobs.push(id);
            steps.push((summarise(&seen, &name), remaining_kinds(&pool, &user).await));
        }
        drop_drafts(&pool, &user).await;
        sides.push(steps);
    }
    drop_jobs(&pool, &jobs).await;

    let rust = sides.pop().expect("two sides");
    let go = sides.pop().expect("two sides");
    let cursor = serde_json::json!({ "create_at": (FUTURE + 6).to_string(), "user_id": "<user>" });
    for (summary, _) in &go {
        assert_eq!(summary.final_status, "success", "{summary:?}");
        assert_eq!(summary.final_progress, 100, "{summary:?}");
        assert_eq!(summary.final_data, cursor, "{summary:?}");
        assert_eq!(summary.statuses, ["in_progress", "success"], "{summary:?}");
    }
    assert_eq!(
        go[0].1,
        ["channel", "live_thread", "deleted_thread", "missing_thread"],
        "the empty migration"
    );
    assert_eq!(go[1].1, ["live_thread"], "the orphan migration");
    assert_eq!(rust, go);
}

// ---------------------------------------------------------------------------------------------
// The DM-limit preference migration, and a cancellation mid-run
// ---------------------------------------------------------------------------------------------

/// Plant one `limit_visible_dms_gms` preference per value, each on its own made-up user id.
async fn plant_dm_limits(pool: &sqlx::PgPool, prefix: &str, values: &[String]) {
    for (i, value) in values.iter().enumerate() {
        sqlx::query(
            "INSERT INTO preferences (userid, category, name, value)
             VALUES ($1, 'sidebar_settings', 'limit_visible_dms_gms', $2)",
        )
        .bind(format!("{prefix}{i:0>width$}", width = 26 - prefix.len()))
        .bind(value)
        .execute(pool)
        .await
        .expect("the preference is planted");
    }
}

async fn remaining_dm_limits(pool: &sqlx::PgPool, prefix: &str) -> Vec<String> {
    let mut values: Vec<String> = sqlx::query_scalar(
        "SELECT value FROM preferences
          WHERE userid LIKE $1 AND category = 'sidebar_settings'
            AND name = 'limit_visible_dms_gms'",
    )
    .bind(format!("{prefix}%"))
    .fetch_all(pool)
    .await
    .expect("the preferences");
    values.sort();
    values
}

async fn drop_dm_limits(pool: &sqlx::PgPool, prefix: &str) {
    sqlx::query("DELETE FROM preferences WHERE userid LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .expect("the preferences are cleared");
}

/// The values the comparison is textual about: out of range, not a number, negative, zero-padded
/// past 40, and the in-range ones — `007` among them — that stay. Plus enough invalid rows to take
/// more than one 100-row batch.
fn dm_limit_values(extra_invalid: usize) -> Vec<String> {
    let mut values: Vec<String> = [
        "0", "41", "abc", "-5", "100", "0000041", "1", "40", "15", "007",
    ]
    .iter()
    .map(|v| (*v).to_owned())
    .collect();
    values.extend(std::iter::repeat_n("999".to_owned(), extra_invalid));
    values
}

/// The out-of-range preferences go, in batches of 100 until a batch deletes none, and the valid
/// ones — `007` included — stay. `Data` is `null` between batches (the migration's next data is
/// Go's nil map) and at the end.
#[tokio::test]
async fn the_dm_limit_migration_deletes_what_go_deletes_and_reports_alike() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let app = rust_app().await;
    let mut sides = Vec::new();
    let mut jobs = Vec::new();
    for side in ["go", "rs"] {
        let prefix = format!("mmrsdmlim{side}");
        drop_dm_limits(&pool, &prefix).await;
        plant_dm_limits(&pool, &prefix, &dm_limit_values(120)).await;
        let (job_id, seen) = if side == "go" {
            go_run(
                &pool,
                "delete_dms_preferences_migration",
                &serde_json::json!({}),
                false,
            )
            .await
        } else {
            rust_run(
                &pool,
                &app,
                job_runtime::delete_dms_preferences_migration_worker(),
                &serde_json::json!({}),
                false,
            )
            .await
        };
        jobs.push(job_id);
        let remaining = remaining_dm_limits(&pool, &prefix).await;
        drop_dm_limits(&pool, &prefix).await;
        sides.push((summarise(&seen, &|s: &str| s.to_owned()), remaining));
    }
    drop_jobs(&pool, &jobs).await;

    let rust = sides.pop().expect("two sides");
    let go = sides.pop().expect("two sides");
    assert_eq!(go.1, ["007", "1", "15", "40"], "{go:?}");
    assert_eq!(go.0.final_status, "success");
    assert_eq!(go.0.final_progress, 100);
    assert_eq!(go.0.final_data, serde_json::Value::Null);
    assert!(
        go.0.data_keys_in_flight.contains(&Vec::<String>::new()),
        "null between batches: {go:?}"
    );
    assert_eq!(rust, go);
}

/// A cancellation request while the job runs: the API refuses it on both servers, and a
/// `cancel_requested` written under the job changes nothing it deletes — the worker has no
/// watcher — but drops every later progress write, so the job ends `success` at progress 0 with
/// the data it was claimed with.
#[tokio::test]
async fn a_cancel_request_mid_run_ends_in_success_at_the_progress_it_had() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let app = rust_app().await;
    let planted = serde_json::json!({ "planted": "yes" });

    // The API cannot cancel any of the four types: a planted `in_progress` row of the real type
    // is refused by both servers before its status is read.
    let id = mm_model::utils::new_id();
    sqlx::query(
        "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status,
                           progress, data)
         VALUES ($1, 'delete_dms_preferences_migration', 0, $2, $2, $2, 'in_progress', 0, '{}')",
    )
    .bind(&id)
    .bind(now_millis())
    .execute(&pool)
    .await
    .expect("the running job is planted");
    let mut refusals = Vec::new();
    for base in [GO, RUST] {
        let response = http
            .post(format!("{base}/api/v4/jobs/{id}/cancel"))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("the server answers");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        refusals.push((status, body["id"].clone()));
    }
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .expect("the planted job goes");
    assert_eq!(
        refusals[0],
        (
            400,
            serde_json::json!("api.job.unable_to_create_job.incorrect_job_type")
        )
    );
    assert_eq!(refusals[1], refusals[0]);

    let mut sides = Vec::new();
    let mut jobs = Vec::new();
    for side in ["go", "rs"] {
        let prefix = format!("mmrsdmcnl{side}");
        drop_dm_limits(&pool, &prefix).await;
        plant_dm_limits(&pool, &prefix, &dm_limit_values(350)).await;
        let (job_id, seen) = if side == "go" {
            go_run(&pool, "delete_dms_preferences_migration", &planted, true).await
        } else {
            rust_run(
                &pool,
                &app,
                job_runtime::delete_dms_preferences_migration_worker(),
                &planted,
                true,
            )
            .await
        };
        jobs.push(job_id);
        let remaining = remaining_dm_limits(&pool, &prefix).await;
        drop_dm_limits(&pool, &prefix).await;
        sides.push((summarise(&seen, &|s: &str| s.to_owned()), remaining));
    }
    drop_jobs(&pool, &jobs).await;

    let rust = sides.pop().expect("two sides");
    let go = sides.pop().expect("two sides");
    assert_eq!(
        go.0.statuses,
        ["in_progress", "cancel_requested", "success"],
        "{go:?}"
    );
    assert_eq!(go.0.final_progress, 0, "the progress writes were dropped");
    assert_eq!(go.0.final_data, planted, "and so were the data writes");
    assert_eq!(go.1, ["007", "1", "15", "40"], "the deletes were not");
    assert_eq!(rust, go);
}

// ---------------------------------------------------------------------------------------------
// The user export
// ---------------------------------------------------------------------------------------------

/// The report's file, its `FileInfo`, the delivering post, and whether a chunk file was left.
#[derive(Debug, PartialEq)]
struct Delivery {
    csv: String,
    file_info: (String, String, i64, String, String),
    post: (String, String, String, usize),
    chunk_left: bool,
}

async fn delivery(pool: &sqlx::PgPool, job_id: &str) -> Delivery {
    let path = format!("admin_reports/batch_report_{job_id}.csv");
    let (name, extension, size, creator, post_id, mime): (
        String,
        String,
        i64,
        String,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT name, extension, size, creatorid, postid, COALESCE(mimetype, '')
           FROM fileinfo WHERE path = $1",
    )
    .bind(&path)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("no FileInfo for {path}: {e}"));
    let (message, user_id, post_type, file_ids): (String, String, String, String) =
        sqlx::query_as("SELECT message, userid, type, fileids FROM posts WHERE id = $1")
            .bind(&post_id)
            .fetch_one(pool)
            .await
            .expect("the delivering post");
    let file_ids: Vec<String> = serde_json::from_str(&file_ids).unwrap_or_default();
    let dir = std::path::PathBuf::from(common::stack_data_dir());
    let csv = std::fs::read_to_string(dir.join(&path)).expect("the compiled report is on disk");
    let chunk_left = dir
        .join(format!("admin_reports/batch_report_{job_id}__0.csv"))
        .exists();
    Delivery {
        csv,
        file_info: (
            name.replace(job_id, "<job>"),
            extension,
            size,
            creator,
            mime,
        ),
        post: (message, user_id, post_type, file_ids.len()),
        chunk_left,
    }
}

/// The export of one team's users on each server: the same CSV byte for byte (Go's
/// `time.Time.String()` stamps, zone abbreviation included, and `encoding/csv` quoting), the same
/// `FileInfo` and post from the system bot to the requester, the chunk removed, and the same job
/// rows — `in_progress` at 0 with `file_count` and the cursor after the first chunk, then
/// `success` at 100.
#[tokio::test]
async fn the_user_export_writes_the_csv_go_writes_and_posts_it_alike() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let admin_id = logged_in_user_id().to_owned();
    let team = create_team(&http, &admin, "batchcsv").await;
    let mut users = Vec::new();
    for tag in ["batchcsva", "batchcsvb", "batchcsvc"] {
        users.push(create_plain_user(&http, &admin, &team, tag).await);
    }
    // A display name to render, a field to quote, and a deactivated account.
    sqlx::query(
        "UPDATE users SET firstname = 'Ada', lastname = 'Lovelace, Countess', nickname = 'ada'
          WHERE id = $1",
    )
    .bind(&users[0].id)
    .execute(&pool)
    .await
    .expect("names");
    let response = http
        .delete(format!("{GO}/api/v4/users/{}", users[2].id))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "deactivating");
    // The requester is not on the team, so the DM the first run creates changes no row the
    // second run reports.
    common::remove_user_from_team(&http, &admin, &team, &admin_id).await;
    common::invalidate_go_caches(&http, &admin).await;

    let data = serde_json::json!({
        "requesting_user_id": admin_id,
        "date_range": "all_time",
        "role": "",
        "team": team,
        "hide_active": "false",
        "hide_inactive": "false",
        "start_at": "0",
        "end_at": "0",
        "guest_filter": "",
    });
    let app = rust_app().await;
    let mut sides = Vec::new();
    let mut jobs = Vec::new();
    for side in ["go", "rs"] {
        let (id, seen) = if side == "go" {
            go_run(&pool, "export_users_to_csv", &data, false).await
        } else {
            rust_run(
                &pool,
                &app,
                job_runtime::export_users_to_csv_worker(),
                &data,
                false,
            )
            .await
        };
        sides.push((
            summarise(&seen, &|s: &str| s.to_owned()),
            delivery(&pool, &id).await,
        ));
        jobs.push(id);
    }
    drop_jobs(&pool, &jobs).await;

    let rust = sides.pop().expect("two sides");
    let go = sides.pop().expect("two sides");
    assert_eq!(go.0.final_status, "success", "{go:?}");
    assert_eq!(go.0.final_progress, 100);
    assert_eq!(go.0.final_data["file_count"], "1");
    assert_eq!(
        go.0.final_data["last_column_value"],
        common::plain_username("batchcsvc"),
        "the cursor is the last username of the only chunk"
    );
    assert_eq!(go.0.final_data["last_user_id"], users[2].id.as_str());
    assert_eq!(
        go.1.csv.lines().count(),
        4,
        "the header and three users: {}",
        go.1.csv
    );
    assert!(!go.1.chunk_left, "the chunk is cleaned up");
    assert_eq!(rust.0, go.0);
    assert_eq!(rust.1, go.1);
}
