//! Port of `SqlChannelMemberHistoryStore` (channels/store/sqlstore/channel_member_history_store.go),
//! the two logging writes only.
//!
//! # Why this table exists at all, and why the writes are invisible
//!
//! `ChannelMemberHistory` is the compliance-export audit trail: who was in a channel when. It is
//! written on **every** join and leave — `addUserToChannel` and `removeUserFromChannel` both do it
//! — and it appears in **no** response body, so a parity test that only compares HTTP answers
//! cannot see whether it happened. That is precisely why it is easy to drop from a port, and why
//! `tests/db_channel_member_history.rs` asserts the rows directly.
//!
//! # The asymmetry between the two writes is deliberate
//!
//! A join is a plain `INSERT` and any failure aborts the caller with a 500. A leave is an
//! `UPDATE … WHERE LeaveTime IS NULL` whose "nothing matched" case is **logged and ignored** —
//! Go's comment calls it best effort, because a user removed from a channel they were never
//! recorded as joining (a row planted before the feature, or a history row already closed) must
//! still be removable. Turning that warning into an error would make such a removal a 500.

use mm_model::channel_member_history::ChannelMemberHistory;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.ChannelMemberHistoryStore` (store/store.go) that is ported.
pub trait ChannelMemberHistoryStore {
    /// Port of `SqlChannelMemberHistoryStore.LogJoinEvent`
    /// (channel_member_history_store.go:41).
    fn log_join_event(
        &self,
        user_id: &str,
        channel_id: &str,
        join_time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelMemberHistoryStore.LogLeaveEvent`
    /// (channel_member_history_store.go:57).
    fn log_leave_event(
        &self,
        user_id: &str,
        channel_id: &str,
        leave_time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Not in Go: the open history rows for one membership, so a store test can assert the two
    /// writes above without reaching for a second database connection of its own.
    fn get_history_for_member(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelMemberHistory>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlChannelMemberHistoryStore {
    pool: PgPool,
}

impl SqlChannelMemberHistoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ChannelMemberHistoryStore for SqlChannelMemberHistoryStore {
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id))]
    async fn log_join_event(
        &self,
        user_id: &str,
        channel_id: &str,
        join_time: i64,
    ) -> Result<(), StoreError> {
        // The primary key is `(ChannelId, UserId, JoinTime)`, so two joins in the same
        // millisecond collide. Go has no `ON CONFLICT` here and neither does this: the caller's
        // own `GetMember` check makes a same-millisecond re-join unreachable through the API, and
        // swallowing the conflict would hide a double-add.
        sqlx::query!(
            r#"
            INSERT INTO channelmemberhistory (userid, channelid, jointime)
            VALUES ($1, $2, $3)
            "#,
            user_id,
            channel_id,
            join_time
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "LogJoinEvent userId={user_id} channelId={channel_id} joinTime={join_time}"
            ),
            source,
        })?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id, rows))]
    async fn log_leave_event(
        &self,
        user_id: &str,
        channel_id: &str,
        leave_time: i64,
    ) -> Result<(), StoreError> {
        // `LeaveTime IS NULL` is the whole predicate that makes this idempotent-ish: it closes
        // the *open* stay and leaves closed ones alone. Dropping it would rewrite every historical
        // row for this membership and destroy the audit trail the table exists for.
        let result = sqlx::query!(
            r#"
            UPDATE channelmemberhistory
               SET leavetime = $1
             WHERE userid = $2
               AND channelid = $3
               AND leavetime IS NULL
            "#,
            leave_time,
            user_id,
            channel_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "LogLeaveEvent userId={user_id} channelId={channel_id} leaveTime={leave_time}"
            ),
            source,
        })?;

        tracing::Span::current().record("rows", result.rows_affected());
        if result.rows_affected() != 1 {
            // Go's `mlog.Warn`, verbatim in effect: best effort, no error.
            tracing::warn!(
                user = %user_id,
                channel = %channel_id,
                "Channel join event for user and channel not found"
            );
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id))]
    async fn get_history_for_member(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> Result<Vec<ChannelMemberHistory>, StoreError> {
        sqlx::query_as!(
            ChannelMemberHistory,
            r#"
            SELECT channelid AS channel_id,
                   userid    AS user_id,
                   jointime  AS join_time,
                   leavetime AS leave_time
              FROM channelmemberhistory
             WHERE userid = $1
               AND channelid = $2
             ORDER BY jointime
            "#,
            user_id,
            channel_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to get ChannelMemberHistory with userId={user_id} channelId={channel_id}"
            ),
            source,
        })
    }
}
