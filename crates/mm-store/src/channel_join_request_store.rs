//! Port of `SqlChannelJoinRequestStore` (channels/store/sqlstore/channel_join_request_store.go) —
//! the whole file, all seven methods.
//!
//! Behind the seven routes of `api4/channel_join_request.go`: a user's request to join a
//! *discoverable* private channel, and the admin review of it.
//!
//! # `ChannelJoinRequests` is a real table, and the partial index is the interesting part
//!
//! Migrations `000180`-`000183` create it on the development database, so every query here is
//! compile-time checked by sqlx like any other. The *routes* are dark
//! (`FeatureFlags.DiscoverableChannels` is false at the pinned SHA — see
//! [`mm_api::channel_join_requests`]), but the schema is not.
//!
//! `idx_channeljoinrequests_pending_unique` is `UNIQUE (ChannelId, UserId) WHERE Status =
//! 'pending'`. It is the *only* thing that stops a user queueing twice, so [`ChannelJoinRequestStore::save`] has no
//! pre-read and instead translates the unique violation into [`StoreError::Conflict`]. A port that
//! checked first and inserted second would have a race the Go one does not.
//!
//! # Rows are never deleted
//!
//! There is no `Delete` in the interface. `pending → approved | denied | withdrawn` is the whole
//! lifecycle and the row stays, which is why `Status` is in the index and why
//! [`ChannelJoinRequestStore::get_pending_for_channel_and_user`] names the status in its predicate
//! rather than assuming the newest row is the live one.
//!
//! # The pagination clamp exists **twice**, deliberately
//!
//! `app.sanitizeJoinRequestListOpts` normalises the caller's page before the store sees it, and
//! [`paginate`] normalises it again (`PerPage <= 0 → 60`, `Page < 0 → 0`). Go has both. Dropping
//! the store's copy would be invisible through the API and would change what a direct store caller
//! gets, so it is ported where Go put it.

use mm_model::channel_join_request::{
    CHANNEL_JOIN_REQUEST_STATUS_PENDING, ChannelJoinRequest, GetChannelJoinRequestsOpts,
};
use sqlx::PgPool;

use crate::error::StoreError;

/// `store/store.go:1403` — `ChannelJoinRequestStore`, all seven methods.
pub trait ChannelJoinRequestStore {
    /// Port of `SqlChannelJoinRequestStore.Save` (channel_join_request_store.go:63).
    ///
    /// Takes `&mut` because Go's `Save` mutates its argument: `PreSave` mints the id, the status
    /// and both timestamps in place and the returned pointer is the same object. A caller that
    /// ignored the mutation would broadcast an event carrying an empty id.
    ///
    /// `PreSave` runs **before** `IsValid`, so a request with no id is valid by the time it is
    /// checked. The insert is bare — no `ON CONFLICT` — because the conflict is the signal: the
    /// app layer reads the caller's existing pending row and answers 201 with it.
    fn save(
        &self,
        req: &mut ChannelJoinRequest,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelJoinRequestStore.Get` (channel_join_request_store.go:86).
    ///
    /// By id alone, **any** status — the review endpoints look rows up this way and then check the
    /// status themselves, so a withdrawn row is found here and refused there.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelJoinRequest, StoreError>> + Send;

    /// Port of `SqlChannelJoinRequestStore.GetPendingForChannelAndUser`
    /// (channel_join_request_store.go:99).
    ///
    /// `Status = 'pending'` is in the predicate, so this can return at most one row by the partial
    /// unique index — there is no `ORDER BY` and none is needed.
    fn get_pending_for_channel_and_user(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelJoinRequest, StoreError>> + Send;

    /// Port of `SqlChannelJoinRequestStore.GetForChannel` (channel_join_request_store.go:133).
    ///
    /// Returns the page **and the unpaginated total** for the same predicate — Go builds both from
    /// one `where` so they cannot drift. The list query runs first, so a driver failure on it is
    /// reported before the count is attempted.
    fn get_for_channel(
        &self,
        channel_id: &str,
        opts: &GetChannelJoinRequestsOpts,
    ) -> impl std::future::Future<Output = Result<(Vec<ChannelJoinRequest>, i64), StoreError>> + Send;

    /// Port of `SqlChannelJoinRequestStore.GetForUser` (channel_join_request_store.go:159).
    /// Identical to [`ChannelJoinRequestStore::get_for_channel`] with `UserId` as the scope column.
    fn get_for_user(
        &self,
        user_id: &str,
        opts: &GetChannelJoinRequestsOpts,
    ) -> impl std::future::Future<Output = Result<(Vec<ChannelJoinRequest>, i64), StoreError>> + Send;

    /// Port of `SqlChannelJoinRequestStore.Update` (channel_join_request_store.go:188).
    ///
    /// Six columns. The four it does **not** write are the point: `Id`, `ChannelId`, `UserId` and
    /// `CreateAt` are immutable once saved, because the partial unique index relies on
    /// `(ChannelId, UserId)` being stable for the lifetime of a row.
    ///
    /// Zero rows affected is a [`StoreError::NotFound`], not a silent success.
    fn update(
        &self,
        req: &mut ChannelJoinRequest,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelJoinRequestStore.CountPending` (channel_join_request_store.go:224).
    ///
    /// Pending only, and the status is a literal rather than `opts.Status` — this backs the
    /// channel-header badge, which counts one thing.
    fn count_pending(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlChannelJoinRequestStore {
    pool: PgPool,
}

impl SqlChannelJoinRequestStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Port of `applyJoinRequestStatusFilter` (channel_join_request_store.go:119).
///
/// **An empty status means `pending`, not "any status".** The app layer already applied the same
/// default *and* rejected unrecognised values; this one is the store's own floor and only ever
/// sees an empty string through a direct caller.
fn status_filter(opts: &GetChannelJoinRequestsOpts) -> &str {
    if opts.status.is_empty() {
        CHANNEL_JOIN_REQUEST_STATUS_PENDING
    } else {
        &opts.status
    }
}

/// Port of `paginate` (channel_join_request_store.go:127) — `(limit, offset)`.
///
/// `PerPage <= 0` takes 60, **not** the api4 maximum of 200, and a negative page is clamped to the
/// first one. Go computes the offset as `uint64(page) * uint64(perPage)` after that clamp, so it
/// cannot be negative here either.
fn paginate(opts: &GetChannelJoinRequestsOpts) -> (i64, i64) {
    let per_page = if opts.per_page <= 0 {
        60
    } else {
        opts.per_page
    };
    let page = opts.page.max(0);
    (per_page, page.saturating_mul(per_page))
}

/// One row of `channelJoinRequestColumns` (channel_join_request_store.go:17).
///
/// Every column is `NOT NULL` in the migration, so this is the model's own shape — there is no
/// nullable column to default, unlike most ported stores.
struct ChannelJoinRequestRow {
    id: String,
    channel_id: String,
    user_id: String,
    message: String,
    status: String,
    denial_reason: String,
    create_at: i64,
    update_at: i64,
    reviewed_by: String,
    reviewed_at: i64,
}

impl From<ChannelJoinRequestRow> for ChannelJoinRequest {
    fn from(row: ChannelJoinRequestRow) -> Self {
        ChannelJoinRequest {
            id: row.id,
            channel_id: row.channel_id,
            user_id: row.user_id,
            message: row.message,
            status: row.status,
            denial_reason: row.denial_reason,
            create_at: row.create_at,
            update_at: row.update_at,
            reviewed_by: row.reviewed_by,
            reviewed_at: row.reviewed_at,
        }
    }
}

/// `IsUniqueConstraintError(err, []string{"idx_channeljoinrequests_pending_unique"})`
/// (channel_join_request_store.go:75) — matched on the **constraint name**, so a violation of the
/// primary key (a caller supplying an id that already exists) is *not* a conflict here and stays a
/// 500, exactly as in Go.
fn is_pending_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.constraint())
        .is_some_and(|name| name == "idx_channeljoinrequests_pending_unique")
}

impl ChannelJoinRequestStore for SqlChannelJoinRequestStore {
    #[tracing::instrument(skip(self, req), fields(channel_id = %req.channel_id, user_id = %req.user_id))]
    async fn save(&self, req: &mut ChannelJoinRequest) -> Result<(), StoreError> {
        req.pre_save();

        // Go returns the `*model.AppError` from `IsValid` straight out of `Save`, and the app
        // layer passes it through with `errors.As` — so the id and the 400 survive to the client.
        req.is_valid().map_err(|app_error| StoreError::Invalid {
            entity: "ChannelJoinRequest",
            app_error,
        })?;

        sqlx::query!(
            r#"
            INSERT INTO channeljoinrequests
                (id, channelid, userid, message, status, denialreason,
                 createat, updateat, reviewedby, reviewedat)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
            req.id,
            req.channel_id,
            req.user_id,
            req.message,
            req.status,
            req.denial_reason,
            req.create_at,
            req.update_at,
            req.reviewed_by,
            req.reviewed_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| {
            if is_pending_unique_violation(&source) {
                // `store.NewErrConflict("ChannelJoinRequest", err, …)` — the app layer reads the
                // caller's existing pending row rather than failing.
                StoreError::Conflict {
                    resource: "ChannelJoinRequest",
                    source,
                }
            } else {
                StoreError::Db {
                    context: "failed to save ChannelJoinRequest".to_owned(),
                    source,
                }
            }
        })?;

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(request_id = %id, found))]
    async fn get(&self, id: &str) -> Result<ChannelJoinRequest, StoreError> {
        let row = sqlx::query_as!(
            ChannelJoinRequestRow,
            r#"
            SELECT id           AS "id!",
                   channelid    AS "channel_id!",
                   userid       AS "user_id!",
                   message      AS "message!",
                   status       AS "status!",
                   denialreason AS "denial_reason!",
                   createat     AS "create_at!",
                   updateat     AS "update_at!",
                   reviewedby   AS "reviewed_by!",
                   reviewedat   AS "reviewed_at!"
              FROM channeljoinrequests
             WHERE id = $1
            "#,
            id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get ChannelJoinRequest with id={id}"),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());
        row.map(Into::into).ok_or_else(|| StoreError::NotFound {
            entity: "ChannelJoinRequest",
            criteria: format!("id={id}"),
        })
    }

    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, user_id = %user_id, found))]
    async fn get_pending_for_channel_and_user(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<ChannelJoinRequest, StoreError> {
        let row = sqlx::query_as!(
            ChannelJoinRequestRow,
            r#"
            SELECT id           AS "id!",
                   channelid    AS "channel_id!",
                   userid       AS "user_id!",
                   message      AS "message!",
                   status       AS "status!",
                   denialreason AS "denial_reason!",
                   createat     AS "create_at!",
                   updateat     AS "update_at!",
                   reviewedby   AS "reviewed_by!",
                   reviewedat   AS "reviewed_at!"
              FROM channeljoinrequests
             WHERE channelid = $1 AND userid = $2 AND status = $3
            "#,
            channel_id,
            user_id,
            CHANNEL_JOIN_REQUEST_STATUS_PENDING,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to get pending ChannelJoinRequest for channel_id={channel_id} user_id={user_id}"
            ),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());
        row.map(Into::into).ok_or_else(|| StoreError::NotFound {
            entity: "ChannelJoinRequest",
            criteria: format!("channel_id={channel_id} user_id={user_id}"),
        })
    }

    #[tracing::instrument(skip(self, opts), fields(channel_id = %channel_id, found))]
    async fn get_for_channel(
        &self,
        channel_id: &str,
        opts: &GetChannelJoinRequestsOpts,
    ) -> Result<(Vec<ChannelJoinRequest>, i64), StoreError> {
        let status = status_filter(opts);
        let (limit, offset) = paginate(opts);

        let rows = sqlx::query_as!(
            ChannelJoinRequestRow,
            r#"
            SELECT id           AS "id!",
                   channelid    AS "channel_id!",
                   userid       AS "user_id!",
                   message      AS "message!",
                   status       AS "status!",
                   denialreason AS "denial_reason!",
                   createat     AS "create_at!",
                   updateat     AS "update_at!",
                   reviewedby   AS "reviewed_by!",
                   reviewedat   AS "reviewed_at!"
              FROM channeljoinrequests
             WHERE channelid = $1 AND status = $2
             ORDER BY createat DESC, id DESC
             LIMIT $3 OFFSET $4
            "#,
            channel_id,
            status,
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to list ChannelJoinRequests for channel_id={channel_id}"),
            source,
        })?;

        let total = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM channeljoinrequests WHERE channelid = $1 AND status = $2"#,
            channel_id,
            status,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count ChannelJoinRequests for channel_id={channel_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok((rows.into_iter().map(Into::into).collect(), total))
    }

    #[tracing::instrument(skip(self, opts), fields(user_id = %user_id, found))]
    async fn get_for_user(
        &self,
        user_id: &str,
        opts: &GetChannelJoinRequestsOpts,
    ) -> Result<(Vec<ChannelJoinRequest>, i64), StoreError> {
        let status = status_filter(opts);
        let (limit, offset) = paginate(opts);

        let rows = sqlx::query_as!(
            ChannelJoinRequestRow,
            r#"
            SELECT id           AS "id!",
                   channelid    AS "channel_id!",
                   userid       AS "user_id!",
                   message      AS "message!",
                   status       AS "status!",
                   denialreason AS "denial_reason!",
                   createat     AS "create_at!",
                   updateat     AS "update_at!",
                   reviewedby   AS "reviewed_by!",
                   reviewedat   AS "reviewed_at!"
              FROM channeljoinrequests
             WHERE userid = $1 AND status = $2
             ORDER BY createat DESC, id DESC
             LIMIT $3 OFFSET $4
            "#,
            user_id,
            status,
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to list ChannelJoinRequests for user_id={user_id}"),
            source,
        })?;

        let total = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM channeljoinrequests WHERE userid = $1 AND status = $2"#,
            user_id,
            status,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count ChannelJoinRequests for user_id={user_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok((rows.into_iter().map(Into::into).collect(), total))
    }

    #[tracing::instrument(skip(self, req), fields(request_id = %req.id))]
    async fn update(&self, req: &mut ChannelJoinRequest) -> Result<(), StoreError> {
        req.pre_update();

        req.is_valid().map_err(|app_error| StoreError::Invalid {
            entity: "ChannelJoinRequest",
            app_error,
        })?;

        let affected = sqlx::query!(
            r#"
            UPDATE channeljoinrequests
               SET status = $1, message = $2, denialreason = $3,
                   updateat = $4, reviewedby = $5, reviewedat = $6
             WHERE id = $7
            "#,
            req.status,
            req.message,
            req.denial_reason,
            req.update_at,
            req.reviewed_by,
            req.reviewed_at,
            req.id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update ChannelJoinRequest with id={}", req.id),
            source,
        })?
        .rows_affected();

        if affected == 0 {
            return Err(StoreError::NotFound {
                entity: "ChannelJoinRequest",
                criteria: format!("id={}", req.id),
            });
        }

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, count))]
    async fn count_pending(&self, channel_id: &str) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM channeljoinrequests WHERE channelid = $1 AND status = $2"#,
            channel_id,
            CHANNEL_JOIN_REQUEST_STATUS_PENDING,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to count pending ChannelJoinRequests for channel_id={channel_id}"
            ),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(status: &str, page: i64, per_page: i64) -> GetChannelJoinRequestsOpts {
        GetChannelJoinRequestsOpts {
            status: status.to_owned(),
            page,
            per_page,
        }
    }

    #[test]
    fn an_empty_status_filters_on_pending_and_a_named_one_is_kept_verbatim() {
        assert_eq!(status_filter(&opts("", 0, 0)), "pending");
        assert_eq!(status_filter(&opts("denied", 0, 0)), "denied");
        // The store does **not** validate: an unrecognised status reaches the query and matches
        // nothing. `sanitizeJoinRequestListOpts` is where the rejection lives.
        assert_eq!(status_filter(&opts("bogus", 0, 0)), "bogus");
    }

    #[test]
    fn a_non_positive_per_page_takes_sixty_and_not_the_api_maximum() {
        assert_eq!(paginate(&opts("", 0, 0)), (60, 0));
        assert_eq!(paginate(&opts("", 0, -5)), (60, 0));
        assert_eq!(paginate(&opts("", 2, 0)), (60, 120));
    }

    #[test]
    fn a_negative_page_becomes_the_first_page() {
        assert_eq!(paginate(&opts("", -1, 10)), (10, 0));
        assert_eq!(paginate(&opts("", 0, 10)), (10, 0));
        assert_eq!(paginate(&opts("", 3, 10)), (10, 30));
    }

    #[test]
    fn the_store_does_not_clamp_per_page_to_the_api_maximum() {
        // Go's `paginate` has no upper bound — 200 is `sanitizeJoinRequestListOpts`' cap, at the
        // app layer. A store caller really can ask for a thousand.
        assert_eq!(paginate(&opts("", 0, 1000)), (1000, 0));
    }
}
