//! A Mattermost plugin that runs a fixed script of database-driver calls on activation and writes
//! down every reply, for `mm-api`'s `parity::plugin_driver`.
//!
//! Like `hook_recorder`, one binary runs under both hosts, so the two transcripts are the same
//! function of what each host answered. Each call is one JSON line appended to
//! `$DRIVER_SCRIPT_TRANSCRIPT`:
//!
//! ```text
//! {"step": "<label>", "reply": <render_typed of the reply>}
//! {"step": "<label>", "rpc_error": "<the net/rpc client's error>"}
//! ```
//!
//! and the last line is `{"step": "done"}`. The calls go through the raw net/rpc client, not the
//! SDK's `DriverClient` methods, because those answer zero values for a failed call and the
//! failure is part of what is compared.
//!
//! Connection, statement, transaction and rows ids are minted by each host, so every id a reply
//! carries is replaced by the script's own name for it (`conn1`, `rows3`, …) before the line is
//! written. Everything else is compared as gob carried it: the values, their Go types, NULLs, the
//! `*pq.Error` fields, and the sentinel codes of the errors that crossed as `ErrorString`.
//!
//! The script reads and writes `mmrs_plugin_driver`, which the parity test plants before each
//! host starts. Its last step reads a `timestamptz`, whose `time.Time` gob cannot carry in an
//! interface: the host's reply fails to encode and the connection closes, so it runs last.

use std::io::Write as _;
use std::sync::{Mutex, OnceLock};

use gobwire::{Decode, Encode, Interface};
use mm_plugin::rpc::{
    ApiClient, DriverClient, Hooks, HooksFileUpload, HooksHttp, NotImplemented, Plugin, client_main,
};
use mm_plugin::wire::plugin::{
    Z_DbBoolReturn, Z_DbConnArgs, Z_DbErrReturn, Z_DbIntReturn, Z_DbResultContErrReturn,
    Z_DbRowScanArg, Z_DbRowScanReturn, Z_DbRowsColumnArg, Z_DbRowsColumnTypePrecisionScaleReturn,
    Z_DbStmtArgs, Z_DbStmtQueryArgs, Z_DbStrErrReturn, Z_DbStrSliceReturn, Z_DbTxArgs,
    Z_OnActivateReturns,
};
use mm_plugin::wire::sql_driver::{NamedValue, TxOptions};
use serde_json::{Value as Json, json};

/// `render.rs` reads a gob oracle for its fixture helpers; this plugin loads no fixture.
fn oracle_dir() -> std::path::PathBuf {
    panic!("driver_script loads no gob fixtures")
}

#[path = "../tests/common/render.rs"]
mod render;

use render::render_typed;

/// The planted table.
const T: &str = "mmrs_plugin_driver";

struct Script {
    transcript: Mutex<std::fs::File>,
    driver: OnceLock<DriverClient>,
}

/// One run of the script: the client, and the ids minted so far with the names they are written
/// under.
struct Run<'a> {
    client: &'a go_netrpc::Client,
    ids: Vec<(String, String)>,
    out: &'a Mutex<std::fs::File>,
}

fn i64v(v: i64) -> Option<Interface> {
    Interface::new(gobwire::names::INT64, &v).ok()
}

fn strv(v: &str) -> Option<Interface> {
    Some(Interface::string(v))
}

fn bytesv(v: &[u8]) -> Option<Interface> {
    Interface::new(gobwire::names::BYTES, &v.to_vec()).ok()
}

/// Positional arguments, as `database/sql` numbers them.
fn args(values: Vec<Option<Interface>>) -> Vec<NamedValue> {
    values
        .into_iter()
        .enumerate()
        .map(|(i, value)| NamedValue {
            name: String::new(),
            ordinal: i as i64 + 1,
            value,
        })
        .collect()
}

impl Run<'_> {
    fn write(&self, line: &Json) {
        let mut text = line.to_string();
        for (id, name) in &self.ids {
            text = text.replace(id.as_str(), name);
        }
        let mut file = self.out.lock().unwrap();
        let _ = writeln!(file, "{text}");
        let _ = file.flush();
    }

    async fn call<A, R>(&mut self, step: &str, method: &str, args: &A) -> Option<R>
    where
        A: Encode,
        R: Encode + Decode + Default + Send + 'static,
    {
        match self
            .client
            .call::<A, R>(&format!("Plugin.{method}"), args)
            .await
        {
            Ok(reply) => {
                self.write(&json!({ "step": step, "reply": render_typed(&reply) }));
                Some(reply)
            }
            Err(e) => {
                self.write(&json!({ "step": step, "rpc_error": e.to_string() }));
                None
            }
        }
    }

    /// A call that answers an id: named `name` from here on.
    async fn id_call<A: Encode>(
        &mut self,
        step: &str,
        method: &str,
        args: &A,
        name: &str,
    ) -> String {
        let reply = self
            .client
            .call::<A, Z_DbStrErrReturn>(&format!("Plugin.{method}"), args)
            .await;
        match reply {
            Ok(reply) => {
                if !reply.a.is_empty() {
                    self.ids.push((reply.a.clone(), name.to_owned()));
                }
                self.write(&json!({ "step": step, "reply": render_typed(&reply) }));
                reply.a
            }
            Err(e) => {
                self.write(&json!({ "step": step, "rpc_error": e.to_string() }));
                String::new()
            }
        }
    }

    async fn conn(&mut self, master: bool, name: &str) -> String {
        self.id_call(&format!("Conn {name}"), "Conn", &master, name)
            .await
    }

    async fn ping(&mut self, conn: &str, step: &str) {
        self.call::<_, Z_DbErrReturn>(step, "ConnPing", &conn.to_owned())
            .await;
    }

    async fn query(
        &mut self,
        conn: &str,
        q: &str,
        values: Vec<Option<Interface>>,
        name: &str,
    ) -> String {
        let a = Z_DbConnArgs {
            a: conn.to_owned(),
            b: q.to_owned(),
            c: args(values),
        };
        self.id_call(&format!("ConnQuery {name}: {q}"), "ConnQuery", &a, name)
            .await
    }

    async fn exec(&mut self, conn: &str, q: &str, values: Vec<Option<Interface>>) {
        let a = Z_DbConnArgs {
            a: conn.to_owned(),
            b: q.to_owned(),
            c: args(values),
        };
        self.call::<_, Z_DbResultContErrReturn>(&format!("ConnExec: {q}"), "ConnExec", &a)
            .await;
    }

    async fn next(&mut self, rows: &str, dest: Vec<Option<Interface>>, step: &str) -> bool {
        let a = Z_DbRowScanArg {
            a: rows.to_owned(),
            b: dest,
        };
        let reply = self
            .call::<_, Z_DbRowScanReturn>(&format!("RowsNext {step}"), "RowsNext", &a)
            .await;
        reply.is_some_and(|r| r.a.is_none())
    }

    /// `Next` with `width` nil slots until it fails, and once more after.
    async fn drain(&mut self, rows: &str, width: usize, name: &str) {
        for i in 0..10 {
            let more = self
                .next(rows, vec![None; width], &format!("{name} #{i}"))
                .await;
            if !more {
                self.next(rows, vec![None; width], &format!("{name} after the end"))
                    .await;
                return;
            }
        }
    }

    async fn columns(&mut self, rows: &str, name: &str) {
        self.call::<_, Z_DbStrSliceReturn>(
            &format!("RowsColumns {name}"),
            "RowsColumns",
            &rows.to_owned(),
        )
        .await;
    }

    async fn column_types(&mut self, rows: &str, n: i64, name: &str) {
        for i in 0..n {
            let a = Z_DbRowsColumnArg {
                a: rows.to_owned(),
                b: i,
            };
            self.call::<_, String>(
                &format!("RowsColumnTypeDatabaseTypeName {name} {i}"),
                "RowsColumnTypeDatabaseTypeName",
                &a,
            )
            .await;
            self.call::<_, Z_DbRowsColumnTypePrecisionScaleReturn>(
                &format!("RowsColumnTypePrecisionScale {name} {i}"),
                "RowsColumnTypePrecisionScale",
                &a,
            )
            .await;
        }
    }

    async fn result_sets(&mut self, rows: &str, name: &str) {
        self.call::<_, Z_DbBoolReturn>(
            &format!("RowsHasNextResultSet {name}"),
            "RowsHasNextResultSet",
            &rows.to_owned(),
        )
        .await;
        self.call::<_, Z_DbErrReturn>(
            &format!("RowsNextResultSet {name}"),
            "RowsNextResultSet",
            &rows.to_owned(),
        )
        .await;
    }

    async fn close_rows(&mut self, rows: &str, name: &str) {
        self.call::<_, Z_DbErrReturn>(&format!("RowsClose {name}"), "RowsClose", &rows.to_owned())
            .await;
    }

    async fn tx(&mut self, conn: &str, isolation: i64, read_only: bool, name: &str) -> String {
        let a = Z_DbTxArgs {
            a: conn.to_owned(),
            b: TxOptions {
                isolation,
                read_only,
            },
        };
        self.id_call(&format!("Tx {name}"), "Tx", &a, name).await
    }

    async fn tx_end(&mut self, tx: &str, method: &str, name: &str) {
        self.call::<_, Z_DbErrReturn>(&format!("{method} {name}"), method, &tx.to_owned())
            .await;
    }

    async fn stmt(&mut self, conn: &str, q: &str, name: &str) -> String {
        let a = Z_DbStmtArgs {
            a: conn.to_owned(),
            b: q.to_owned(),
        };
        self.id_call(&format!("Stmt {name}: {q}"), "Stmt", &a, name)
            .await
    }

    async fn stmt_query(&mut self, st: &str, values: Vec<Option<Interface>>, name: &str) -> String {
        let a = Z_DbStmtQueryArgs {
            a: st.to_owned(),
            b: args(values),
        };
        self.id_call(&format!("StmtQuery {name}"), "StmtQuery", &a, name)
            .await
    }

    async fn stmt_exec(&mut self, st: &str, values: Vec<Option<Interface>>, step: &str) {
        let a = Z_DbStmtQueryArgs {
            a: st.to_owned(),
            b: args(values),
        };
        self.call::<_, Z_DbResultContErrReturn>(&format!("StmtExec {step}"), "StmtExec", &a)
            .await;
    }

    async fn close_conn(&mut self, conn: &str, step: &str) {
        self.call::<_, Z_DbErrReturn>(step, "ConnClose", &conn.to_owned())
            .await;
    }

    async fn run(&mut self) {
        let c1 = self.conn(true, "conn1").await;
        self.ping(&c1, "ConnPing conn1").await;

        // Every column type, NULLs, the precision of a numeric, and the end of the rows.
        let all = format!(
            "SELECT id, name, amount, data, flag, big, ratio, note, small FROM {T} ORDER BY id"
        );
        let r = self.query(&c1, &all, vec![], "rows1").await;
        self.columns(&r, "rows1").await;
        self.column_types(&r, 9, "rows1").await;
        self.drain(&r, 9, "rows1").await;
        self.result_sets(&r, "rows1").await;
        self.close_rows(&r, "rows1").await;

        // With arguments: the extended protocol, and binary result columns.
        let q =
            format!("SELECT id, name, data, big FROM {T} WHERE id = $1 OR name = $2 ORDER BY id");
        let r = self
            .query(&c1, &q, vec![i64v(1), strv("beta")], "rows2")
            .await;
        self.columns(&r, "rows2").await;
        self.column_types(&r, 4, "rows2").await;
        self.next(&r, vec![None; 2], "rows2 into two").await;
        self.next(
            &r,
            vec![
                None,
                strv("was"),
                None,
                None,
                strv("keep"),
                Some(Interface::int(7)),
            ],
            "rows2 into six",
        )
        .await;
        self.next(&r, vec![None; 4], "rows2 at the end").await;
        self.close_rows(&r, "rows2").await;

        // Writes, and the results lib/pq reports for them.
        self.exec(
            &c1,
            &format!(
                "INSERT INTO {T} (id, name, data, flag, big, ratio) VALUES ($1, $2, $3, $4, $5, $6)"
            ),
            vec![
                i64v(10),
                strv("ten"),
                bytesv(&[0, 1, 2]),
                Some(Interface::bool(true)),
                i64v(-5),
                Some(Interface::float64(2.5)),
            ],
        )
        .await;
        self.exec(
            &c1,
            &format!("INSERT INTO {T} (id, name, data, note) VALUES ($1, $2, $3, $4)"),
            vec![i64v(11), strv("eleven"), strv("\\x41 bin"), bytesv(&[])],
        )
        .await;
        self.exec(
            &c1,
            &format!("INSERT INTO {T} (id, name) VALUES ($1, 'twelve')"),
            vec![Some(Interface::int(12))],
        )
        .await;
        self.exec(&c1, "", vec![]).await;
        self.exec(&c1, "SELEC 1", vec![]).await;
        self.exec(
            &c1,
            &format!("SELECT 1 FROM {T} WHERE id = $1"),
            vec![i64v(1)],
        )
        .await;
        self.exec(
            &c1,
            &format!("UPDATE {T} SET note = 'u' WHERE id = 10; DELETE FROM {T} WHERE id = 11 AND note = 'none'"),
            vec![],
        )
        .await;
        self.exec(
            &c1,
            &format!("INSERT INTO {T} (id, name) VALUES (1, 'dup')"),
            vec![],
        )
        .await;
        let r = self
            .query(
                &c1,
                &format!("SELECT nosuch FROM {T}"),
                vec![],
                "rows-error",
            )
            .await;
        let _ = r;
        let r = self
            .query(
                &c1,
                &format!("SELECT data, note, name FROM {T} WHERE id IN (3, 10, 11) ORDER BY id"),
                vec![],
                "rows3",
            )
            .await;
        self.drain(&r, 3, "rows3").await;
        self.close_rows(&r, "rows3").await;

        // Several result sets, and an error in the second.
        let r = self
            .query(
                &c1,
                "SELECT 1 AS one; SELECT 'two' AS two, 3::int8 AS three",
                vec![],
                "rows4",
            )
            .await;
        self.next(&r, vec![None; 2], "rows4 first set").await;
        self.next(&r, vec![None; 2], "rows4 first set's end").await;
        self.result_sets(&r, "rows4").await;
        self.columns(&r, "rows4 second set").await;
        self.column_types(&r, 2, "rows4 second set").await;
        self.next(&r, vec![None; 2], "rows4 second set").await;
        self.next(&r, vec![None; 2], "rows4 second set's end").await;
        self.result_sets(&r, "rows4 at the end").await;
        self.close_rows(&r, "rows4").await;
        // Read past the first set without `NextResultSet`: the second set's row arrives under the
        // first set's columns, and once the rows are done no further set is reported.
        let r = self
            .query(&c1, "SELECT 1; SELECT 2", vec![], "rows4b")
            .await;
        self.next(&r, vec![None], "rows4b first set").await;
        self.next(&r, vec![None], "rows4b first set's end").await;
        self.next(&r, vec![None], "rows4b without NextResultSet")
            .await;
        self.next(&r, vec![None], "rows4b at the end").await;
        self.result_sets(&r, "rows4b").await;
        self.close_rows(&r, "rows4b").await;
        let r = self
            .query(&c1, "SELECT 1; SELECT nosuch", vec![], "rows5")
            .await;
        self.drain(&r, 1, "rows5").await;
        self.close_rows(&r, "rows5").await;
        let r = self.query(&c1, ";", vec![], "rows6").await;
        self.columns(&r, "rows6").await;
        self.drain(&r, 1, "rows6").await;
        self.close_rows(&r, "rows6").await;

        // Prepared statements.
        let s = self
            .stmt(
                &c1,
                &format!("SELECT name, amount FROM {T} WHERE id = $1"),
                "stmt1",
            )
            .await;
        self.call::<_, Z_DbIntReturn>("StmtNumInput stmt1", "StmtNumInput", &s)
            .await;
        let r = self.stmt_query(&s, vec![i64v(1)], "rows7").await;
        self.columns(&r, "rows7").await;
        self.column_types(&r, 2, "rows7").await;
        self.drain(&r, 2, "rows7").await;
        self.close_rows(&r, "rows7").await;
        self.stmt_exec(&s, vec![i64v(2)], "stmt1 as an exec").await;
        self.stmt_query(&s, vec![], "rows-wrong-arity").await;
        self.stmt_exec(&s, vec![i64v(1), i64v(2)], "stmt1 with two")
            .await;
        self.call::<_, Z_DbErrReturn>("StmtClose stmt1", "StmtClose", &s)
            .await;
        let s = self
            .stmt(
                &c1,
                &format!("UPDATE {T} SET note = $1 WHERE id >= $2"),
                "stmt2",
            )
            .await;
        self.call::<_, Z_DbIntReturn>("StmtNumInput stmt2", "StmtNumInput", &s)
            .await;
        self.stmt_exec(&s, vec![strv("upd"), i64v(10)], "stmt2")
            .await;
        self.stmt_exec(&s, vec![None, bytesv(b"x")], "stmt2 with bytes for an int")
            .await;
        self.call::<_, Z_DbErrReturn>("StmtClose stmt2", "StmtClose", &s)
            .await;
        self.stmt(&c1, "SELEC", "stmt-error").await;

        // Transactions: rolled back, read-only and failed, unsupported, committed.
        let count = format!("SELECT count(*) FROM {T} WHERE id >= 20");
        let t = self.tx(&c1, 0, false, "tx1").await;
        self.exec(
            &c1,
            &format!("INSERT INTO {T} (id, name) VALUES (20, 'in tx')"),
            vec![],
        )
        .await;
        let r = self.query(&c1, &count, vec![], "rows8").await;
        self.drain(&r, 1, "rows8").await;
        self.tx_end(&t, "TxRollback", "tx1").await;
        let r = self.query(&c1, &count, vec![], "rows9").await;
        self.drain(&r, 1, "rows9").await;
        let t = self.tx(&c1, 6, true, "tx2").await;
        self.exec(
            &c1,
            &format!("INSERT INTO {T} (id, name) VALUES (21, 'ro')"),
            vec![],
        )
        .await;
        self.exec(&c1, "SELECT 1", vec![]).await;
        self.tx_end(&t, "TxCommit", "tx2").await;
        self.tx(&c1, 7, false, "tx-unsupported").await;
        let t = self.tx(&c1, 1, false, "tx3").await;
        self.tx_end(&t, "TxCommit", "tx3").await;
        let t = self.tx(&c1, 4, false, "tx4").await;
        self.exec(
            &c1,
            &format!("INSERT INTO {T} (id, name) VALUES (22, 'kept')"),
            vec![],
        )
        .await;
        self.tx_end(&t, "TxCommit", "tx4").await;
        let t = self.tx(&c1, 2, false, "tx5").await;
        self.tx_end(&t, "TxRollback", "tx5").await;
        let r = self.query(&c1, &count, vec![], "rows10").await;
        self.drain(&r, 1, "rows10").await;

        // A query while another's rows are open, and rows closed part-way through.
        let r = self
            .query(
                &c1,
                &format!("SELECT id FROM {T} ORDER BY id"),
                vec![],
                "rows11",
            )
            .await;
        self.query(&c1, "SELECT 1", vec![], "rows-in-progress")
            .await;
        self.exec(&c1, "SELECT 1", vec![]).await;
        self.next(&r, vec![None], "rows11 first").await;
        self.close_rows(&r, "rows11 part-way").await;
        let r = self
            .query(
                &c1,
                &format!("SELECT name FROM {T} WHERE id = $1"),
                vec![],
                "rows-no-args",
            )
            .await;
        let _ = r;
        let a = Z_DbConnArgs {
            a: c1.clone(),
            b: format!("SELECT name FROM {T} WHERE id = $1"),
            c: vec![NamedValue {
                name: "id".into(),
                ordinal: 1,
                value: i64v(2),
            }],
        };
        let r = self
            .id_call(
                "ConnQuery rows12 with a named argument",
                "ConnQuery",
                &a,
                "rows12",
            )
            .await;
        self.drain(&r, 1, "rows12").await;

        // A replica connection, closed; then an id that is no longer there.
        let c3 = self.conn(false, "conn3").await;
        self.ping(&c3, "ConnPing conn3").await;
        self.close_conn(&c3, "ConnClose conn3").await;
        self.ping(&c3, "ConnPing conn3 after close").await;
        self.close_conn(&c3, "ConnClose conn3 again").await;

        // A connection lib/pq marks bad, which database/sql then closes.
        let c2 = self.conn(true, "conn2").await;
        self.tx(&c2, 0, false, "tx6").await;
        self.tx(&c2, 0, false, "tx7 inside tx6").await;
        self.ping(&c2, "ConnPing conn2 once bad").await;
        self.query(&c2, "SELECT 1", vec![], "rows-bad").await;
        self.ping(&c2, "ConnPing conn2 once closed").await;
        self.exec(&c2, "SELECT 2", vec![]).await;
        self.close_conn(&c2, "ConnClose conn2").await;
        self.close_conn(&c2, "ConnClose conn2 again").await;
        self.close_conn(&c1, "ConnClose conn1").await;

        // The pool: the suite runs both hosts with `MaxOpenConns` 4 and `QueryTimeout` 2, and
        // every connection above is closed. Four are handed out, the fifth waits out the timeout,
        // and a closed one is handed out again — with the session it had, so a `SET` made on it
        // is still there.
        let mut held = Vec::new();
        for i in 6..10 {
            held.push(self.conn(true, &format!("conn{i}")).await);
        }
        self.conn(true, "conn-over-the-bound").await;
        self.exec(&held[0], "SET application_name = 'mmrs-pooled'", vec![])
            .await;
        self.close_conn(&held[0], "ConnClose conn6").await;
        let again = self.conn(true, "conn10").await;
        let r = self
            .query(&again, "SHOW application_name", vec![], "rows-reused")
            .await;
        self.drain(&r, 1, "rows-reused").await;
        self.close_conn(&again, "ConnClose conn10").await;
        for (i, conn) in held.iter().enumerate().skip(1) {
            self.close_conn(conn, &format!("ConnClose conn{}", i + 6))
                .await;
        }

        // Last: a value gob cannot carry.
        let c5 = self.conn(true, "conn5").await;
        let r = self
            .query(
                &c5,
                &format!("SELECT created FROM {T} WHERE id = 1"),
                vec![],
                "rows13",
            )
            .await;
        self.next(&r, vec![None], "rows13 a timestamptz").await;
        self.ping(&c5, "ConnPing conn5 after the failed reply")
            .await;
        self.write(&json!({ "step": "done" }));
    }
}

impl Hooks for Script {
    fn implemented(&self) -> Vec<String> {
        vec!["OnActivate".to_owned()]
    }
}

impl HooksHttp for Script {}
impl HooksFileUpload for Script {}

impl Plugin for Script {
    fn set_api(&self, _: ApiClient, driver: DriverClient) {
        let _ = self.driver.set(driver);
    }

    async fn on_activate(&self) -> Result<Z_OnActivateReturns, NotImplemented> {
        if let Some(driver) = self.driver.get() {
            let mut run = Run {
                client: driver.client(),
                ids: Vec::new(),
                out: &self.transcript,
            };
            run.run().await;
        }
        Ok(Z_OnActivateReturns::default())
    }
}

#[tokio::main]
async fn main() {
    let path =
        std::env::var_os("DRIVER_SCRIPT_TRANSCRIPT").expect("DRIVER_SCRIPT_TRANSCRIPT is not set");
    let transcript = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open the transcript");
    let plugin = Script {
        transcript: Mutex::new(transcript),
        driver: OnceLock::new(),
    };
    if let Err(e) = client_main(plugin).await {
        eprintln!("driver script: {e}");
        std::process::exit(1);
    }
}
