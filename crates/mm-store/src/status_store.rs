//! Port of `SqlStatusStore` (channels/store/sqlstore/status_store.go), `GetByIds` only.
//!
//! Ported for `getUserStatus` and `getUserStatusesByIds` (api4/status.go), which both go through
//! `PlatformService.GetUserStatusesByIds` and therefore through this one query. `Get` is not
//! ported: over REST it is reached only from `updateUserStatus` (the PUT), which is not migrated.

use mm_model::status::Status;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.StatusStore` (store/store.go) that is ported.
pub trait StatusStore {
    /// Port of `SqlStatusStore.GetByIds` (status_store.go:123).
    fn get_by_ids(
        &self,
        user_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Status>, StoreError>> + Send;

    /// Port of `SqlStatusStore.Get` (status_store.go).
    ///
    /// `Ok(None)` is Go's `ErrNotFound`, which `GetStatus` renders as
    /// `app.status.get.missing.app_error` at **404** — a shape no other status path produces.
    fn get(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<Status>, StoreError>> + Send;

    /// Port of `SqlStatusStore.SaveOrUpdate` (status_store.go).
    ///
    /// **`ActiveChannel` is not a column.** `statusSliceColumns` is `UserId, Status, Manual,
    /// LastActivityAt, DNDEndTime, PrevStatus` — the model's `active_channel` carries `db:"-"` and
    /// lives only in the cache, which is why a status read back from the table never has one.
    fn save_or_update(
        &self,
        status: &Status,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// One row of Go's `statusSelectQuery` (status_store.go:37-46).
///
/// Every column but `UserId` is nullable in the schema, and Go wraps each one in `COALESCE` with
/// its zero value — so a NULL never reaches the scanner there, and never reaches the model here.
/// The `COALESCE`s are reproduced in the SQL rather than applied on the Rust side so that a
/// future query copied from this one inherits them.
struct StatusRow {
    userid: String,
    status: String,
    manual: bool,
    lastactivityat: i64,
    dndendtime: i64,
    prevstatus: String,
}

impl From<StatusRow> for Status {
    fn from(row: StatusRow) -> Self {
        Status {
            user_id: row.userid,
            status: row.status,
            manual: row.manual,
            last_activity_at: row.lastactivityat,
            // `db:"-"` in Go: the column does not exist, so a status read from the database
            // always has it empty — which `omitempty` then keeps off the wire.
            active_channel: String::new(),
            dnd_end_time: row.dndendtime,
            prev_status: row.prevstatus,
        }
    }
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlStatusStore {
    pool: PgPool,
}

impl SqlStatusStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl StatusStore for SqlStatusStore {
    #[tracing::instrument(skip(self), fields(user_id = %user_id, found))]
    async fn get(&self, user_id: &str) -> Result<Option<Status>, StoreError> {
        let row = sqlx::query_as!(
            StatusRow,
            r#"
            SELECT userid                          AS "userid!",
                   COALESCE(status, '')            AS "status!",
                   COALESCE(manual, false)         AS "manual!",
                   COALESCE(lastactivityat, 0)     AS "lastactivityat!",
                   COALESCE(dndendtime, 0)         AS "dndendtime!",
                   COALESCE(prevstatus, '')        AS "prevstatus!"
              FROM status
             WHERE userid = $1
            "#,
            user_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Status with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());
        Ok(row.map(Status::from))
    }

    /// The conflict clause updates **five** columns and not `UserId`, which is the key.
    #[tracing::instrument(skip(self, status), fields(user_id = %status.user_id, status = %status.status))]
    async fn save_or_update(&self, status: &Status) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            INSERT INTO status (userid, status, manual, lastactivityat, dndendtime, prevstatus)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (userid)
                DO UPDATE SET status         = EXCLUDED.status,
                              manual         = EXCLUDED.manual,
                              lastactivityat = EXCLUDED.lastactivityat,
                              dndendtime     = EXCLUDED.dndendtime,
                              prevstatus     = EXCLUDED.prevstatus
            "#,
            status.user_id,
            status.status,
            status.manual,
            status.last_activity_at,
            status.dnd_end_time,
            status.prev_status,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to upsert Status".to_owned(),
            source,
        })?;
        Ok(())
    }

    /// `sq.Eq{"UserId": userIds}` renders as `UserId IN (...)`; `= ANY($1)` is the same predicate
    /// with one bind. There is **no `ORDER BY`** in Go and none here: the app layer owns the
    /// order it puts on the wire (see `mm_app::status`).
    ///
    /// An empty `user_ids` is answered without a round trip. squirrel renders `sq.Eq` against an
    /// empty slice as `(1=0)`, so Go's query returns no rows too; the result is the same and the
    /// platform layer never asks with an empty list anyway.
    #[tracing::instrument(skip_all, fields(asked = user_ids.len(), found))]
    async fn get_by_ids(&self, user_ids: &[String]) -> Result<Vec<Status>, StoreError> {
        if user_ids.is_empty() {
            tracing::Span::current().record("found", 0);
            return Ok(Vec::new());
        }

        // `COALESCE(x, zero) AS x` — the `!` tells sqlx the column is non-null, which is what the
        // coalesce guarantees; the `SELECT` is Go's verbatim apart from the bind.
        let rows = sqlx::query_as!(
            StatusRow,
            r#"
            SELECT COALESCE(userid, '')        AS "userid!",
                   COALESCE(status, '')        AS "status!",
                   COALESCE(manual, FALSE)     AS "manual!",
                   COALESCE(lastactivityat, 0) AS "lastactivityat!",
                   COALESCE(dndendtime, 0)     AS "dndendtime!",
                   COALESCE(prevstatus, '')    AS "prevstatus!"
              FROM status
             WHERE userid = ANY($1)
            "#,
            user_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Statuses".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(Status::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row-to-model mapping: every persisted column lands on its field, and
    /// `active_channel` — which has no column — is empty, so serialising the result omits it.
    #[test]
    fn a_row_maps_onto_the_model_with_no_active_channel() {
        let status = Status::from(StatusRow {
            userid: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            status: "dnd".to_owned(),
            manual: true,
            lastactivityat: 1_701_355_039_000,
            dndendtime: 1_701_358_639,
            prevstatus: "online".to_owned(),
        });

        assert_eq!(status.user_id, "y9i4er48tt8bukijy7i3u5y9ar");
        assert_eq!(status.status, "dnd");
        assert!(status.manual);
        assert_eq!(status.last_activity_at, 1_701_355_039_000);
        assert_eq!(status.dnd_end_time, 1_701_358_639);
        assert_eq!(status.prev_status, "online");
        assert_eq!(status.active_channel, "");

        let json = serde_json::to_string(&status).expect("serialises");
        assert!(
            !json.contains("active_channel"),
            "db:\"-\" plus omitempty: never on the wire from a database read — {json}"
        );
        assert!(!json.contains("prev_status"), "json:\"-\" — {json}");
    }
}
