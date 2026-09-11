//! Port of `SqlViewStore` (channels/store/sqlstore/view_store.go) — the whole file.
//!
//! Behind the seven routes of `api4/view.go`, the integrated-boards ("kanban view") surface.
//!
//! # `Views` is a real table, and this was worth checking before writing a line
//!
//! Migration `000166_create_views.up.sql` creates it and `000167` adds
//! `idx_views_channel_id_delete_at`; both are applied on the development database, so every query
//! here is compile-time checked by sqlx like any other. The *routes* are dark
//! (`FeatureFlags.IntegratedBoards` is false at the pinned SHA — see [`mm_api::views`]), but the
//! schema is not.
//!
//! # Three columns whose Postgres type is narrower than the model's
//!
//! - **`SortOrder` is `INTEGER`** while `model.View.SortOrder` is Go's `int`, i.e. 64-bit. Go
//!   hands the 64-bit value to the driver and Postgres refuses anything that does not fit, so a
//!   `sort_order` above 2^31-1 is a **write failure**, not a truncation. [`sort_order_column`]
//!   reproduces that as a refusal before the query rather than after it — same outcome, same
//!   500, one round trip fewer.
//! - **`Description` is nullable `TEXT`** and Go writes `""`, never `NULL`. A `NULL` would fail
//!   Go's scan into `string`; nothing reachable through the API can put one there, so this reads
//!   it as `Option<String>` and defaults — see [`row_to_view`], which says what that diverges
//!   from.
//! - **`Props` is nullable `jsonb`** and the model wants an object. `IsValid` refuses a kanban
//!   view without props, so every row this store writes has one.
//!
//! # The sort-order rewrite is a list operation pretending to be SQL
//!
//! [`ViewStore::update_sort_order`] reads the channel's views in their canonical order, moves one
//! element, and writes **every** row's new index back. Go builds a `CASE WHEN Id = ? THEN n`
//! ladder; the same effect is expressed here as a join against two `unnest`ed arrays, which keeps
//! the statement static and therefore checked. The ladder's semantics — one `UPDATE`, one
//! `UpdateAt` shared by every row — are preserved exactly.

use mm_model::utils::StringInterface;
use mm_model::view::{VIEW_QUERY_DEFAULT_PER_PAGE, VIEW_QUERY_MAX_PER_PAGE, View, ViewQueryOpts};
use sqlx::PgPool;

use crate::error::StoreError;

/// Port of `store.ViewStore` (store/store.go) — all seven methods, which is the whole interface.
pub trait ViewStore {
    /// Port of `SqlViewStore.Save` (view_store.go:43) via `saveViewT`.
    ///
    /// Takes `&mut View` because Go's `Save` mutates its argument: `PreSave` mints the id and the
    /// timestamps in place, and the returned pointer is the same object. A caller that ignored
    /// the mutation would publish a websocket event carrying an empty id.
    fn save(
        &self,
        view: &mut View,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlViewStore.Get` (view_store.go:76).
    ///
    /// `DeleteAt = 0` is part of the predicate, so a soft-deleted view is a miss rather than a
    /// row with `delete_at` set — there is no "include deleted" spelling anywhere in this store.
    fn get(&self, id: &str) -> impl std::future::Future<Output = Result<View, StoreError>> + Send;

    /// Port of `SqlViewStore.GetForChannel` (view_store.go:89).
    fn get_for_channel(
        &self,
        channel_id: &str,
        opts: &ViewQueryOpts,
    ) -> impl std::future::Future<Output = Result<Vec<View>, StoreError>> + Send;

    /// Port of `SqlViewStore.CountForChannel` (view_store.go:122).
    ///
    /// `opts` is accepted and **never read** — Go takes it and builds an unpaginated `COUNT(*)`.
    /// Kept in the signature so the call sites match, and so nobody "fixes" the count by
    /// applying the caller's page to it.
    fn count_for_channel(
        &self,
        channel_id: &str,
        opts: &ViewQueryOpts,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlViewStore.Update` (view_store.go:140).
    ///
    /// Five columns, and the two it does **not** write are the point: `Type` and `CreatorId` are
    /// immutable once saved, so a patch cannot turn a kanban board into something else or
    /// reassign its author.
    fn update(
        &self,
        view: &mut View,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlViewStore.Delete` (view_store.go:171) — a **soft** delete.
    ///
    /// `UpdateAt` is set to the same instant as `DeleteAt`, and the row is matched on
    /// `DeleteAt = 0`, so deleting twice is a not-found rather than a no-op.
    fn delete(
        &self,
        view_id: &str,
        delete_at: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlViewStore.UpdateSortOrder` (view_store.go:194).
    ///
    /// Returns the channel's views in their **new** order, each carrying its new `sort_order` and
    /// a shared `update_at` — which is what the handler encodes and what the `view_sorted`
    /// websocket event carries.
    fn update_sort_order(
        &self,
        view_id: &str,
        channel_id: &str,
        new_index: i64,
    ) -> impl std::future::Future<Output = Result<Vec<View>, StoreError>> + Send;
}

#[derive(Debug, Clone)]
pub struct SqlViewStore {
    pool: PgPool,
}

impl SqlViewStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `viewColumns()` (view_store.go:36), as Postgres hands it back.
struct ViewRow {
    id: String,
    channel_id: String,
    view_type: String,
    creator_id: String,
    title: String,
    description: Option<String>,
    sort_order: i32,
    props: Option<serde_json::Value>,
    create_at: i64,
    update_at: i64,
    delete_at: i64,
}

/// `Views.SortOrder` is `INTEGER`; `model.View.SortOrder` is 64-bit.
///
/// Go passes the wide value straight to the driver, so an out-of-range `sort_order` is refused by
/// Postgres with `numeric field overflow` and becomes a 500 at the API edge. Refusing it here
/// produces the same 500 from the same caller without the round trip. The value **is** reachable:
/// nothing between `createView`'s JSON decode and the insert bounds it.
fn sort_order_column(sort_order: i64) -> Result<i32, StoreError> {
    i32::try_from(sort_order).map_err(|_| StoreError::Argument {
        entity: "View",
        detail: "SortOrder does not fit the INTEGER column",
    })
}

/// Assemble a [`View`] from a row.
///
/// # Two nullable columns, and what defaulting them diverges from
///
/// `Description` and `Props` are nullable in the schema and never written as `NULL` by this store
/// or by Go's. Go scans them into `string` and `StringInterface`, where a `NULL` description
/// would be a **scan error** — a 500 — and here it is `""`. The divergence is unreachable through
/// the API (every write goes through `IsValid`, which requires a non-nil props map, and through
/// an insert that writes `""` for an absent description), and the alternative is an error branch
/// no test can reach. Recorded rather than hidden.
///
/// A `props` value that is valid JSON but not an object is treated the same way. Only a writer
/// outside both servers can produce one.
fn row_to_view(row: ViewRow) -> View {
    View {
        id: row.id,
        channel_id: row.channel_id,
        view_type: row.view_type,
        creator_id: row.creator_id,
        title: row.title,
        description: row.description.unwrap_or_default(),
        sort_order: i64::from(row.sort_order),
        props: match row.props {
            Some(serde_json::Value::Object(map)) => Some(map),
            _ => None,
        },
        create_at: row.create_at,
        update_at: row.update_at,
        delete_at: row.delete_at,
    }
}

/// `Props` on its way into a `jsonb` bind.
fn props_column(props: Option<&StringInterface>) -> Option<serde_json::Value> {
    props.map(|props| serde_json::Value::Object(props.clone()))
}

/// Port of `GetForChannel`'s page clamping (view_store.go:94-104).
///
/// **Three clamps, and the order matters**: zero-or-less becomes the default *before* the maximum
/// is consulted, so `per_page = 0` is 20 rather than 200. A negative page becomes 0.
///
/// Note the default is [`VIEW_QUERY_DEFAULT_PER_PAGE`] = 20, which is **not** `web.PerPageDefault`
/// = 60. The handler passes `c.Params.PerPage`, which has already defaulted to 60, so 20 is
/// reached only when a caller asks for `per_page=0` explicitly. Both numbers are live.
fn clamp_page(opts: &ViewQueryOpts) -> (i64, i64) {
    let per_page = if opts.per_page <= 0 {
        VIEW_QUERY_DEFAULT_PER_PAGE
    } else if opts.per_page > VIEW_QUERY_MAX_PER_PAGE {
        VIEW_QUERY_MAX_PER_PAGE
    } else {
        opts.per_page
    };
    let page = if opts.page < 0 { 0 } else { opts.page };
    (page, per_page)
}

impl ViewStore for SqlViewStore {
    #[tracing::instrument(skip(self, view), fields(channel_id = %view.channel_id, view_id))]
    async fn save(&self, view: &mut View) -> Result<(), StoreError> {
        // `saveViewT` runs both of these itself "so callers can't forget to validate"
        // (view_store.go:51). The order is load-bearing: `PreSave` mints the id and the
        // timestamps that `IsValid` then requires to be non-empty and non-zero.
        view.pre_save();
        if let Err(app_error) = view.is_valid() {
            return Err(StoreError::Invalid {
                entity: "View",
                app_error,
            });
        }
        tracing::Span::current().record("view_id", &view.id);

        let sort_order = sort_order_column(view.sort_order)?;
        let props = props_column(view.props.as_ref());

        sqlx::query!(
            r#"
            INSERT INTO views
                (id, channelid, type, creatorid, title,
                 description, sortorder, props,
                 createat, updateat, deleteat)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
            view.id,
            view.channel_id,
            view.view_type,
            view.creator_id,
            view.title,
            view.description,
            sort_order,
            props,
            view.create_at,
            view.update_at,
            view.delete_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to save view".to_owned(),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(view_id = %id, found))]
    async fn get(&self, id: &str) -> Result<View, StoreError> {
        let row = sqlx::query_as!(
            ViewRow,
            r#"
            SELECT id            AS "id!",
                   channelid     AS "channel_id!",
                   type          AS "view_type!",
                   creatorid     AS "creator_id!",
                   title         AS "title!",
                   description   AS "description?",
                   sortorder     AS "sort_order!",
                   props         AS "props?",
                   createat      AS "create_at!",
                   updateat      AS "update_at!",
                   deleteat      AS "delete_at!"
              FROM views
             WHERE id = $1 AND deleteat = 0
            "#,
            id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get view with id={id}"),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());
        // `store.NewErrNotFound("View", id)` (view_store.go:82).
        row.map(row_to_view).ok_or_else(|| StoreError::NotFound {
            entity: "View",
            criteria: format!("id={id}"),
        })
    }

    #[tracing::instrument(skip(self, opts), fields(channel_id = %channel_id, found))]
    async fn get_for_channel(
        &self,
        channel_id: &str,
        opts: &ViewQueryOpts,
    ) -> Result<Vec<View>, StoreError> {
        // `store.NewErrInvalidInput("View", "channelID", channelID)` (view_store.go:91) — a 400
        // at the app layer, unlike every other failure on this path.
        if channel_id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "View",
                field: "channelID",
                value: String::new(),
            });
        }

        let (page, per_page) = clamp_page(opts);

        let rows = sqlx::query_as!(
            ViewRow,
            r#"
            SELECT id            AS "id!",
                   channelid     AS "channel_id!",
                   type          AS "view_type!",
                   creatorid     AS "creator_id!",
                   title         AS "title!",
                   description   AS "description?",
                   sortorder     AS "sort_order!",
                   props         AS "props?",
                   createat      AS "create_at!",
                   updateat      AS "update_at!",
                   deleteat      AS "delete_at!"
              FROM views
             WHERE channelid = $1 AND deleteat = 0
             ORDER BY sortorder ASC, createat ASC, id ASC
             LIMIT $2 OFFSET $3
            "#,
            channel_id,
            per_page,
            page * per_page,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get views for channel {channel_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(row_to_view).collect())
    }

    #[tracing::instrument(skip(self, _opts), fields(channel_id = %channel_id, count))]
    async fn count_for_channel(
        &self,
        channel_id: &str,
        _opts: &ViewQueryOpts,
    ) -> Result<i64, StoreError> {
        if channel_id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "View",
                field: "channelID",
                value: String::new(),
            });
        }

        // `COUNT(*)` is never NULL, but sqlx types an aggregate as nullable; `!` asserts what
        // Postgres guarantees rather than defaulting a number that cannot be absent.
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM views WHERE channelid = $1 AND deleteat = 0"#,
            channel_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count views for channel {channel_id}"),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip(self, view), fields(view_id = %view.id))]
    async fn update(&self, view: &mut View) -> Result<(), StoreError> {
        // Same pairing as `save`, same order, and `PreUpdate` only moves `UpdateAt`.
        view.pre_update();
        if let Err(app_error) = view.is_valid() {
            return Err(StoreError::Invalid {
                entity: "View",
                app_error,
            });
        }

        let sort_order = sort_order_column(view.sort_order)?;
        let props = props_column(view.props.as_ref());

        let result = sqlx::query!(
            r#"
            UPDATE views
               SET title = $1, description = $2, sortorder = $3, props = $4, updateat = $5
             WHERE id = $6 AND deleteat = 0
            "#,
            view.title,
            view.description,
            sort_order,
            props,
            view.update_at,
            view.id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update view with id={}", view.id),
            source,
        })?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "View",
                criteria: format!("id={}", view.id),
            });
        }

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(view_id = %view_id))]
    async fn delete(&self, view_id: &str, delete_at: i64) -> Result<(), StoreError> {
        let result = sqlx::query!(
            r#"UPDATE views SET deleteat = $1, updateat = $1 WHERE id = $2 AND deleteat = 0"#,
            delete_at,
            view_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete view with id={view_id}"),
            source,
        })?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "View",
                criteria: format!("id={view_id}"),
            });
        }

        Ok(())
    }

    /// # The four refusals, in Go's order
    ///
    /// 1. Empty `channelID`, then empty `viewID`, then a negative index — all `ErrInvalidInput`,
    ///    a **400**. The handler already refuses a negative index with a different id, so that
    ///    third one is unreachable from the API; the first two are unreachable from a router that
    ///    cannot match an empty segment. Ported anyway, because the store is not the handler.
    /// 2. **An index past the end of the list is also `ErrInvalidInput`**, not a clamp — and this
    ///    one *is* reachable: `POST .../sort_order` with `99` on a three-view channel is a 400.
    ///    Measured.
    /// 3. A view that is not in the channel's list is `ErrNotFound`, a **404**. Note the guard is
    ///    "not in *this channel's* list", so a real view id in another channel is a 404 here
    ///    rather than a mismatch error.
    ///
    /// The empty-list case shares branch 2 with the out-of-range one, so the first view sorted
    /// into an empty channel cannot happen: `len(views) == 0` is a 400.
    #[tracing::instrument(skip(self), fields(view_id = %view_id, channel_id = %channel_id, new_index))]
    async fn update_sort_order(
        &self,
        view_id: &str,
        channel_id: &str,
        new_index: i64,
    ) -> Result<Vec<View>, StoreError> {
        if channel_id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "View",
                field: "channelID",
                value: String::new(),
            });
        }
        if view_id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "View",
                field: "viewID",
                value: String::new(),
            });
        }
        if new_index < 0 {
            return Err(StoreError::InvalidInput {
                entity: "View",
                field: "SortOrder",
                value: new_index.to_string(),
            });
        }

        let now = mm_model::utils::get_millis();

        let mut transaction = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "failed to begin transaction for UpdateSortOrder".to_owned(),
            source,
        })?;

        // The same ordering as `GetForChannel` and **no pagination**: the rewrite renumbers the
        // whole channel, so a page of it would renumber the rest to 0.
        let rows = sqlx::query_as!(
            ViewRow,
            r#"
            SELECT id            AS "id!",
                   channelid     AS "channel_id!",
                   type          AS "view_type!",
                   creatorid     AS "creator_id!",
                   title         AS "title!",
                   description   AS "description?",
                   sortorder     AS "sort_order!",
                   props         AS "props?",
                   createat      AS "create_at!",
                   updateat      AS "update_at!",
                   deleteat      AS "delete_at!"
              FROM views
             WHERE channelid = $1 AND deleteat = 0
             ORDER BY sortorder ASC, createat ASC, id ASC
            "#,
            channel_id,
        )
        .fetch_all(&mut *transaction)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get views for channel {channel_id}"),
            source,
        })?;

        let mut views: Vec<View> = rows.into_iter().map(row_to_view).collect();

        // `len(views) == 0 || int(newIndex) > len(views)-1` (view_store.go:224) — one branch, so
        // an empty channel and an over-long index answer identically.
        if views.is_empty() || new_index > (views.len() as i64) - 1 {
            return Err(StoreError::InvalidInput {
                entity: "View",
                field: "SortOrder",
                value: new_index.to_string(),
            });
        }

        let Some(current_index) = views.iter().position(|view| view.id == view_id) else {
            return Err(StoreError::NotFound {
                entity: "View",
                criteria: format!("id={view_id}"),
            });
        };

        // `RemoveElementFromSliceAtIndex` then `slices.Insert` — a move, so every element between
        // the old and new positions shifts by one and nothing else changes order.
        let current = views.remove(current_index);
        // `new_index` is bounded by the length check above, and the list is one shorter now, so
        // the insert position is always in range.
        views.insert(
            usize::try_from(new_index)
                .unwrap_or(views.len())
                .min(views.len()),
            current,
        );

        let mut ids: Vec<String> = Vec::with_capacity(views.len());
        let mut orders: Vec<i32> = Vec::with_capacity(views.len());
        for (index, view) in views.iter_mut().enumerate() {
            // The returned list carries the new values, and the handler encodes *this* list —
            // not a re-read — so the two must be written together.
            let order = sort_order_column(index as i64)?;
            view.sort_order = i64::from(order);
            view.update_at = now;
            ids.push(view.id.clone());
            orders.push(order);
        }

        // Go's `CASE WHEN Id = ? THEN n … END`, expressed as a join so the statement is static.
        // One `UPDATE`, one `UpdateAt` for every row, exactly as the ladder does it.
        sqlx::query!(
            r#"
            UPDATE views
               SET sortorder = new_order.ord, updateat = $1
              FROM (SELECT unnest($2::text[]) AS id, unnest($3::int[]) AS ord) AS new_order
             WHERE views.id = new_order.id
            "#,
            now,
            &ids,
            &orders,
        )
        .execute(&mut *transaction)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update sort order for views in channel {channel_id}"),
            source,
        })?;

        transaction
            .commit()
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to commit sort order update for views".to_owned(),
                source,
            })?;

        Ok(views)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(page: i64, per_page: i64) -> ViewQueryOpts {
        ViewQueryOpts { page, per_page }
    }

    #[test]
    fn per_page_of_zero_takes_the_stores_default_and_not_its_maximum() {
        // The order of the two clamps: `<= 0` is tested first, so zero is 20 rather than 200.
        assert_eq!(clamp_page(&opts(0, 0)), (0, 20));
        assert_eq!(clamp_page(&opts(0, -5)), (0, 20));
    }

    #[test]
    fn per_page_above_the_maximum_is_clamped_and_below_it_is_kept() {
        assert_eq!(clamp_page(&opts(0, 201)), (0, 200));
        assert_eq!(clamp_page(&opts(0, 200)), (0, 200));
        assert_eq!(clamp_page(&opts(0, 199)), (0, 199));
        assert_eq!(clamp_page(&opts(0, 1)), (0, 1));
    }

    #[test]
    fn a_negative_page_becomes_the_first_page() {
        assert_eq!(clamp_page(&opts(-1, 10)), (0, 10));
        assert_eq!(clamp_page(&opts(3, 10)), (3, 10));
    }

    #[test]
    fn sort_order_is_refused_when_it_does_not_fit_the_integer_column() {
        assert!(sort_order_column(0).is_ok());
        assert_eq!(sort_order_column(i64::from(i32::MAX)).ok(), Some(i32::MAX));
        assert!(matches!(
            sort_order_column(i64::from(i32::MAX) + 1),
            Err(StoreError::Argument { entity: "View", .. })
        ));
        assert!(sort_order_column(i64::from(i32::MIN) - 1).is_err());
    }

    #[test]
    fn a_null_description_reads_as_the_empty_string_and_a_null_props_as_none() {
        let view = row_to_view(ViewRow {
            id: "a".into(),
            channel_id: "b".into(),
            view_type: "kanban".into(),
            creator_id: "c".into(),
            title: "t".into(),
            description: None,
            sort_order: 4,
            props: None,
            create_at: 1,
            update_at: 2,
            delete_at: 0,
        });
        assert_eq!(view.description, "");
        assert_eq!(view.props, None);
        assert_eq!(view.sort_order, 4);
    }

    #[test]
    fn a_props_value_that_is_not_an_object_reads_as_none() {
        let view = row_to_view(ViewRow {
            id: "a".into(),
            channel_id: "b".into(),
            view_type: "kanban".into(),
            creator_id: "c".into(),
            title: "t".into(),
            description: Some("d".into()),
            sort_order: 0,
            props: Some(serde_json::json!([1, 2])),
            create_at: 1,
            update_at: 2,
            delete_at: 0,
        });
        assert_eq!(view.props, None);
        assert_eq!(view.description, "d");
    }

    #[test]
    fn props_reach_the_column_as_a_json_object() {
        let mut props = StringInterface::new();
        props.insert("group_by".into(), serde_json::json!({"field_id": "x"}));
        assert_eq!(
            props_column(Some(&props)),
            Some(serde_json::json!({"group_by": {"field_id": "x"}}))
        );
        assert_eq!(props_column(None), None);
    }
}
