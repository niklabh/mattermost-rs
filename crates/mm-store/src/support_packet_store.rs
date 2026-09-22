//! The three `SqlStore` methods the Support Packet reads that belong to no entity store:
//! `GetDBSchemaVersion` (sqlstore/store.go:1112), `GetSchemaDefinition`
//! (sqlstore/schema_dump.go:18) and `GetDiagnostics` (sqlstore/diagnostics.go:22).
//!
//! # Partial answers are the contract
//!
//! Each Go method keeps going after a failure and returns what it has **together with** the
//! accumulated error, and its caller treats the two differently: the schema dump discards the
//! partial schema when there is any error, the diagnostics keep every counter they got. So these
//! return the value and the error texts side by side rather than a `Result` — a `Result` would
//! make the caller's choice for it. The texts are Go's `errors.Wrap` prefixes around this crate's
//! own error text; the driver's message after the colon is sqlx's, not lib/pq's.

use std::collections::BTreeMap;
use std::time::Duration;

use mm_model::support_packet::{
    DatabaseColumn, DatabaseIndex, DatabaseTable, SupportPacketDatabaseSchema,
};

use crate::{SqlStore, StoreError};

/// `pgDiagnosticsQueryTimeout` (sqlstore/diagnostics.go:20).
const PG_DIAGNOSTICS_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Port of `store.DatabaseDiagnostics` (store/store.go) — the pool counters and the
/// PostgreSQL-only statistics. `None` is Go's nil pointer, which the YAML omits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DatabaseDiagnostics {
    pub master_connections_in_use: i64,
    pub master_connections_idle: i64,
    pub master_pool_wait_count: i64,
    pub master_pool_wait_duration_ms: i64,
    pub master_connections_closed_max_idle: i64,
    pub master_connections_closed_max_lifetime: i64,
    pub replica_connections_in_use: i64,
    pub replica_connections_idle: i64,
    pub replica_pool_wait_count: i64,
    pub replica_pool_wait_duration_ms: i64,
    pub replica_connections_closed_max_idle: i64,
    pub replica_connections_closed_max_lifetime: i64,
    pub cache_hit_ratio: Option<f64>,
    pub deadlocks: Option<i64>,
    pub temp_files: Option<i64>,
    pub temp_bytes_mb: Option<f64>,
    pub rollbacks: Option<i64>,
    pub idle_in_transaction_count: Option<i64>,
    pub longest_query_duration_seconds: Option<f64>,
    pub waiting_for_lock_count: Option<i64>,
    pub posts_dead_tuples: Option<i64>,
    pub posts_last_autovacuum: Option<chrono::DateTime<chrono::Utc>>,
}

fn db_error(context: &str, source: sqlx::Error) -> String {
    StoreError::Db {
        context: context.to_owned(),
        source,
    }
    .to_string()
}

impl SqlStore {
    /// Port of `SqlStore.GetDBSchemaVersion` (sqlstore/store.go:1112): the newest applied
    /// migration's version.
    #[tracing::instrument(skip_all, fields(version))]
    pub async fn get_db_schema_version(&self) -> Result<i64, StoreError> {
        let version = sqlx::query_scalar!(
            r#"SELECT version AS "version!" FROM db_migrations ORDER BY version DESC LIMIT 1"#
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "unable to select from db_migrations".to_owned(),
            source,
        })?;
        tracing::Span::current().record("version", version);
        Ok(version)
    }

    /// Port of `SqlStore.GetSchemaDefinition` (sqlstore/schema_dump.go:18): the collation, the
    /// encoding, and every table of the current schema with its options, columns and indexes —
    /// plus the error of each of the five steps that failed, in Go's order.
    ///
    /// Go assembles the tables by ranging a **map**, so its table order is random per call; here
    /// it is by name. `options` is a `map[string]string` and marshals sorted either way. Column
    /// order is `ordinal_position`, and index order is whatever `pg_indexes` returns for the same
    /// query text — Go's query has no `ORDER BY`, so neither does this one.
    #[tracing::instrument(skip_all, fields(tables, errors))]
    pub async fn get_schema_definition(&self) -> (SupportPacketDatabaseSchema, Vec<String>) {
        let mut schema = SupportPacketDatabaseSchema::default();
        let mut errors = Vec::new();

        match sqlx::query_scalar!(
            r#"SELECT datcollate::text AS "datcollate" FROM pg_database WHERE datname = current_database()"#
        )
        .fetch_one(&self.pool)
        .await
        {
            Ok(collation) => schema.database_collation = collation,
            Err(source) => errors.push(format!(
                "failed to get database collation: {}",
                db_error("failed to read pg_database", source)
            )),
        }

        match sqlx::query_scalar!(
            r#"SELECT pg_encoding_to_char(encoding)::text AS "encoding" FROM pg_database WHERE datname = current_database()"#
        )
        .fetch_one(&self.pool)
        .await
        {
            Ok(encoding) => schema.database_encoding = encoding.unwrap_or_default(),
            Err(source) => errors.push(format!(
                "failed to get database encoding: {}",
                db_error("failed to read pg_database", source)
            )),
        }

        let mut table_options: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        match sqlx::query!(
            r#"SELECT c.relname::text AS "table_name!", unnest(c.reloptions) AS "option_value!"
                 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE (n.nspname = current_schema() AND c.relkind = 'r' AND c.reloptions IS NOT NULL)"#
        )
        .fetch_all(&self.pool)
        .await
        {
            Ok(rows) => {
                for row in rows {
                    // "Parse option in format key=value"; anything else is skipped.
                    if let Some((key, value)) = row.option_value.split_once('=') {
                        table_options
                            .entry(row.table_name)
                            .or_default()
                            .insert(key.to_owned(), value.to_owned());
                    }
                }
            }
            Err(source) => errors.push(format!(
                "failed to query table options: {}",
                db_error("failed to read pg_class", source)
            )),
        }

        let mut tables: BTreeMap<String, DatabaseTable> = BTreeMap::new();
        let mut table_collations: BTreeMap<String, String> = BTreeMap::new();
        match sqlx::query!(
            r#"SELECT t.table_name::text AS "table_name", c.column_name::text AS "column_name",
                      c.data_type::text AS "data_type",
                      c.character_maximum_length::bigint AS "character_maximum_length",
                      c.is_nullable::text AS "is_nullable", c.collation_name::text AS "collation_name"
                 FROM information_schema.tables t
                 LEFT JOIN information_schema.columns c
                   ON t.table_name = c.table_name AND t.table_schema = c.table_schema
                WHERE t.table_schema = current_schema()
                ORDER BY t.table_name, c.ordinal_position"#
        )
        .fetch_all(&self.pool)
        .await
        {
            Ok(rows) => {
                let mut scan_errors = Vec::new();
                for row in rows {
                    // Go scans the first, second, third and fifth columns into plain strings,
                    // so a NULL in any of them is that row's scan error, and the row is skipped.
                    let (Some(table_name), Some(column_name), Some(data_type), Some(is_nullable)) =
                        (row.table_name, row.column_name, row.data_type, row.is_nullable)
                    else {
                        scan_errors.push(
                            "failed to scan database schema row: sql: Scan error: converting NULL to string is unsupported"
                                .to_owned(),
                        );
                        continue;
                    };
                    if let Some(collation) = row.collation_name.filter(|c| !c.is_empty()) {
                        table_collations
                            .entry(table_name.clone())
                            .or_insert(collation);
                    }
                    let table = tables
                        .entry(table_name.clone())
                        .or_insert_with(|| DatabaseTable {
                            name: table_name,
                            ..DatabaseTable::default()
                        });
                    if !column_name.is_empty() {
                        table.columns.push(DatabaseColumn {
                            name: column_name,
                            data_type,
                            max_length: row.character_maximum_length.unwrap_or(0),
                            is_nullable: is_nullable == "YES",
                        });
                    }
                }
                errors.extend(scan_errors);
            }
            Err(source) => errors.push(format!(
                "failed to query schema information: {}",
                db_error("failed to read information_schema", source)
            )),
        }

        let mut table_indexes: BTreeMap<String, Vec<DatabaseIndex>> = BTreeMap::new();
        match sqlx::query!(
            r#"SELECT tablename::text AS "tablename!", indexname::text AS "indexname!", indexdef AS "indexdef!"
                 FROM pg_indexes
                WHERE schemaname = current_schema()"#
        )
        .fetch_all(&self.pool)
        .await
        {
            Ok(rows) => {
                for row in rows {
                    table_indexes
                        .entry(row.tablename)
                        .or_default()
                        .push(DatabaseIndex {
                            name: row.indexname,
                            definition: row.indexdef,
                        });
                }
            }
            Err(source) => errors.push(format!(
                "failed to query index information: {}",
                db_error("failed to read pg_indexes", source)
            )),
        }

        for (name, mut table) in tables {
            if let Some(collation) = table_collations.remove(&name) {
                table.collation = collation;
            }
            if let Some(options) = table_options.remove(&name).filter(|o| !o.is_empty()) {
                table.options = options;
            }
            if let Some(indexes) = table_indexes.remove(&name) {
                table.indexes = indexes;
            }
            schema.tables.push(table);
        }

        let span = tracing::Span::current();
        span.record("tables", schema.tables.len());
        span.record("errors", errors.len());
        (schema, errors)
    }

    /// Port of `SqlStore.GetDiagnostics` (sqlstore/diagnostics.go:22) for PostgreSQL: the pool
    /// counters, then the three statistics queries, each under its own ten-second timeout and
    /// each failure recorded without stopping the next.
    ///
    /// # The pool counters are sqlx's, which has fewer of them
    ///
    /// `sql.DBStats` counts waits, wait time and connections closed for idleness or age over
    /// the process's lifetime. sqlx's pool keeps none of those, so they are **0** — the honest
    /// reading of "this pool never recorded one", not a measurement. In-use and idle are real.
    /// Go's replica figures come from `ReplicaDBStats`, a zero `DBStats` with no replica
    /// configured, which is the only configuration this store has.
    #[tracing::instrument(skip_all, fields(errors))]
    pub async fn get_diagnostics(&self) -> (DatabaseDiagnostics, Vec<String>) {
        let size = i64::from(self.pool.size());
        let idle = i64::try_from(self.pool.num_idle()).unwrap_or(i64::MAX);
        let mut diagnostics = DatabaseDiagnostics {
            master_connections_in_use: (size - idle).max(0),
            master_connections_idle: idle,
            ..DatabaseDiagnostics::default()
        };
        let mut errors = Vec::new();

        let timed = |what: &'static str, error: String| {
            format!("postgres diagnostics query failed for {what}: {error}")
        };

        match tokio::time::timeout(
            PG_DIAGNOSTICS_QUERY_TIMEOUT,
            sqlx::query!(
                r#"
SELECT
    COALESCE(blks_hit::double precision / NULLIF(blks_hit + blks_read, 0), 0) AS "cache_hit_ratio!",
    deadlocks AS "deadlocks!",
    temp_files AS "temp_files!",
    temp_bytes AS "temp_bytes!",
    xact_rollback AS "xact_rollback!"
FROM pg_stat_database
WHERE datname = current_database()"#
            )
            .fetch_one(&self.pool),
        )
        .await
        {
            Ok(Ok(row)) => {
                diagnostics.cache_hit_ratio = Some(row.cache_hit_ratio);
                diagnostics.deadlocks = Some(row.deadlocks);
                diagnostics.temp_files = Some(row.temp_files);
                diagnostics.temp_bytes_mb = Some(row.temp_bytes as f64 / (1024.0 * 1024.0));
                diagnostics.rollbacks = Some(row.xact_rollback);
            }
            Ok(Err(source)) => errors.push(timed(
                "pg_stat_database",
                db_error("failed to read pg_stat_database", source),
            )),
            Err(_) => errors.push(timed(
                "pg_stat_database",
                "context deadline exceeded".into(),
            )),
        }

        match tokio::time::timeout(
            PG_DIAGNOSTICS_QUERY_TIMEOUT,
            sqlx::query!(
                r#"
SELECT
    COUNT(*) FILTER (WHERE state = 'idle in transaction') AS "idle_in_transaction_count!",
    EXTRACT(EPOCH FROM COALESCE(
        MAX(clock_timestamp() - query_start) FILTER (WHERE state = 'active' AND query_start IS NOT NULL),
        interval '0 second'
    ))::double precision AS "longest_query_duration_seconds!",
    COUNT(*) FILTER (WHERE wait_event_type = 'Lock') AS "waiting_for_lock_count!"
FROM pg_stat_activity
WHERE datname = current_database()"#
            )
            .fetch_one(&self.pool),
        )
        .await
        {
            Ok(Ok(row)) => {
                diagnostics.idle_in_transaction_count = Some(row.idle_in_transaction_count);
                diagnostics.longest_query_duration_seconds =
                    Some(row.longest_query_duration_seconds);
                diagnostics.waiting_for_lock_count = Some(row.waiting_for_lock_count);
            }
            Ok(Err(source)) => errors.push(timed(
                "pg_stat_activity",
                db_error("failed to read pg_stat_activity", source),
            )),
            Err(_) => errors.push(timed("pg_stat_activity", "context deadline exceeded".into())),
        }

        match tokio::time::timeout(
            PG_DIAGNOSTICS_QUERY_TIMEOUT,
            sqlx::query!(
                r#"
SELECT
    n_dead_tup AS "n_dead_tup!",
    last_autovacuum
FROM pg_stat_user_tables
WHERE lower(relname) = 'posts'
  AND schemaname = current_schema()
LIMIT 1"#
            )
            .fetch_optional(&self.pool),
        )
        .await
        {
            // `sql.ErrNoRows` is not an error here: no `posts` table, no figures.
            Ok(Ok(None)) => {}
            Ok(Ok(Some(row))) => {
                diagnostics.posts_dead_tuples = Some(row.n_dead_tup);
                diagnostics.posts_last_autovacuum = row.last_autovacuum;
            }
            Ok(Err(source)) => errors.push(timed(
                "pg_stat_user_tables",
                db_error("failed to read pg_stat_user_tables", source),
            )),
            Err(_) => errors.push(timed(
                "pg_stat_user_tables",
                "context deadline exceeded".into(),
            )),
        }

        tracing::Span::current().record("errors", errors.len());
        (diagnostics, errors)
    }
}
