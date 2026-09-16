//! Entry point for the `mm-api` server.
//!
//! Runs in front of the Go server: serves the migrated routes and forwards the rest.
//!
//! ```text
//! client ──▶ mm-api :8066 ──┬──▶ handled here (GET /api/v4/users/me)
//!                           └──▶ Go server :8065  (everything else)
//!                                     │
//!                            one shared Postgres
//! ```

use std::path::PathBuf;

use anyhow::Context;
use mm_api::{AppState, local, router};
use mm_app::App;
use mm_store::SqlStore;

/// Kept small on purpose: the Go server is sizing its own pool against the same Postgres, and
/// during a migration the interesting failure is two servers exhausting the connection limit
/// while each behaves as though it were alone.
const DEFAULT_MAX_DB_CONNECTIONS: u32 = 8;

/// `LocalModeSocketPath` (model/config.go), Go's default for
/// `ServiceSettings.LocalModeSocketLocation`.
///
/// It is a **shared, fixed path**, which is fine for one server on a host and is a collision for
/// the numbered development stacks — `startLocalModeServer` opens with `os.RemoveAll(socket)`, so
/// the last server to start silently unlinks every other one's socket. `scripts/stack-env.sh`
/// therefore sets both variables per stack; this default exists only so a bare `cargo run` with
/// local mode on finds the same socket a stock Go server would.
const GO_DEFAULT_LOCAL_SOCKET: &str = "/var/tmp/mattermost_local.socket";

/// Where this server puts *its* local socket when nothing says otherwise.
///
/// Deliberately not Go's path: both servers run side by side during the migration, and a shared
/// path means whichever starts second unlinks the other's socket and then proxies to a socket
/// that is no longer there.
const DEFAULT_LOCAL_SOCKET: &str = "/var/tmp/mmrs_local.socket";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mm_api=info,mm_app=info,mm_store=info".into()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must be set — see docker-compose.yml for the development value")?;
    let go_upstream =
        std::env::var("MM_GO_UPSTREAM").unwrap_or_else(|_| "http://localhost:8065".to_owned());
    let listen = std::env::var("MM_API_LISTEN").unwrap_or_else(|_| "0.0.0.0:8066".to_owned());
    let max_connections = match std::env::var("MM_API_MAX_DB_CONNECTIONS") {
        Ok(value) => value
            .parse()
            .context("MM_API_MAX_DB_CONNECTIONS must be a positive integer")?,
        Err(_) => DEFAULT_MAX_DB_CONNECTIONS,
    };

    let store = SqlStore::connect(&database_url, max_connections)
        .await
        .context("could not connect to the shared Postgres")?;
    tracing::info!("connected to the shared database");

    // Read the configuration the Go server is actually running on, rather than guessing at it:
    // the active `Configurations` row it persists, with the `MM_<SECTION>_<SETTING>` overlay on
    // top, in that order. See `mm_app::config` for why the order is not interchangeable, and
    // `docker-compose.yml` for the `MM_CONFIG` line that puts the document in the shared database
    // in the first place.
    //
    // This is fatal on failure. A server that cannot read its configuration would answer on
    // *defaults*, and the settings it would then get wrong are permission gates — every one of
    // `RestrictSystemAdmin` and `ComplianceSettings.Enable` fails towards over-granting. Starting
    // anyway and hoping is the failure mode this whole change exists to remove.
    let config = mm_app::config::Config::load(store.config()).await.context(
        "could not load the active configuration from the shared database. If this says the \
             `configurations` relation does not exist, the Go server is still on its config.json \
             file store: add the MM_CONFIG line from docker-compose.yml and recreate the \
             container with `docker compose up -d mattermost`",
    )?;
    tracing::info!(
        restrict_system_admin = config.restrict_system_admin,
        compliance_enable = config.compliance_enable,
        show_full_name = config.show_full_name,
        show_email_address = config.show_email_address,
        licensed = !config.license.is_empty(),
        "loaded configuration from the shared database"
    );

    let app = App::with_config(store, config);

    // `markdown.SetMaxPostRunes(ps.MaxPostSize())` (platform/service.go:338): the markdown
    // walker refuses inputs longer than four bytes per rune of the post limit, and the limit is
    // read from the `Posts.Message` column at boot. `GetMaxPostSize` swallows its own error and
    // answers the v1 default, so a failure here is the same default rather than a refusal to
    // boot.
    let max_post_size = match app.max_post_size().await {
        Ok(size) => size,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the maximum post size; using the v1 default");
            mm_model::post::POST_MESSAGE_MAX_RUNES_V1
        }
    };
    mm_markdown::set_max_post_runes(max_post_size);
    tracing::info!(
        max_post_size,
        max_markdown_len = mm_markdown::max_len(),
        "markdown limits set"
    );

    // Port of the initial product-notices fetch (app/server.go:544) and of the
    // `product_notices` job's schedule (jobs/product_notices/scheduler.go): one fetch now, in
    // the background as Go's `s.platform.Go`, then one every `NoticesFetchFrequency` seconds
    // while either notice gate is on. Both are this process's own cache; see
    // `mm_app::product_notices`.
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let enabled =
                    app.config().admin_notices_enabled || app.config().user_notices_enabled;
                if enabled {
                    if let Err(err) = app.update_product_notices().await {
                        tracing::warn!(error = %err, "Failed to perform initial product notices fetch");
                    }
                }
                let period = u64::try_from(app.config().notices_fetch_frequency).unwrap_or(3600);
                tokio::time::sleep(std::time::Duration::from_secs(period.max(1))).await;
            }
        });
    }

    // The job workers: `Server.StartWorkers` (app/server.go), which is the `Watcher` polling
    // `Jobs` for `pending` rows and handing each to the worker registered for its type.
    //
    // **Off unless `MM_API_ENABLE_JOB_WORKERS` says otherwise**, and that default is not caution
    // about the port — it is what the deployment is. Running the workers is safe beside the Go
    // server: `ClaimJob` is one optimistic `UPDATE … WHERE Status = 'pending'`, which is exactly
    // how Mattermost runs this loop on every node of a cluster, so at most one of the two servers
    // claims any given job. What it is not is *useful* by default — the Go server already runs a
    // worker for all twenty-nine registered types and this one runs `cleanup_desktop_tokens`, so
    // turning it on only decides which process does that delete. The switch exists so the loop
    // can be exercised on a stack, and so the default cannot surprise anyone running the parity
    // suite, where a job claimed here rather than there changes who answers.
    //
    // The **schedulers** are not started at all, under any variable; see
    // `mm_app::job_scheduler`'s module note and [D-802]. They have no optimistic guard, both
    // servers believe they are the cluster leader, and each period would queue its own job.
    if job_workers_enabled() {
        let app = app.clone();
        let workers = std::sync::Arc::new(mm_app::job_runtime::registered_workers());
        let watcher = mm_app::job_runtime::Watcher::new(
            mm_app::job_runtime::DEFAULT_WATCHER_POLLING_INTERVAL_MS,
        );
        tracing::info!(
            workers = workers.len(),
            "job workers enabled; the watcher will poll Jobs for pending rows"
        );
        tokio::spawn(async move { app.run_watcher(workers, watcher).await });
    }

    let state = AppState::new(app, go_upstream.clone());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("could not bind {listen}"))?;

    // The local-mode admin API, on a unix socket beside the Go server's own.
    //
    // Off unless `MM_SERVICESETTINGS_ENABLELOCALMODE` says otherwise, which is Go's default and
    // Go's variable name. **Read from the environment and not from the configuration document**
    // that `Config::load` parsed: `mm_app::config::Config` does not model either of these two
    // settings, and a deployment that enables local mode only in the database therefore gets no
    // socket from this server. That is [D-321], and it is a gap rather than a decision.
    if local_mode_enabled() {
        let socket = PathBuf::from(
            std::env::var("MM_API_LOCAL_SOCKET")
                .unwrap_or_else(|_| DEFAULT_LOCAL_SOCKET.to_owned()),
        );
        let go_socket = PathBuf::from(
            std::env::var("MM_SERVICESETTINGS_LOCALMODESOCKETLOCATION")
                .unwrap_or_else(|_| GO_DEFAULT_LOCAL_SOCKET.to_owned()),
        );

        // Binding both servers to one path is not a misconfiguration that degrades — it is this
        // process unlinking the socket it is about to forward to, and then forwarding to itself.
        // Fatal at startup, where it is one line to fix.
        anyhow::ensure!(
            socket != go_socket,
            "MM_API_LOCAL_SOCKET and MM_SERVICESETTINGS_LOCALMODESOCKETLOCATION are the same \
             path ({}). They are two servers' sockets and must differ — see scripts/stack-env.sh",
            socket.display()
        );

        let local_listener = local::bind(&socket)
            .await
            .with_context(|| format!("could not bind the local socket {}", socket.display()))?;
        let local_router = local::router(state.clone(), go_socket.clone());

        tracing::info!(
            socket = %socket.display(),
            go_socket = %go_socket.display(),
            "local-mode API listening; unmigrated local routes forward to the Go server's socket"
        );

        tokio::spawn(async move {
            if let Err(err) = local::serve(local_listener, local_router).await {
                tracing::error!(error = %err, "the local-mode server stopped");
            }
        });
    }

    tracing::info!(
        listen = %listen,
        upstream = %go_upstream,
        "mm-api listening; unmigrated routes forward to the Go server"
    );

    axum::serve(listener, router(state))
        .await
        .context("server error")?;

    Ok(())
}

/// Whether to run the job workers. Not a Mattermost setting — Go starts its workers
/// unconditionally — so this is deliberately `MM_API_`-prefixed rather than `MM_SERVICESETTINGS_`,
/// and parsed the same way Go parses a bool so a typo cannot read as true.
fn job_workers_enabled() -> bool {
    match std::env::var("MM_API_ENABLE_JOB_WORKERS") {
        Ok(value) => matches!(value.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True"),
        Err(_) => false,
    }
}

/// `*ServiceSettings.EnableLocalMode`, from the environment.
///
/// Go's `applyEnvironmentMap` parses a bool with `strconv.ParseBool`, which accepts `1`, `t`,
/// `T`, `TRUE`, `true`, `True` and their false counterparts — and **rejects** anything else,
/// leaving the setting at its previous value rather than treating a typo as true. Reproduced so a
/// `MM_SERVICESETTINGS_ENABLELOCALMODE=yes` does not silently open an unauthenticated socket.
fn local_mode_enabled() -> bool {
    match std::env::var("MM_SERVICESETTINGS_ENABLELOCALMODE") {
        Ok(value) => matches!(value.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True"),
        Err(_) => false,
    }
}
