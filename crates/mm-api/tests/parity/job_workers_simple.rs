//! Cross-server parity for the `SimpleWorker` bodies of D-804, one job type at a time: the same
//! planting is run once by the **Go server's** worker (a pending row its watcher picks up within
//! one fifteen-second poll) and once by **this crate's** worker (`do_job` called on a fresh
//! pending row), and the two job rows and side effects are compared.
//!
//! ```sh
//! scripts/parity.sh --test parity job_workers_simple
//! ```
//!
//! # Who runs which job
//!
//! Both servers poll one `Jobs` table, and `ClaimJob` decides. The Go half plants a row and waits
//! for Go to finish it; the stack's mm-api runs no watcher (`MM_API_ENABLE_JOB_WORKERS` is unset
//! there), so nothing else competes. The Rust half plants a row and claims it **at once** through
//! `do_job` — Go's watcher would have to poll inside those few milliseconds to take it first, and
//! the claim assertion below says so if it ever does.
//!
//! Rows are prefixed `mmrsjs`.

use std::time::Duration;

use mm_app::App;
use mm_app::job_runtime::{do_job, registered_workers};
use mm_model::job::Job;
use mm_store::{JobStore, SqlStore};
use sqlx::PgPool;

use crate::common;

use common::{RUST, client, go_minted_token, stack_enabled};

/// One test at a time: the `expiry_notify` test starts an mm-api whose watcher claims **any**
/// pending job of a type it runs, which includes the Go halves of the other tests here.
static JOBS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

fn now() -> i64 {
    mm_model::utils::get_millis()
}

/// A 26-character id carrying `tag`.
fn id(tag: &str) -> String {
    format!("mmrsjs{tag:x>20}")
}

/// A pending row. `create_at` of `1` puts it at the **head** of Go's queue: the watcher offers
/// oldest first and an idle worker takes one job per poll, so a row planted "now" can sit behind
/// a backlog Go's own scheduler queued (measured: `active_users` had dozens pending).
async fn plant_job(pool: &PgPool, job_id: &str, job_type: &str) {
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(job_id)
        .execute(pool)
        .await
        .expect("clears the old row");
    sqlx::query(
        "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status,
                           progress, data)
         VALUES ($1, $2, 0, $3, 0, 0, 'pending', 0, $4)",
    )
    .bind(job_id)
    .bind(job_type)
    .bind(1_i64)
    .bind(serde_json::json!({"planted": "yes"}))
    .execute(pool)
    .await
    .expect("the job row is written");
}

async fn read_job(pool: &PgPool, job_id: &str) -> Job {
    mm_store::SqlJobStore::new(pool.clone())
        .get(job_id)
        .await
        .expect("the job row is there")
}

/// What a finished job leaves that must agree: status, progress, data — and that it was started.
fn outcome(job: &Job) -> serde_json::Value {
    assert!(job.start_at > 0, "the job was claimed: {job:?}");
    serde_json::json!({
        "status": job.status,
        "progress": job.progress,
        "data": job.data,
    })
}

/// Wait for whichever server's worker takes the pending row to finish it.
async fn wait_done(pool: &PgPool, job_id: &str) -> Job {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let job = read_job(pool, job_id).await;
        if job.status != "pending" && job.status != "in_progress" {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Go never finished {job_id}: {job:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Plant a row and run it through this crate's registered worker, as the watcher would.
async fn rust_runs(pool: &PgPool, job_id: &str, job_type: &str) -> Job {
    plant_job(pool, job_id, job_type).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));
    let workers = registered_workers();
    let slot = workers
        .get(job_type)
        .unwrap_or_else(|| panic!("{job_type} has a worker here"))
        .clone();
    let job = read_job(pool, job_id).await;
    do_job(app, slot, job).await;
    let done = read_job(pool, job_id).await;
    assert_ne!(
        done.status, "pending",
        "our claim lost the race to Go's watcher; re-run"
    );
    done
}

// -------------------------------------------------------------------------------------------
// refresh_materialized_views
// -------------------------------------------------------------------------------------------

/// A team with one channel, a bot, two posts by the bot in it, and a live file row — every one of
/// the four views gains something only a refresh can show.
struct Planted {
    team: String,
    bot: String,
}

/// Every row the view test planted on an earlier run. Once per test, **before** either side
/// plants: purging per side would delete the Go side's file row as the Rust side adds its own,
/// leaving the live count where the stale `file_stats` already had it.
async fn purge_views_fixture(pool: &PgPool) {
    for statement in [
        "DELETE FROM posts WHERE id LIKE 'mmrsjs%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsjs%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsjs%'",
        "DELETE FROM fileinfo WHERE id LIKE 'mmrsjs%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("clears the old fixture");
    }
}

/// **Every id is new on every run.** A materialized view keeps what its last refresh saw, so a
/// fixture that reused its ids would find last run's rows already in the view and pass with the
/// refresh removed — measured: dropping the `poststats` and the `file_stats` refresh both
/// survived until this nonce existed.
async fn plant_views_fixture(pool: &PgPool, side: &str) -> Planted {
    let nonce = format!("{side}{}", now() % 100_000_000);
    let team = id(&format!("t{nonce}"));
    let channel = id(&format!("c{nonce}"));
    let bot = common::plant_bot(&format!("js{nonce}"), common::logged_in_user_id(), 0)
        .await
        .expect("a bot");
    let at = now();
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                            email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                            schemeid, groupconstrained, cloudlimitsarchived)
         VALUES ($1, $2, $2, 0, 'mmrs js', $3, '', '', 'O', '', '', $1, false, NULL, false, false)",
    )
    .bind(&team)
    .bind(at)
    .bind(format!("mmrs-js-{nonce}"))
    .execute(pool)
    .await
    .expect("the team");
    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name,
                               header, purpose, lastpostat, totalmsgcount, extraupdateat,
                               creatorid, totalmsgcountroot, lastrootpostat)
         VALUES ($1, $2, $2, 0, $3, 'O', 'mmrs js', $4, '', '', $2, 2, 0, '', 2, $2)",
    )
    .bind(&channel)
    .bind(at)
    .bind(&team)
    .bind(format!("mmrs-js-{nonce}"))
    .execute(pool)
    .await
    .expect("the channel");
    for n in 0..2 {
        sqlx::query(
            "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid,
                                channelid, rootid, originalid, message, type, props, hashtags,
                                filenames, fileids, hasreactions, remoteid)
             VALUES ($1, $2, $2, 0, 0, false, $3, $4, '', '', 'mmrs js', '', '{}', '', '[]', '[]',
                     false, NULL)",
        )
        .bind(id(&format!("p{n}{nonce}")))
        .bind(at + n)
        .bind(&bot)
        .bind(&channel)
        .execute(pool)
        .await
        .expect("a post");
    }
    sqlx::query(
        "INSERT INTO fileinfo (id, creatorid, postid, createat, updateat, deleteat, path,
                               thumbnailpath, previewpath, name, extension, size, mimetype,
                               width, height, haspreviewimage, minipreview, content, remoteid,
                               archived, channelid)
         VALUES ($1, $2, '', $3, $3, 0, '', '', '', 'mmrs.txt', 'txt', 7, 'text/plain', 0, 0,
                 false, NULL, '', NULL, false, $4)",
    )
    .bind(id(&format!("f{nonce}")))
    .bind(&bot)
    .bind(at)
    .bind(&channel)
    .execute(pool)
    .await
    .expect("a file row");
    Planted { team, bot }
}

async fn scalar(pool: &PgPool, sql: &str, bind: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(bind)
        .fetch_optional(pool)
        .await
        .expect("the view reads")
        .unwrap_or(0)
}

async fn live_files(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fileinfo WHERE deleteat = 0")
        .fetch_one(pool)
        .await
        .expect("counts")
}

/// What each view says about this side's fixture after the job, reduced to booleans: `true` means
/// the view was refreshed after the planting.
async fn views_after(
    pool: &PgPool,
    planted: &Planted,
    files_before: i64,
    files_after: i64,
) -> [bool; 4] {
    let by_team = scalar(
        pool,
        "SELECT sum(num)::bigint FROM posts_by_team_day WHERE teamid = $1",
        &planted.team,
    )
    .await;
    let bots_by_team = scalar(
        pool,
        "SELECT sum(num)::bigint FROM bot_posts_by_team_day WHERE teamid = $1",
        &planted.team,
    )
    .await;
    let per_user = scalar(
        pool,
        "SELECT sum(numposts)::bigint FROM poststats WHERE userid = $1",
        &planted.bot,
    )
    .await;
    let files: i64 = sqlx::query_scalar("SELECT num FROM file_stats")
        .fetch_one(pool)
        .await
        .expect("file_stats has one row");
    [
        by_team == 2,
        bots_by_team == 2,
        per_user == 2,
        (files_before..=files_after).contains(&files),
    ]
}

/// `refresh_materialized_views` refreshes all four views — `posts_by_team_day`,
/// `bot_posts_by_team_day`, `file_stats`, `poststats` — and ends `success` at progress 100 with
/// the planted data untouched, on both servers.
#[tokio::test]
async fn refresh_materialized_views_runs_like_go() {
    if !stack_enabled() {
        return;
    }
    let _jobs = JOBS.lock().await;
    let http = client();
    let _ = go_minted_token(&http).await;
    let pool = pool().await;
    let job_type = "refresh_materialized_views";

    purge_views_fixture(&pool).await;
    let mut sides = Vec::new();
    for side in ["go", "rs"] {
        let planted = plant_views_fixture(&pool, side).await;
        let files_before = live_files(&pool).await;
        let job_id = id(&format!("rmv{side}"));
        let job = if side == "go" {
            plant_job(&pool, &job_id, job_type).await;
            wait_done(&pool, &job_id).await
        } else {
            rust_runs(&pool, &job_id, job_type).await
        };
        let files_after = live_files(&pool).await;
        sides.push((
            outcome(&job),
            views_after(&pool, &planted, files_before, files_after).await,
        ));
        common::unplant_bot(&planted.bot).await;
    }
    let rust = sides.pop().expect("two sides");
    let go = sides.pop().expect("two sides");
    assert_eq!(go.0["status"], "success", "Go: {go:?}");
    assert_eq!(go.1, [true; 4], "Go refreshed every view: {go:?}");
    assert_eq!(rust, go, "the job and the views differ");
}

// -------------------------------------------------------------------------------------------
// mobile_session_metadata, active_users
// -------------------------------------------------------------------------------------------

/// `mobile_session_metadata` is enabled by default on both servers and, with no metrics
/// interface, finishes `success` without reading anything.
#[tokio::test]
async fn mobile_session_metadata_runs_like_go() {
    if !stack_enabled() {
        return;
    }
    let _jobs = JOBS.lock().await;
    let pool = pool().await;
    let job_type = "mobile_session_metadata";
    let go_id = id("msmgo");
    plant_job(&pool, &go_id, job_type).await;
    let go = outcome(&wait_done(&pool, &go_id).await);
    let rust = outcome(&rust_runs(&pool, &id("msmrs"), job_type).await);
    assert_eq!(go["status"], "success", "{go}");
    assert_eq!(rust, go);
}

async fn patch_metrics(http: &reqwest::Client, admin: &str, enable: bool, listen: &str) {
    let response = http
        .put(format!("{RUST}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "MetricsSettings": { "Enable": enable, "ListenAddress": listen }
        }))
        .send()
        .await
        .expect("mm-api answers");
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status, 200, "MetricsSettings.Enable={enable}: {body}");
}

/// `active_users` is off unless `MetricsSettings.Enable`, so Go only runs it with metrics on —
/// patched for the Go half, with `ListenAddress` `":0"` so the metrics listener Go then starts
/// takes a free port rather than the shared `:8067`, and restored.
#[tokio::test]
async fn active_users_runs_like_go() {
    if !stack_enabled() {
        return;
    }
    let _jobs = JOBS.lock().await;
    let _document = common::CONFIG_DOCUMENT.write().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let pool = pool().await;
    let job_type = "active_users";

    let before: serde_json::Value = http
        .get(format!("{}/api/v4/config", common::GO))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a config");
    let listen = before["MetricsSettings"]["ListenAddress"]
        .as_str()
        .unwrap_or(":8067")
        .to_owned();

    patch_metrics(&http, &admin, true, ":0").await;
    let go_id = id("augo");
    plant_job(&pool, &go_id, job_type).await;
    let go = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(wait_done(
        &pool, &go_id,
    )))
    .await;
    patch_metrics(&http, &admin, false, &listen).await;
    let go = match go {
        Ok(job) => job,
        Err(panic) => std::panic::resume_unwind(panic),
    };

    let go = outcome(&go);
    let rust = outcome(&rust_runs(&pool, &id("aurs"), job_type).await);
    assert_eq!(go["status"], "success", "{go}");
    assert_eq!(rust, go);
}

// -------------------------------------------------------------------------------------------
// expiry_notify
// -------------------------------------------------------------------------------------------

async fn set_extend_sessions(http: &reqwest::Client, admin: &str, on: bool) {
    let response = http
        .put(format!("{RUST}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "ServiceSettings": { "ExtendSessionLengthWithActivity": on } }))
        .send()
        .await
        .expect("mm-api answers");
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status, 200, "ExtendSessionLengthWithActivity={on}: {body}");
}

/// The mm-api that runs `expiry_notify`'s Rust half; see `second_server_ports`.
const JOB_WORKER_RUST_PORT: u16 = 8113;

/// A mobile session of `user_id` that expired five minutes ago and has not been notified, plus
/// three that `GetSessionsExpired` must pass over: one expired two hours ago (outside the hour),
/// one already notified, and a web session, which has no device.
async fn plant_expired_sessions(pool: &PgPool, user_id: &str, device: &str, nonce: &str) -> String {
    let at = now();
    let rows = [
        (
            format!("x{nonce}"),
            device.to_owned(),
            at - 5 * 60 * 1000,
            false,
        ),
        (
            format!("o{nonce}"),
            format!("{device}old"),
            at - 2 * 60 * 60 * 1000,
            false,
        ),
        (
            format!("n{nonce}"),
            format!("{device}told"),
            at - 5 * 60 * 1000,
            true,
        ),
        // A web session: no `DeviceId`, so not mobile.
        (
            format!("w{nonce}"),
            String::new(),
            at - 5 * 60 * 1000,
            false,
        ),
    ];
    for (tag, device_id, expires_at, notified) in &rows {
        sqlx::query(
            "INSERT INTO sessions (id, token, createat, expiresat, lastactivityat, userid,
                                   deviceid, roles, isoauth, props, expirednotify)
             VALUES ($1, $2, $3, $4, $3, $5, $6, 'system_user', false, '{}', $7)",
        )
        .bind(id(tag))
        .bind(id(&format!("k{tag}")))
        .bind(at - 24 * 60 * 60 * 1000)
        .bind(expires_at)
        .bind(user_id)
        .bind(device_id)
        .bind(notified)
        .execute(pool)
        .await
        .expect("a session");
    }
    id(&format!("x{nonce}"))
}

async fn expired_notify(pool: &PgPool, session_id: &str) -> bool {
    sqlx::query_scalar("SELECT expirednotify FROM sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .expect("the session is there")
}

/// `expiry_notify` pushes a v2 `session` message — unsigned, in the user's locale, naming the
/// site and the mobile session length — to each mobile session that expired in the last hour and
/// was not yet told, and then sets `ExpiredNotify`. Go needs `ExtendSessionLengthWithActivity`
/// for the worker to run at all, so the Go half patches it on and restores it.
#[tokio::test]
async fn expiry_notify_pushes_like_go() {
    if !stack_enabled() {
        return;
    }
    let _jobs = JOBS.lock().await;
    let _document = common::CONFIG_DOCUMENT.write().await;
    let _setting = common::SESSION_EXPIRY_SETTING.write().await;
    let proxy = common::push_proxy::push_proxy().expect("the push proxy is bound");
    let http = client();
    let admin = go_minted_token(&http).await;
    let pool = pool().await;
    sqlx::query("DELETE FROM sessions WHERE id LIKE 'mmrsjs%'")
        .execute(&pool)
        .await
        .expect("purges old sessions");
    let (team, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = common::create_plain_user(&http, &admin, &team, "jsexpiry").await;

    let mut sides = Vec::new();
    for side in ["go", "rs"] {
        let nonce = format!("{side}{}", now() % 100_000_000);
        let device = format!("android_rn:mmrsjs{nonce}");
        let session = plant_expired_sessions(&pool, &user.id, &device, &nonce).await;
        let job_id = id(&format!("en{side}"));
        let job = if side == "go" {
            set_extend_sessions(&http, &admin, true).await;
            plant_job(&pool, &job_id, "expiry_notify").await;
            let job = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                wait_done(&pool, &job_id),
            ))
            .await;
            set_extend_sessions(&http, &admin, false).await;
            match job {
                Ok(job) => job,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        } else {
            // **On a running mm-api, not in this process.** The stack hands the push settings
            // to its servers through the environment (`MM_EMAILSETTINGS_*`), not the stored
            // document, so an in-process `App` reads push as off and sends nothing — measured.
            // A second mm-api with the stack's push environment, its job workers on, and
            // `ExtendSessionLengthWithActivity` on **in its environment only** is the one
            // process whose `expiry_notify` worker is enabled: Go's reads the stored `false`
            // and never claims the row.
            plant_job(&pool, &job_id, "expiry_notify").await;
            let push_server = format!("http://localhost:{}", common::push_proxy::push_port());
            let server = common::SecondServer::start(
                JOB_WORKER_RUST_PORT,
                &[
                    ("MM_API_ENABLE_JOB_WORKERS", "true"),
                    ("MM_EMAILSETTINGS_SENDPUSHNOTIFICATIONS", "true"),
                    (
                        "MM_EMAILSETTINGS_PUSHNOTIFICATIONSERVER",
                        push_server.as_str(),
                    ),
                    ("MM_SERVICESETTINGS_EXTENDSESSIONLENGTHWITHACTIVITY", "true"),
                ],
            )
            .await
            .expect("the job-worker mm-api starts");
            let job = wait_done(&pool, &job_id).await;
            drop(server);
            job
        };
        let device_key = device.trim_start_matches("android_rn:").to_owned();
        let push = proxy
            .take(Duration::from_secs(10), |r| {
                r.json()["device_id"].as_str() == Some(device_key.as_str())
            })
            .await
            .unwrap_or_else(|| panic!("{side}: no push for the expired session"));
        let mut body = common::push_proxy::normalize_push(push.json());
        body["device_id"] = serde_json::Value::from("<device>");
        let others = [format!("{device_key}old"), format!("{device_key}told")];
        let stray = proxy
            .take(Duration::from_millis(500), |r| {
                let device = r.json()["device_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                device.is_empty() || others.contains(&device)
            })
            .await;
        sides.push((
            outcome(&job),
            push.path.clone(),
            body,
            expired_notify(&pool, &session).await,
            stray.is_none(),
        ));
    }
    let rust = sides.pop().expect("two sides");
    let go = sides.pop().expect("two sides");
    assert_eq!(go.0["status"], "success", "Go: {go:?}");
    assert!(go.3, "Go set ExpiredNotify");
    assert!(
        go.4,
        "Go pushed nothing to the old or the already-notified session"
    );
    assert_eq!(rust, go, "the job, the push and the flag differ");

    common::delete_plain_user(&http, &admin, &user.id).await;
}
