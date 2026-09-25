//! Port of app/plugin_db_driver.go: the database driver a plugin queries the server's database
//! through (`DriverImpl`), served over the plugin RPC as db_rpc.go serves it.
//!
//! # What a plugin sees
//!
//! Every connection is a [`gopq::Conn`], the port of the lib/pq connection Go's `*sql.Conn`
//! wraps, so each call sends the protocol messages lib/pq sends and answers with lib/pq's values
//! and errors: a query without arguments goes through the simple protocol, one with arguments
//! through an unnamed prepare; result columns are binary for `bytea`, `int2/4/8` and `uuid` and
//! text otherwise; a `*pq.Error` crosses whole and every other error as `encodableError` wraps it,
//! with the `database/sql` sentinels (`driver.ErrBadConn`, `io.EOF`, `sql.ErrConnDone`) named by
//! their codes.
//!
//! `database/sql`'s part is kept too. `ConnPing`, `ConnQuery`, `ConnExec`, `Tx` and `Stmt` run
//! through `(*sql.Conn).Raw`, which closes the `*sql.Conn` when the driver answers
//! `driver.ErrBadConn`: the connection is then closed, its entry stays in the map, and every later
//! call through it answers `sql.ErrConnDone`, `ConnClose` included (which also forgets it). An id
//! that is not in the map answers `driver.ErrBadConn`, as Go's comment explains.
//!
//! A date or time column decodes to a `time.Time`, which gob cannot carry in an interface (it is
//! never registered): Go fails to encode the `RowsNext` reply and closes the plugin's database
//! connection. [`value_to_interface`] makes the same reply fail the same way, and go-netrpc then
//! closes the connection just as Go's `gobServerCodec` does.
//!
//! # Where it differs
//!
//! - **One server connection per `Conn`.** Go takes a connection from its `*sql.DB` pool and
//!   `ConnClose` returns it there, so a plugin may be handed a session another caller used (with
//!   its temporary tables and `SET`s); here each `Conn` opens a fresh session with Go's own DSN
//!   (`SqlSettings.DataSource`, or a replica under the same rule as `GetInternalReplicaDB`) and
//!   closes it when nothing else holds it. `SqlSettings.QueryTimeout` bounds the open, as it bounds
//!   Go's wait for a pooled connection. [D-1340]
//! - **An unknown transaction, statement or rows id.** Go dereferences the missing map entry and
//!   the server process panics. Here the RPC call fails instead, with go-netrpc's service error,
//!   and the plugin's `database/sql` reports that. `RowsColumnTypeDatabaseTypeName` and
//!   `RowsColumnTypePrecisionScale` with an index outside the columns are the same case.
//! - **After a bad connection is closed**, a statement, transaction or rows handle still holding
//!   it answers `driver.ErrBadConn`; Go's would read or write the closed socket and answer the
//!   `*net.OpError` naming both addresses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gobwire::Interface;
use gobwire::names;
use mm_plugin::error::{PluginError, Sentinel, encodable_error};
use mm_plugin::rpc::{AppDriver, Driver, NotImplemented};
use mm_plugin::wire::plugin::{
    ResultContainer, Z_DbBoolReturn, Z_DbConnArgs, Z_DbErrReturn, Z_DbIntReturn,
    Z_DbResultContErrReturn, Z_DbRowScanArg, Z_DbRowScanReturn, Z_DbRowsColumnArg,
    Z_DbRowsColumnTypePrecisionScaleReturn, Z_DbStmtArgs, Z_DbStmtQueryArgs, Z_DbStrErrReturn,
    Z_DbStrSliceReturn, Z_DbTxArgs,
};
use mm_plugin::wire::{pq, sql_driver};

use crate::App;

/// The lib/pq connection behind one `*sql.Conn`. `None` once `database/sql` closed it.
type SharedConn = Arc<tokio::sync::Mutex<Option<gopq::Conn>>>;

/// `connMeta`: the connection and the plugin that opened it, with `*sql.Conn`'s `done` flag.
struct ConnEntry {
    plugin_id: String,
    conn: SharedConn,
    done: AtomicBool,
}

/// A statement, a transaction or a result set, each holding the connection it runs on, as lib/pq's
/// `*stmt`, `*conn` (its own `driver.Tx`) and `*rows` hold their `*conn`.
struct Handle<T> {
    conn: SharedConn,
    inner: tokio::sync::Mutex<T>,
}

type Tx = Handle<()>;
type Stmt = Handle<gopq::Stmt>;
type Rows = Handle<gopq::Rows>;

#[derive(Default)]
struct Maps {
    conns: HashMap<String, Arc<ConnEntry>>,
    txs: HashMap<String, Arc<Tx>>,
    stmts: HashMap<String, Arc<Stmt>>,
    rows: HashMap<String, Arc<Rows>>,
}

/// Port of `app.DriverImpl` (plugin_db_driver.go:22).
pub struct AppPluginDriver {
    app: App,
    maps: Mutex<Maps>,
    /// `SqlStore.rrCounter`, for the replica round robin.
    replica_counter: AtomicU64,
}

impl AppPluginDriver {
    /// `NewDriverImpl`.
    pub fn new(app: App) -> Self {
        Self {
            app,
            maps: Mutex::new(Maps::default()),
            replica_counter: AtomicU64::new(0),
        }
    }

    fn maps(&self) -> std::sync::MutexGuard<'_, Maps> {
        self.maps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `GetInternalMasterDB` / `GetInternalReplicaDB`'s DSN, and `QueryTimeout`.
    ///
    /// A replica is used only when `DataSourceReplicas` is not empty and a licence is loaded, and
    /// then in turn, as `GetInternalReplicaDB` chooses.
    async fn data_source(&self, is_master: bool) -> Result<(String, i64), PluginError> {
        let config = crate::config::load_model_config(self.app.store().config())
            .await
            .map_err(|e| PluginError::Message(e.to_string()))?;
        let sql = &config.sql_settings;
        let timeout = sql.query_timeout.unwrap_or(30);
        let replicas = sql.data_source_replicas.as_deref().unwrap_or_default();
        if !is_master && !replicas.is_empty() && matches!(self.app.license().await, Ok(Some(_))) {
            let n = self.replica_counter.fetch_add(1, Ordering::Relaxed) + 1;
            let i = (n % replicas.len() as u64) as usize;
            return Ok((replicas[i].clone(), timeout));
        }
        Ok((sql.data_source.clone().unwrap_or_default(), timeout))
    }

    /// `DriverImpl.conn`.
    async fn open(&self, is_master: bool, plugin_id: &str) -> Result<String, PluginError> {
        let (dsn, timeout) = self.data_source(is_master).await?;
        let connect = gopq::Conn::connect(&dsn);
        let conn =
            match tokio::time::timeout(Duration::from_secs(timeout.max(0) as u64), connect).await {
                Ok(result) => result.map_err(pq_error)?,
                Err(_) => return Err(PluginError::Message("context deadline exceeded".into())),
            };
        let id = mm_model::utils::new_id();
        self.maps().conns.insert(
            id.clone(),
            Arc::new(ConnEntry {
                plugin_id: plugin_id.to_owned(),
                conn: Arc::new(tokio::sync::Mutex::new(Some(conn))),
                done: AtomicBool::new(false),
            }),
        );
        Ok(id)
    }

    /// The entry for `conn_id`, or `driver.ErrBadConn` when there is none.
    fn entry(&self, conn_id: &str) -> Result<Arc<ConnEntry>, PluginError> {
        self.maps()
            .conns
            .get(conn_id)
            .cloned()
            .ok_or(PluginError::Sentinel(Sentinel::BadConn))
    }

    /// `(*sql.Conn).Raw` around `f`: `sql.ErrConnDone` once the `*sql.Conn` is closed, and a
    /// `driver.ErrBadConn` answer closes it.
    async fn raw<T>(
        &self,
        conn_id: &str,
        f: impl AsyncFnOnce(&mut gopq::Conn, &SharedConn) -> Result<T, gopq::Error>,
    ) -> Result<T, PluginError> {
        let entry = self.entry(conn_id)?;
        if entry.done.load(Ordering::SeqCst) {
            return Err(PluginError::Sentinel(Sentinel::ConnDone));
        }
        let mut guard = entry.conn.lock().await;
        let Some(conn) = guard.as_mut() else {
            return Err(PluginError::Sentinel(Sentinel::ConnDone));
        };
        let result = f(conn, &entry.conn).await;
        if let Err(gopq::Error::BadConn) = result {
            entry.done.store(true, Ordering::SeqCst);
            if let Some(conn) = guard.take() {
                let _ = conn.close().await;
            }
        }
        result.map_err(pq_error)
    }

    /// `(*sql.Conn).Close`: `sql.ErrConnDone` when it was already closed. The session ends when
    /// no statement, transaction or rows handle still holds it.
    async fn close_entry(entry: &ConnEntry) -> Result<(), PluginError> {
        if entry.done.swap(true, Ordering::SeqCst) {
            return Err(PluginError::Sentinel(Sentinel::ConnDone));
        }
        let mut guard = entry.conn.lock().await;
        if Arc::strong_count(&entry.conn) == 1
            && let Some(conn) = guard.take()
        {
            let _ = conn.close().await;
        }
        Ok(())
    }

    fn stmt(&self, id: &str) -> Option<Arc<Stmt>> {
        self.maps().stmts.get(id).cloned()
    }

    fn rows(&self, id: &str) -> Option<Arc<Rows>> {
        self.maps().rows.get(id).cloned()
    }

    fn insert_rows(&self, conn: &SharedConn, rows: gopq::Rows) -> String {
        let id = mm_model::utils::new_id();
        self.maps().rows.insert(
            id.clone(),
            Arc::new(Handle {
                conn: Arc::clone(conn),
                inner: tokio::sync::Mutex::new(rows),
            }),
        );
        id
    }
}

/// Run `f` on the connection a handle holds; `driver.ErrBadConn` once it was closed.
async fn on_conn<T>(
    conn: &SharedConn,
    f: impl AsyncFnOnce(&mut gopq::Conn) -> Result<T, gopq::Error>,
) -> Result<T, gopq::Error> {
    let mut guard = conn.lock().await;
    match guard.as_mut() {
        Some(conn) => f(conn).await,
        None => Err(gopq::Error::BadConn),
    }
}

/// A lib/pq error as `encodableError` sends it.
fn pq_error(e: gopq::Error) -> PluginError {
    match e {
        gopq::Error::Pq(e) => PluginError::Postgres(Box::new(pq::Error {
            severity: e.severity,
            code: e.code,
            message: e.message,
            detail: e.detail,
            hint: e.hint,
            position: e.position,
            internal_position: e.internal_position,
            internal_query: e.internal_query,
            r#where: e.r#where,
            schema: e.schema,
            table: e.table,
            column: e.column,
            data_type_name: e.data_type_name,
            constraint: e.constraint,
            file: e.file,
            line: e.line,
            routine: e.routine,
        })),
        gopq::Error::BadConn => PluginError::Sentinel(Sentinel::BadConn),
        gopq::Error::Eof => PluginError::Sentinel(Sentinel::Eof),
        other => PluginError::Message(other.to_string()),
    }
}

fn err_field(result: Result<(), PluginError>) -> Option<Interface> {
    encodable_error(result.err().as_ref())
}

/// A `driver.Value` a plugin sent, by the name gob registered its type under. Any other type is
/// kept by name, and lib/pq refuses to encode it.
fn interface_to_value(value: Option<&Interface>) -> gopq::Value {
    let Some(value) = value else {
        return gopq::Value::Null;
    };
    let other = || gopq::Value::Other(value.name.clone());
    match value.name.as_str() {
        names::INT64 => value
            .downcast::<i64>()
            .map_or_else(|_| other(), gopq::Value::Int64),
        names::FLOAT64 => value
            .downcast::<f64>()
            .map_or_else(|_| other(), gopq::Value::Float64),
        names::BOOL => value
            .downcast::<bool>()
            .map_or_else(|_| other(), gopq::Value::Bool),
        names::BYTES => value
            .downcast::<Vec<u8>>()
            .map_or_else(|_| other(), gopq::Value::Bytes),
        names::STRING => value
            .downcast::<String>()
            .map_or_else(|_| other(), gopq::Value::String),
        _ => other(),
    }
}

/// A value lib/pq decoded, as gob carries it in an interface. A `time.Time` is given no
/// registered name, so encoding the reply fails as it does in Go.
fn value_to_interface(value: &gopq::Value) -> Option<Interface> {
    match value {
        gopq::Value::Null => None,
        gopq::Value::Int64(v) => Interface::new(names::INT64, v).ok(),
        gopq::Value::Float64(v) => Some(Interface::float64(*v)),
        gopq::Value::Bool(v) => Some(Interface::bool(*v)),
        gopq::Value::Bytes(v) => Interface::new(names::BYTES, v).ok(),
        gopq::Value::String(v) => Some(Interface::string(v.as_str())),
        gopq::Value::Time(text) | gopq::Value::Other(text) => {
            let mut unregistered = Interface::string(text.as_str());
            unregistered.name = String::new();
            Some(unregistered)
        }
    }
}

fn named_values(args: &[sql_driver::NamedValue]) -> Vec<gopq::NamedValue> {
    args.iter()
        .map(|a| gopq::NamedValue {
            name: a.name.clone(),
            ordinal: a.ordinal,
            value: interface_to_value(a.value.as_ref()),
        })
        .collect()
}

/// `ret.LastID, ret.LastIDError = res.LastInsertId()` and the same for `RowsAffected`.
fn result_container(result: gopq::ExecResult) -> ResultContainer {
    let (last_id, last_id_error) = match result.last_insert_id() {
        Ok(id) => (id, None),
        Err(e) => (0, encodable_error(Some(&pq_error(e)))),
    };
    let (rows_affected, rows_affected_error) = match result.rows_affected() {
        Ok(n) => (n, None),
        Err(e) => (0, encodable_error(Some(&pq_error(e)))),
    };
    ResultContainer {
        last_id,
        last_id_error,
        rows_affected,
        rows_affected_error,
    }
}

fn str_err(result: Result<String, PluginError>) -> Z_DbStrErrReturn {
    match result {
        Ok(a) => Z_DbStrErrReturn { a, b: None },
        Err(e) => Z_DbStrErrReturn {
            a: String::new(),
            b: encodable_error(Some(&e)),
        },
    }
}

fn result_err(result: Result<gopq::ExecResult, PluginError>) -> Z_DbResultContErrReturn {
    match result {
        Ok(r) => Z_DbResultContErrReturn {
            a: result_container(r),
            b: None,
        },
        Err(e) => Z_DbResultContErrReturn {
            a: ResultContainer::default(),
            b: encodable_error(Some(&e)),
        },
    }
}

impl Driver for AppPluginDriver {
    /// `Conn(isMaster)`: a connection no plugin owns. A plugin's own call arrives through
    /// `driverForPlugin` as [`AppDriver::conn_with_plugin_id`].
    async fn conn(&self, args: bool) -> Result<Z_DbStrErrReturn, NotImplemented> {
        Ok(str_err(self.open(args, "").await))
    }

    /// `ConnPing`: lib/pq's `Ping`, whose every failure is `driver.ErrBadConn`.
    async fn conn_ping(&self, args: String) -> Result<Z_DbErrReturn, NotImplemented> {
        let result = self.raw(&args, async |c, _| c.ping().await).await;
        Ok(Z_DbErrReturn {
            a: err_field(result),
        })
    }

    /// `ConnQuery`: `QueryContext` with the arguments as sent.
    async fn conn_query(&self, args: Z_DbConnArgs) -> Result<Z_DbStrErrReturn, NotImplemented> {
        let values = named_values(&args.c);
        let result = self
            .raw(&args.a, async |c, shared| {
                let rows = c.query(&args.b, &values).await?;
                Ok(self.insert_rows(shared, rows))
            })
            .await;
        Ok(str_err(result))
    }

    /// `ConnExec`: `ExecContext`, which keeps only each argument's value.
    async fn conn_exec(
        &self,
        args: Z_DbConnArgs,
    ) -> Result<Z_DbResultContErrReturn, NotImplemented> {
        let values: Vec<gopq::Value> = named_values(&args.c).into_iter().map(|v| v.value).collect();
        let result = self
            .raw(&args.a, async |c, _| c.exec(&args.b, &values).await)
            .await;
        Ok(result_err(result))
    }

    /// `ConnClose`: forgotten first, then closed.
    async fn conn_close(&self, args: String) -> Result<Z_DbErrReturn, NotImplemented> {
        let entry = self.maps().conns.remove(&args);
        let result = match entry {
            None => Err(PluginError::Sentinel(Sentinel::BadConn)),
            Some(entry) => Self::close_entry(&entry).await,
        };
        Ok(Z_DbErrReturn {
            a: err_field(result),
        })
    }

    /// `Tx`: lib/pq's `BeginTx`; the transaction is the connection itself.
    async fn tx(&self, args: Z_DbTxArgs) -> Result<Z_DbStrErrReturn, NotImplemented> {
        let opts = args.b;
        let result = self
            .raw(&args.a, async |c, shared| {
                c.begin_tx(opts.isolation, opts.read_only).await?;
                let id = mm_model::utils::new_id();
                self.maps().txs.insert(
                    id.clone(),
                    Arc::new(Handle {
                        conn: Arc::clone(shared),
                        inner: tokio::sync::Mutex::new(()),
                    }),
                );
                Ok(id)
            })
            .await;
        Ok(str_err(result))
    }

    /// `TxCommit`: forgotten whatever the outcome.
    async fn tx_commit(&self, args: String) -> Result<Z_DbErrReturn, NotImplemented> {
        let tx = self.maps().txs.remove(&args).ok_or(NotImplemented)?;
        let result = on_conn(&tx.conn, async |c| c.commit().await).await;
        Ok(Z_DbErrReturn {
            a: err_field(result.map_err(pq_error)),
        })
    }

    /// `TxRollback`: forgotten whatever the outcome.
    async fn tx_rollback(&self, args: String) -> Result<Z_DbErrReturn, NotImplemented> {
        let tx = self.maps().txs.remove(&args).ok_or(NotImplemented)?;
        let result = on_conn(&tx.conn, async |c| c.rollback().await).await;
        Ok(Z_DbErrReturn {
            a: err_field(result.map_err(pq_error)),
        })
    }

    /// `Stmt`: lib/pq's `Prepare`, a statement named `1`, `2`, … on its connection.
    async fn stmt(&self, args: Z_DbStmtArgs) -> Result<Z_DbStrErrReturn, NotImplemented> {
        let result = self
            .raw(&args.a, async |c, shared| {
                let stmt = c.prepare(&args.b).await?;
                let id = mm_model::utils::new_id();
                self.maps().stmts.insert(
                    id.clone(),
                    Arc::new(Handle {
                        conn: Arc::clone(shared),
                        inner: tokio::sync::Mutex::new(stmt),
                    }),
                );
                Ok(id)
            })
            .await;
        Ok(str_err(result))
    }

    /// `StmtClose`: closed, then forgotten whatever the outcome.
    async fn stmt_close(&self, args: String) -> Result<Z_DbErrReturn, NotImplemented> {
        let stmt = self.stmt(&args).ok_or(NotImplemented)?;
        let result = {
            let mut st = stmt.inner.lock().await;
            on_conn(&stmt.conn, async |c| st.close(c).await).await
        };
        self.maps().stmts.remove(&args);
        Ok(Z_DbErrReturn {
            a: err_field(result.map_err(pq_error)),
        })
    }

    /// `StmtNumInput`.
    async fn stmt_num_input(&self, args: String) -> Result<Z_DbIntReturn, NotImplemented> {
        let stmt = self.stmt(&args).ok_or(NotImplemented)?;
        let a = stmt.inner.lock().await.num_input();
        Ok(Z_DbIntReturn { a })
    }

    /// `StmtQuery`: `st.Query` with the values alone, renumbered from 1.
    async fn stmt_query(
        &self,
        args: Z_DbStmtQueryArgs,
    ) -> Result<Z_DbStrErrReturn, NotImplemented> {
        let stmt = self.stmt(&args.a).ok_or(NotImplemented)?;
        let values = positional(&args.b);
        let result = {
            let st = stmt.inner.lock().await;
            on_conn(&stmt.conn, async |c| st.query(c, &values).await).await
        };
        Ok(str_err(
            result
                .map(|rows| self.insert_rows(&stmt.conn, rows))
                .map_err(pq_error),
        ))
    }

    /// `StmtExec`: `st.Exec` with the values alone, renumbered from 1.
    async fn stmt_exec(
        &self,
        args: Z_DbStmtQueryArgs,
    ) -> Result<Z_DbResultContErrReturn, NotImplemented> {
        let stmt = self.stmt(&args.a).ok_or(NotImplemented)?;
        let values = positional(&args.b);
        let result = {
            let st = stmt.inner.lock().await;
            on_conn(&stmt.conn, async |c| st.exec(c, &values).await).await
        };
        Ok(result_err(result.map_err(pq_error)))
    }

    /// `RowsColumns`: nil for a statement without columns.
    async fn rows_columns(&self, args: String) -> Result<Z_DbStrSliceReturn, NotImplemented> {
        let rows = self.rows(&args).ok_or(NotImplemented)?;
        let a = rows
            .inner
            .lock()
            .await
            .columns()
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        Ok(Z_DbStrSliceReturn { a })
    }

    /// `RowsClose`: read to the end, then forgotten whatever the outcome.
    async fn rows_close(&self, args: String) -> Result<Z_DbErrReturn, NotImplemented> {
        let rows = self.rows(&args).ok_or(NotImplemented)?;
        let result = {
            let mut rs = rows.inner.lock().await;
            on_conn(&rows.conn, async |c| rs.close(c).await).await
        };
        self.maps().rows.remove(&args);
        Ok(Z_DbErrReturn {
            a: err_field(result.map_err(pq_error)),
        })
    }

    /// `RowsNext`: the destination comes back filled — as far as the row reaches, and whatever
    /// the plugin sent beyond it.
    async fn rows_next(&self, args: Z_DbRowScanArg) -> Result<Z_DbRowScanReturn, NotImplemented> {
        let rows = self.rows(&args.a).ok_or(NotImplemented)?;
        let before: Vec<gopq::Value> = args
            .b
            .iter()
            .map(|v| interface_to_value(v.as_ref()))
            .collect();
        let mut dest = before.clone(); // `before` is compared against after the call
        let result = {
            let mut rs = rows.inner.lock().await;
            on_conn(&rows.conn, async |c| rs.next(c, &mut dest).await).await
        };
        let b = args
            .b
            .into_iter()
            .zip(before.iter().zip(&dest))
            .map(|(sent, (old, new))| {
                if old == new {
                    sent
                } else {
                    value_to_interface(new)
                }
            })
            .collect();
        Ok(Z_DbRowScanReturn {
            a: err_field(result.map_err(pq_error)),
            b,
        })
    }

    /// `RowsHasNextResultSet`.
    async fn rows_has_next_result_set(
        &self,
        args: String,
    ) -> Result<Z_DbBoolReturn, NotImplemented> {
        let rows = self.rows(&args).ok_or(NotImplemented)?;
        let a = rows.inner.lock().await.has_next_result_set();
        Ok(Z_DbBoolReturn { a })
    }

    /// `RowsNextResultSet`: `io.EOF` when no further result set was seen.
    async fn rows_next_result_set(&self, args: String) -> Result<Z_DbErrReturn, NotImplemented> {
        let rows = self.rows(&args).ok_or(NotImplemented)?;
        let result = rows.inner.lock().await.next_result_set();
        Ok(Z_DbErrReturn {
            a: err_field(result.map_err(pq_error)),
        })
    }

    /// `RowsColumnTypeDatabaseTypeName`: lib/pq's upper-case type name, `""` for an OID it does
    /// not know.
    async fn rows_column_type_database_type_name(
        &self,
        args: Z_DbRowsColumnArg,
    ) -> Result<String, NotImplemented> {
        let rows = self.rows(&args.a).ok_or(NotImplemented)?;
        let rs = rows.inner.lock().await;
        let column = column(rs.column_types(), args.b)?;
        Ok(column.name().to_owned())
    }

    /// `RowsColumnTypePrecisionScale`: only a `numeric` column has one.
    async fn rows_column_type_precision_scale(
        &self,
        args: Z_DbRowsColumnArg,
    ) -> Result<Z_DbRowsColumnTypePrecisionScaleReturn, NotImplemented> {
        let rows = self.rows(&args.a).ok_or(NotImplemented)?;
        let rs = rows.inner.lock().await;
        let (a, b, c) = column(rs.column_types(), args.b)?.precision_scale();
        Ok(Z_DbRowsColumnTypePrecisionScaleReturn { a, b, c })
    }
}

/// `colTyps[index]`; Go panics outside the columns.
fn column(types: &[gopq::FieldDesc], index: i64) -> Result<gopq::FieldDesc, NotImplemented> {
    usize::try_from(index)
        .ok()
        .and_then(|i| types.get(i).copied())
        .ok_or(NotImplemented)
}

/// `argVals[i] = a.Value`, then `toNamedValue`: names dropped, ordinals from 1.
fn positional(args: &[sql_driver::NamedValue]) -> Vec<gopq::NamedValue> {
    args.iter()
        .enumerate()
        .map(|(i, a)| gopq::NamedValue::positional(i, interface_to_value(a.value.as_ref())))
        .collect()
}

impl AppDriver for AppPluginDriver {
    /// `ConnWithPluginID`: a connection `ShutdownConns` closes when the plugin stops.
    async fn conn_with_plugin_id(
        &self,
        is_master: bool,
        plugin_id: &str,
    ) -> Result<Z_DbStrErrReturn, NotImplemented> {
        Ok(str_err(self.open(is_master, plugin_id).await))
    }

    /// `ShutdownConns`: close and forget every connection the plugin left open, logging a
    /// failure.
    async fn shutdown_conns(&self, plugin_id: &str) {
        let entries: Vec<Arc<ConnEntry>> = {
            let mut maps = self.maps();
            let ids: Vec<String> = maps
                .conns
                .iter()
                .filter(|(_, e)| e.plugin_id == plugin_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| maps.conns.remove(id)).collect()
        };
        for entry in entries {
            if let Err(err) = Self::close_entry(&entry).await {
                tracing::error!(error = %err.go_error(), plugin_id, "Error while closing DB connection");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_cross_gob_as_go_registers_them() {
        for value in [
            gopq::Value::Int64(-7),
            gopq::Value::Float64(1.5),
            gopq::Value::Bool(true),
            gopq::Value::Bytes(vec![0, 1, 255]),
            gopq::Value::String("s".into()),
        ] {
            let sent = value_to_interface(&value);
            assert_eq!(interface_to_value(sent.as_ref()), value);
        }
        assert_eq!(value_to_interface(&gopq::Value::Null), None);
        assert_eq!(interface_to_value(None), gopq::Value::Null);
        assert_eq!(
            value_to_interface(&gopq::Value::Int64(1)).map(|i| i.name),
            Some("int64".to_owned())
        );
        assert_eq!(
            value_to_interface(&gopq::Value::Bytes(vec![1])).map(|i| i.name),
            Some("[]uint8".to_owned())
        );
        assert_eq!(
            interface_to_value(Some(&Interface::int(3))),
            gopq::Value::Other("int".into())
        );
    }

    #[test]
    fn a_time_cannot_be_encoded() {
        let time = value_to_interface(&gopq::Value::Time("2026-01-01".into()));
        let reply = Z_DbRowScanReturn {
            a: None,
            b: vec![time],
        };
        assert!(gobwire::Encoder::new().encode(&reply).is_err());
    }

    #[test]
    fn errors_cross_as_encodable_error_makes_them() {
        let decoded = |e: gopq::Error| {
            mm_plugin::error::decodable_error(encodable_error(Some(&pq_error(e))).as_ref())
        };
        assert_eq!(
            decoded(gopq::Error::BadConn),
            Some(PluginError::Sentinel(Sentinel::BadConn))
        );
        assert_eq!(
            decoded(gopq::Error::Eof),
            Some(PluginError::Sentinel(Sentinel::Eof))
        );
        let pq = gopq::PqError {
            severity: "ERROR".into(),
            code: "42601".into(),
            message: "m".into(),
            query: "q".into(),
            ..Default::default()
        };
        let Some(PluginError::Postgres(e)) = decoded(gopq::Error::Pq(Box::new(pq))) else {
            panic!("a pq.Error crosses whole");
        };
        assert_eq!((e.code.as_str(), e.message.as_str()), ("42601", "m"));
    }

    #[test]
    fn a_result_reports_what_lib_pq_reports() {
        let rc = result_container(gopq::ExecResult::RowsAffected(3));
        assert_eq!((rc.last_id, rc.rows_affected), (0, 3));
        assert!(rc.rows_affected_error.is_none());
        let message = |i: Option<Interface>| {
            mm_plugin::error::decodable_error(i.as_ref()).map(|e| e.go_error())
        };
        assert_eq!(
            message(rc.last_id_error),
            Some("LastInsertId is not supported by this driver".into())
        );
        let rc = result_container(gopq::ExecResult::Empty);
        assert_eq!(
            message(rc.rows_affected_error),
            Some("no RowsAffected available after the empty statement".into())
        );
    }
}
