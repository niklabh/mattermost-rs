//! Port of `SqlAuditStore` (channels/store/sqlstore/audit_store.go), `Get` only.
//!
//! Ported for `getUserAudits` (api4/user.go:2827) — the webapp's *View Access History* panel. The
//! write side (`Save`, `PermanentDeleteByUser`) is not ported: nothing migrated writes an audit
//! row, and `Audits` is append-only from the Go server's side of the shared database.

use mm_model::audit::{Audit, Audits};
use sqlx::PgPool;

use crate::error::StoreError;

/// `SqlAuditStore.Get`'s own bound (audit_store.go:54), **not** a page-size default.
///
/// Reached before any query runs and answered with `ErrOutOfBounds`, which the app layer turns
/// into a 400. Through `getUserAudits` it is unreachable — `web.ParamsFromRequest` clamps
/// `per_page` to 200 first — so it is ported for fidelity and the app layer's 400 has no test
/// through the route. See `mm_app::audit`.
pub const AUDIT_LIMIT_MAXIMUM: i64 = 1000;

/// The subset of Go's `store.AuditStore` (store/store.go) that is ported.
pub trait AuditStore {
    /// Port of `SqlAuditStore.Get` (audit_store.go:53).
    ///
    /// `Err` is `ErrOutOfBounds` when `limit` exceeds [`AUDIT_LIMIT_MAXIMUM`] — modelled as its
    /// own variant rather than as a database error, because the app layer branches on it and
    /// answers 400 rather than 500.
    fn get(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Audits, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlAuditStore {
    pool: PgPool,
}

impl SqlAuditStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `auditQuery` (audit_store.go:25-35).
///
/// Every column but `Id` is nullable in the schema while Go scans into plain `string`/`int64`, so
/// a NULL is a scan failure on Go's side too. `COALESCE` is applied anyway: a NULL here would make
/// *our* query fail where Go's fails, which is agreement on the error and not on the answer, and
/// the zero value is what Go's model would hold if it could scan one.
struct AuditRow {
    id: String,
    createat: i64,
    userid: String,
    action: String,
    extrainfo: String,
    ipaddress: String,
    sessionid: String,
}

impl From<AuditRow> for Audit {
    fn from(row: AuditRow) -> Self {
        Audit {
            id: row.id,
            create_at: row.createat,
            user_id: row.userid,
            action: row.action,
            extra_info: row.extrainfo,
            ip_address: row.ipaddress,
            session_id: row.sessionid,
        }
    }
}

impl AuditStore for SqlAuditStore {
    /// # `ORDER BY CreateAt DESC` and nothing else
    ///
    /// Go sorts on the millisecond timestamp alone (audit_store.go:59). Two rows written in the
    /// same millisecond — which a login does, twice, on every sign-in — have **no defined order**,
    /// and Postgres is free to return them either way to either server. Reproduced verbatim
    /// rather than stabilised with a tiebreak: adding `Id` here would make our order *more*
    /// defined than Go's, which is a divergence that only shows up as a flake somewhere else.
    /// The parity suite compares such a page as a set. See the note in `parity/user_audits.rs`.
    ///
    /// # The empty-`user_id` branch is not reproduced as a branch
    ///
    /// Go drops the `WHERE` entirely when `userId` is empty, returning **every** user's audits.
    /// `getUserAudits` calls `RequireUserId` first, so nothing reachable passes an empty id; the
    /// predicate is unconditional here and an empty id simply matches nothing. Turning "no filter"
    /// into "match nothing" is the safe direction for a table holding other users' IP addresses.
    #[tracing::instrument(skip_all, fields(user_id, offset, limit, found))]
    async fn get(&self, user_id: &str, offset: i64, limit: i64) -> Result<Audits, StoreError> {
        if limit > AUDIT_LIMIT_MAXIMUM {
            return Err(StoreError::OutOfBounds { limit });
        }

        let rows = sqlx::query_as!(
            AuditRow,
            r#"
            SELECT id                        AS "id!",
                   COALESCE(createat, 0)     AS "createat!",
                   COALESCE(userid, '')      AS "userid!",
                   COALESCE(action, '')      AS "action!",
                   COALESCE(extrainfo, '')   AS "extrainfo!",
                   COALESCE(ipaddress, '')   AS "ipaddress!",
                   COALESCE(sessionid, '')   AS "sessionid!"
              FROM audits
             WHERE userid = $1
             ORDER BY createat DESC
             LIMIT $2 OFFSET $3
            "#,
            user_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Audit list for userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(Audits(rows.into_iter().map(Audit::from).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `limit > 1000` guard, without a database.
    ///
    /// `connect_lazy` builds a pool that opens no connection until a query runs, and the guard
    /// returns before any query does — so this exercises the branch itself rather than a
    /// round trip. It needs to be here: the bound is **unreachable through `getUserAudits`**,
    /// because `web.ParamsFromRequest` clamps `per_page` to 200 long before the store sees it,
    /// so no parity test can reach it and a mutation of it survives the whole `api` suite.
    #[tokio::test]
    async fn a_limit_above_the_maximum_is_refused_before_any_query() {
        // The timeout is capped deliberately. sqlx's default `acquire_timeout` is **30 seconds**,
        // and the second assertion below waits out a connection that can never succeed — at the
        // default this one test costs half a minute, which is exactly the class of bug
        // `CLAUDE.md` records having found six of.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://unreachable:unreachable@127.0.0.1:1/none")
            .expect("a lazy pool needs no server");
        let store = SqlAuditStore::new(pool);

        let err = store
            .get("6rtg4qbe5bn55mw5t6gphxyaxa", 0, AUDIT_LIMIT_MAXIMUM + 1)
            .await
            .expect_err("a limit above the maximum is refused");
        assert!(
            err.is_out_of_bounds(),
            "and it is `ErrOutOfBounds`, which the app layer answers 400 to: {err:?}"
        );

        // The bound itself is inclusive: 1000 is fine and only 1001 is not. Reaching the database
        // is what proves the guard let it past, so the error here is a *connection* failure.
        let err = store
            .get("6rtg4qbe5bn55mw5t6gphxyaxa", 0, AUDIT_LIMIT_MAXIMUM)
            .await
            .expect_err("no server is listening");
        assert!(
            !err.is_out_of_bounds(),
            "`limit > MAXIMUM` is strict, so exactly the maximum passes the guard: {err:?}"
        );
    }

    /// Every persisted column lands on its field, in Go's own column order.
    #[test]
    fn a_row_maps_onto_the_model() {
        let audit = Audit::from(AuditRow {
            id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            createat: 1_701_355_039_000,
            userid: "6rtg4qbe5bn55mw5t6gphxyaxa".to_owned(),
            action: "/api/v4/users/login".to_owned(),
            extrainfo: "success session_user=6rtg4qbe5bn55mw5t6gphxyaxa".to_owned(),
            ipaddress: "172.18.0.1".to_owned(),
            sessionid: "t6cgb45btfbjxxt4z1ob7hknir".to_owned(),
        });

        assert_eq!(audit.id, "y9i4er48tt8bukijy7i3u5y9ar");
        assert_eq!(audit.create_at, 1_701_355_039_000);
        assert_eq!(audit.user_id, "6rtg4qbe5bn55mw5t6gphxyaxa");
        assert_eq!(audit.action, "/api/v4/users/login");
        assert_eq!(
            audit.extra_info,
            "success session_user=6rtg4qbe5bn55mw5t6gphxyaxa"
        );
        assert_eq!(audit.ip_address, "172.18.0.1");
        assert_eq!(audit.session_id, "t6cgb45btfbjxxt4z1ob7hknir");
    }
}
