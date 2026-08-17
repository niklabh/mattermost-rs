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

use anyhow::Context;
use mm_api::{AppState, router};
use mm_app::App;
use mm_store::SqlStore;

/// Kept small on purpose: the Go server is sizing its own pool against the same Postgres, and
/// during a migration the interesting failure is two servers exhausting the connection limit
/// while each behaves as though it were alone.
const DEFAULT_MAX_DB_CONNECTIONS: u32 = 8;

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

    let state = AppState::new(App::new(store), go_upstream.clone());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("could not bind {listen}"))?;

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
