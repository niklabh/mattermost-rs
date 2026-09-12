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
