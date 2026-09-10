//! Port of the sidebar-category **reads and writes** of `SqlChannelStore`
//! (channels/store/sqlstore/channel_store_categories.go): `GetSidebarCategories`,
//! `GetSidebarCategory` and `GetSidebarCategoryOrder`, plus
//! `CreateInitialSidebarCategories`, `CreateSidebarCategory`, `UpdateSidebarCategoryOrder`,
//! `UpdateSidebarCategories` and `DeleteSidebarCategory` — and the
//! `completePopulatingCategor{y,ies}T` / `getOrphanedSidebarChannels` machinery the reads all end
//! in and three of the writes reuse.
//!
//! Go hangs these off `ChannelStore`; they live in their own store here so that the sidebar
//! routes and the channel routes can be migrated independently.
//!
//! # Every write is one transaction, and the statement order inside it is Go's
//!
//! Go's own comments say why twice — *"SidebarCategories need to be update first, and then
//! SidebarChannels should be deleted … it prevents deadlocks from other transactions operating
//! on the tables in reverse order"* — and once more for the id-sorted update loop in
//! [`update_sidebar_categories`]. The net effect of reordering is identical, so no test of a
//! response body can see it; it is reproduced because two servers write these tables
//! concurrently against one database and the lock order is the only thing keeping them apart.
//!
//! # The multi-query reads take a `&mut PgConnection`, not a `&PgPool`
//!
//! Go threads `sqlxExecutor` through so that `getSidebarCategoriesT` can run either on the
//! replica or *inside* a write's transaction — `CreateInitialSidebarCategories` and
//! `CreateSidebarCategory` both read their own uncommitted rows back, and
//! `UpdateSidebarCategories` reads each category it is about to change. A `&mut PgConnection` is
//! what both callers can produce: a pool connection derefs to one, and so does a
//! `Transaction`. (`sqlx::Acquire` is the more obvious choice and does not compile here — its
//! impl for `&mut PgConnection` is not higher-ranked, so a generic function over it cannot be
//! instantiated at a borrow whose lifetime the caller chooses.)
//!
//! The single-query leaves stay generic over `PgExecutor`, which keeps the two guard tests below
//! able to pass a pool that has no server behind it.
//!
//! # The answer is not what is in `SidebarChannels`
//!
//! Every one of these reads ends by *adding rows that are not in the table*. A channel the user
//! is a member of but which appears in no category of theirs is an **orphan**, and Go appends it
//! to the Channels category (public/private) or the DMs category (DM/GM) on the way out —
//! `getOrphanedSidebarChannels` (channel_store_categories.go:399). Joining a channel writes a
//! `ChannelMembers` row and no `SidebarChannels` row, so on a normal server *most* of a user's
//! Channels category is orphans. A port that returned the join alone would answer `[]` for a
//! freshly joined user and look plausible doing it.
//!
//! Two consequences worth keeping in mind:
//!
//! - **Orphans come last**, after whatever the join produced, in `DisplayName` order — a
//!   different order from the explicit channels, which are in `SidebarChannels.SortOrder` order.
//! - **The `NOT EXISTS` subquery is scoped to the user *and the team*.** A channel filed in a
//!   category on another team is an orphan here. That is Go's rule, not an oversight: a channel
//!   can only be in one team's sidebar, and DMs are shown on every team.
//!
//! # Nullability
//!
//! Every column of `SidebarCategories` except `Id` is nullable in the schema Go migrates, and Go
//! scans them into plain `string`/`int64`/`bool`. `database/sql` refuses NULL into those, so a
//! NULL row fails the **whole** query rather than defaulting — the same rule
//! `SqlPreferenceStore`'s `Value` follows. The `"col!"` overrides below make sqlx fail the decode
//! identically instead of inventing a `None` Go never produces.

use mm_model::channel::{
    CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE,
};
use mm_model::preference::PREFERENCE_CATEGORY_FAVORITE_CHANNEL;
use mm_model::sidebar_category::{
    DEFAULT_SIDEBAR_SORT_ORDER_CHANNELS, DEFAULT_SIDEBAR_SORT_ORDER_DMS,
    DEFAULT_SIDEBAR_SORT_ORDER_FAVORITES, MINIMAL_SIDEBAR_SORT_DISTANCE, OrderedSidebarCategories,
    SIDEBAR_CATEGORY_CHANNELS, SIDEBAR_CATEGORY_CUSTOM, SIDEBAR_CATEGORY_DIRECT_MESSAGES,
    SIDEBAR_CATEGORY_FAVORITES, SIDEBAR_CATEGORY_SORT_DEFAULT, SIDEBAR_CATEGORY_SORT_RECENT,
    SidebarCategory, SidebarCategoryWithChannels,
};
use sqlx::{PgConnection, PgPool, Postgres};

use crate::error::StoreError;

/// The subset of Go's `store.ChannelStore` sidebar surface that is ported: three reads and five
/// writes.
pub trait SidebarCategoryStore {
    /// Port of `SqlChannelStore.GetSidebarCategoriesForTeamForUser`
    /// (channel_store_categories.go:542) — which is `GetSidebarCategories` (:546) under a second
    /// name, both delegating to the same `getSidebarCategoriesT`.
    fn get_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<OrderedSidebarCategories, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetSidebarCategory` (channel_store_categories.go:453).
    fn get_sidebar_category(
        &self,
        category_id: &str,
    ) -> impl std::future::Future<Output = Result<SidebarCategoryWithChannels, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetSidebarCategoryOrder` (channel_store_categories.go:550).
    fn get_sidebar_category_order(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// Port of `SqlChannelStore.CreateInitialSidebarCategories`
    /// (channel_store_categories.go:19).
    fn create_initial_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<OrderedSidebarCategories, StoreError>> + Send;

    /// Port of `SqlChannelStore.CreateSidebarCategory` (channel_store_categories.go:232).
    fn create_sidebar_category(
        &self,
        user_id: &str,
        team_id: &str,
        new_category: &SidebarCategoryWithChannels,
    ) -> impl std::future::Future<Output = Result<SidebarCategoryWithChannels, StoreError>> + Send;

    /// Port of `SqlChannelStore.UpdateSidebarCategoryOrder` (channel_store_categories.go:592).
    fn update_sidebar_category_order(
        &self,
        user_id: &str,
        team_id: &str,
        category_order: &[String],
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelStore.UpdateSidebarCategories` (channel_store_categories.go:629).
    fn update_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
        categories: &[SidebarCategoryWithChannels],
    ) -> impl std::future::Future<Output = Result<SidebarCategoryUpdate, StoreError>> + Send;

    /// Port of `SqlChannelStore.DeleteSidebarCategory` (channel_store_categories.go:1013).
    fn delete_sidebar_category(
        &self,
        category_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlSidebarCategoryStore {
    pool: PgPool,
}

impl SqlSidebarCategoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl SidebarCategoryStore for SqlSidebarCategoryStore {
    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
    async fn get_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<OrderedSidebarCategories, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(|source| StoreError::Db {
            context: "acquire_connection".to_owned(),
            source,
        })?;
        get_sidebar_categories(&mut conn, user_id, team_id).await
    }

    #[tracing::instrument(skip_all, fields(category_id = %category_id))]
    async fn get_sidebar_category(
        &self,
        category_id: &str,
    ) -> Result<SidebarCategoryWithChannels, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(|source| StoreError::Db {
            context: "acquire_connection".to_owned(),
            source,
        })?;
        get_sidebar_category(&mut conn, category_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
    async fn get_sidebar_category_order(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        get_sidebar_category_order(&self.pool, user_id, team_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
    async fn create_initial_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<OrderedSidebarCategories, StoreError> {
        create_initial_sidebar_categories(&self.pool, user_id, team_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
    async fn create_sidebar_category(
        &self,
        user_id: &str,
        team_id: &str,
        new_category: &SidebarCategoryWithChannels,
    ) -> Result<SidebarCategoryWithChannels, StoreError> {
        create_sidebar_category(&self.pool, user_id, team_id, new_category).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
    async fn update_sidebar_category_order(
        &self,
        user_id: &str,
        team_id: &str,
        category_order: &[String],
    ) -> Result<(), StoreError> {
        update_sidebar_category_order(&self.pool, user_id, team_id, category_order).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
    async fn update_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
        categories: &[SidebarCategoryWithChannels],
    ) -> Result<SidebarCategoryUpdate, StoreError> {
        update_sidebar_categories(&self.pool, user_id, team_id, categories).await
    }

    #[tracing::instrument(skip_all, fields(category_id = %category_id))]
    async fn delete_sidebar_category(&self, category_id: &str) -> Result<(), StoreError> {
        delete_sidebar_category(&self.pool, category_id).await
    }
}

/// One row of Go's `sidebarCategorySelectQuery` (channel_store.go:548) plus the joined
/// `SidebarChannels.ChannelId` — Go's `sidebarCategoryForJoin`
/// (channel_store_categories.go:227).
///
/// `channelid` is the only genuinely optional column: it is NULL for a category with no explicit
/// channels, because the join is a `LEFT JOIN`.
struct SidebarCategoryRow {
    id: String,
    userid: String,
    teamid: String,
    sortorder: i64,
    sorting: String,
    category_type: String,
    displayname: String,
    muted: bool,
    collapsed: bool,
    channelid: Option<String>,
}

impl SidebarCategoryRow {
    fn to_category(&self) -> SidebarCategory {
        SidebarCategory {
            id: self.id.clone(),
            user_id: self.userid.clone(),
            team_id: self.teamid.clone(),
            sort_order: self.sortorder,
            sorting: self.sorting.clone(),
            category_type: self.category_type.clone(),
            display_name: self.displayname.clone(),
            muted: self.muted,
            collapsed: self.collapsed,
        }
    }
}

/// Go's `OrphanedSidebarChannel` (channel_store_categories.go:394).
struct OrphanedSidebarChannel {
    id: String,
    channel_type: String,
}

/// Port of `getSidebarCategoriesT` (channel_store_categories.go:490).
///
/// # Row order is the whole contract
///
/// `ORDER BY SidebarCategories.SortOrder ASC, SidebarChannels.SortOrder ASC` — and the grouping
/// below relies on it twice over. Categories enter `categories`/`order` **in first-seen order**,
/// so the category sort key decides the sidebar's order; channels are appended in the order the
/// rows arrive, so the channel sort key decides each category's contents. Go does not sort
/// afterwards, and neither does this.
///
/// Both `SortOrder` columns are nullable, so a NULL sorts **last** under Postgres' `ASC`
/// default. That is Go's behaviour too — identical SQL — rather than something chosen here.
///
/// # Both slices are always non-nil
///
/// Go initialises `Categories` and `Order` with `make(..., 0)` before the loop, so this route
/// emits `[]` and never `null` even for a user with no categories at all. `Some(Vec::new())`
/// rather than `None` is what reproduces that; see the `null`-vs-`[]` note in
/// `mm_model::sidebar_category`.
#[tracing::instrument(skip(conn), fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn get_sidebar_categories(
    conn: &mut PgConnection,
    user_id: &str,
    team_id: &str,
) -> Result<OrderedSidebarCategories, StoreError> {
    let rows = sqlx::query_as!(
        SidebarCategoryRow,
        r#"
        SELECT sidebarcategories.id AS "id!",
               sidebarcategories.userid AS "userid!",
               sidebarcategories.teamid AS "teamid!",
               sidebarcategories.sortorder AS "sortorder!",
               sidebarcategories.sorting AS "sorting!",
               sidebarcategories.type AS "category_type!",
               sidebarcategories.displayname AS "displayname!",
               sidebarcategories.muted AS "muted!",
               sidebarcategories.collapsed AS "collapsed!",
               -- `?` and not the schema's NOT NULL: the join is a LEFT JOIN, so a category
               -- with no explicit channels yields one row with this column NULL. sqlx infers
               -- nullability from the column definition and cannot see that, and without the
               -- override every empty category fails the whole query with UnexpectedNullError.
               sidebarchannels.channelid AS "channelid?"
        FROM sidebarcategories
        LEFT JOIN sidebarchannels ON sidebarchannels.categoryid = sidebarcategories.id
        WHERE sidebarcategories.userid = $1
          AND sidebarcategories.teamid = $2
        ORDER BY sidebarcategories.sortorder ASC, sidebarchannels.sortorder ASC
        "#,
        user_id,
        team_id,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get categories for userId={user_id}, teamId={team_id}"),
        source,
    })?;

    let mut categories: Vec<SidebarCategoryWithChannels> = Vec::new();
    let mut order: Vec<String> = Vec::new();

    for row in &rows {
        // Go scans the whole list for a matching id rather than assuming the ORDER BY groups
        // them (`for _, existing := range oc.Categories`). Kept: the two agree today only
        // because the sort key is the category's own column, and a duplicate SortOrder could
        // interleave two categories' rows.
        let index = match categories.iter().position(|c| c.category.id == row.id) {
            Some(index) => index,
            None => {
                categories.push(SidebarCategoryWithChannels {
                    category: row.to_category(),
                    channel_ids: Some(Vec::new()),
                });
                order.push(row.id.clone());
                categories.len() - 1
            }
        };

        if let Some(channel_id) = &row.channelid
            && let Some(channel_ids) = &mut categories[index].channel_ids
        {
            channel_ids.push(channel_id.clone());
        }
    }

    complete_populating_categories(&mut *conn, user_id, team_id, &mut categories).await?;

    tracing::Span::current().record("count", categories.len());
    Ok(OrderedSidebarCategories {
        categories: Some(categories),
        order: Some(order),
    })
}

/// Port of `getSidebarCategoryT` (channel_store_categories.go:457).
///
/// # An empty result is a not-found, not an empty category
///
/// The `LEFT JOIN` means a category with no channels still yields one row, so zero rows can only
/// mean the category does not exist — `store.NewErrNotFound("SidebarCategories", categoryId)`.
/// The app layer turns that into a **404**, but note that the API layer's permission gate calls
/// this first and answers **403** for a missing category, so the 404 is not reachable through
/// `GET .../categories/{category_id}`. See `mm_api::sidebar`.
///
/// The category itself comes from `categories[0]`; every later row contributes only its
/// `ChannelId`. With no `ORDER BY` on the category columns that is safe because they are all
/// equal — one category, joined.
#[tracing::instrument(skip(conn), fields(category_id = %category_id))]
pub async fn get_sidebar_category(
    conn: &mut PgConnection,
    category_id: &str,
) -> Result<SidebarCategoryWithChannels, StoreError> {
    let rows = sqlx::query_as!(
        SidebarCategoryRow,
        r#"
        SELECT sidebarcategories.id AS "id!",
               sidebarcategories.userid AS "userid!",
               sidebarcategories.teamid AS "teamid!",
               sidebarcategories.sortorder AS "sortorder!",
               sidebarcategories.sorting AS "sorting!",
               sidebarcategories.type AS "category_type!",
               sidebarcategories.displayname AS "displayname!",
               sidebarcategories.muted AS "muted!",
               sidebarcategories.collapsed AS "collapsed!",
               -- `?` and not the schema's NOT NULL: the join is a LEFT JOIN, so a category
               -- with no explicit channels yields one row with this column NULL. sqlx infers
               -- nullability from the column definition and cannot see that, and without the
               -- override every empty category fails the whole query with UnexpectedNullError.
               sidebarchannels.channelid AS "channelid?"
        FROM sidebarcategories
        LEFT JOIN sidebarchannels ON sidebarchannels.categoryid = sidebarcategories.id
        WHERE sidebarcategories.id = $1
        ORDER BY sidebarchannels.sortorder ASC
        "#,
        category_id,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get category with id={category_id}"),
        source,
    })?;

    let Some(first) = rows.first() else {
        return Err(StoreError::NotFound {
            entity: "SidebarCategories",
            criteria: format!("id={category_id}"),
        });
    };

    let mut category = SidebarCategoryWithChannels {
        category: first.to_category(),
        channel_ids: Some(Vec::new()),
    };
    for row in &rows {
        if let Some(channel_id) = &row.channelid
            && let Some(channel_ids) = &mut category.channel_ids
        {
            channel_ids.push(channel_id.clone());
        }
    }

    complete_populating_category(&mut *conn, &mut category).await?;
    Ok(category)
}

/// Port of `getSidebarCategoryOrderT` (channel_store_categories.go:554).
///
/// The one read that does **not** touch `SidebarChannels` and does not populate orphans: it is
/// the category ids alone, in `SortOrder` order. `ids := []string{}` in Go, so an empty answer is
/// `[]` and never `null`.
#[tracing::instrument(skip(executor), fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn get_sidebar_category_order<'e, E>(
    executor: E,
    user_id: &str,
    team_id: &str,
) -> Result<Vec<String>, StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    let ids = sqlx::query_scalar!(
        r#"
        SELECT id AS "id!"
        FROM sidebarcategories
        WHERE userid = $1
          AND teamid = $2
        ORDER BY sidebarcategories.sortorder ASC
        "#,
        user_id,
        team_id,
    )
    .fetch_all(executor)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get category order for userId={user_id}, teamId={team_id}"),
        source,
    })?;

    tracing::Span::current().record("count", ids.len());
    Ok(ids)
}

/// Port of `completePopulatingCategoryT` (channel_store_categories.go:337) — the single-category
/// variant.
///
/// The two selectors are derived from **this category's own type**, and everything the query
/// returns is appended to it unconditionally: the filter has already restricted the rows to the
/// kinds this category holds. For a Favorites or a custom category both selectors are false, so
/// [`get_orphaned_sidebar_channels`] returns early and no query runs at all.
async fn complete_populating_category<'e, E>(
    executor: E,
    category: &mut SidebarCategoryWithChannels,
) -> Result<(), StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    let orphans = get_orphaned_sidebar_channels(
        executor,
        &category.category.user_id,
        &category.category.team_id,
        category.category.category_type == SIDEBAR_CATEGORY_CHANNELS,
        category.category.category_type == SIDEBAR_CATEGORY_DIRECT_MESSAGES,
    )
    .await?;

    if let Some(channel_ids) = &mut category.channel_ids {
        channel_ids.extend(orphans.into_iter().map(|orphan| orphan.id));
    }
    Ok(())
}

/// Port of `completePopulatingCategoriesT` (channel_store_categories.go:360) — the list variant,
/// and *not* the same function applied per category.
///
/// One query serves both destinations, so the selectors ask "does a Channels category exist" and
/// "does a DMs category exist" rather than "what type is this category", and each returned row is
/// then routed by its **channel type**: `O`/`P` to the Channels category, `D`/`G` to the DMs one.
/// Calling the single-category variant in a loop would issue N queries and, worse, would let a
/// DM land in the Channels category on a server where the DMs category had been deleted.
///
/// Go takes the *last* matching index for each (`channelsIndex = i` without a break), so a
/// duplicate Channels category collects the orphans in the later one. Reproduced.
async fn complete_populating_categories<'e, E>(
    executor: E,
    user_id: &str,
    team_id: &str,
    categories: &mut [SidebarCategoryWithChannels],
) -> Result<(), StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    let mut channels_index: Option<usize> = None;
    let mut dms_index: Option<usize> = None;
    for (index, category) in categories.iter().enumerate() {
        if category.category.category_type == SIDEBAR_CATEGORY_CHANNELS {
            channels_index = Some(index);
        } else if category.category.category_type == SIDEBAR_CATEGORY_DIRECT_MESSAGES {
            dms_index = Some(index);
        }
    }

    let orphans = get_orphaned_sidebar_channels(
        executor,
        user_id,
        team_id,
        channels_index.is_some(),
        dms_index.is_some(),
    )
    .await?;

    for orphan in orphans {
        let destination = if orphan.channel_type == CHANNEL_TYPE_OPEN
            || orphan.channel_type == CHANNEL_TYPE_PRIVATE
        {
            channels_index
        } else if orphan.channel_type == CHANNEL_TYPE_DIRECT
            || orphan.channel_type == CHANNEL_TYPE_GROUP
        {
            dms_index
        } else {
            None
        };

        if let Some(index) = destination
            && let Some(channel_ids) = &mut categories[index].channel_ids
        {
            channel_ids.push(orphan.id);
        }
    }

    Ok(())
}

/// Port of `getOrphanedSidebarChannels` (channel_store_categories.go:399): the user's channels on
/// this team that appear in no category of theirs.
///
/// # The early return is Go's, and it is not an optimisation
///
/// With both selectors false, Go returns `nil, nil` **before building the query**. Letting it run
/// would produce `sq.Or{}` — which squirrel renders as the empty string — and a `WHERE` with an
/// empty disjunct matches *everything*, so a Favorites category would swallow every channel the
/// user is in. The guard is load-bearing.
///
/// # The predicates, in Go's order
///
/// 1. `ChannelMembers.UserId = ?` — membership, not visibility.
/// 2. The type filter: DMs and GMs regardless of team, public and private **only on this team**.
///    A DM belongs to no team, which is why the `TeamId` predicate sits inside the public/private
///    half rather than beside it.
/// 3. `Channels.DeleteAt = 0` — the *channel's* column, not the membership's. An archived channel
///    disappears from the sidebar while the membership row survives.
/// 4. `NOT EXISTS (…)` — no `SidebarChannels` row for this channel under any category belonging
///    to this user **on this team**.
///
/// `ORDER BY DisplayName ASC` is the channel's display name, unqualified in Go because
/// `ChannelMembers` has no such column. Ties are broken by whatever Postgres returns, on both
/// servers.
#[tracing::instrument(skip(executor), fields(user_id = %user_id, team_id = %team_id, count))]
async fn get_orphaned_sidebar_channels<'e, E>(
    executor: E,
    user_id: &str,
    team_id: &str,
    select_channels: bool,
    select_dms: bool,
) -> Result<Vec<OrphanedSidebarChannel>, StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    if !select_channels && !select_dms {
        return Ok(Vec::new());
    }

    let rows = sqlx::query_as!(
        OrphanedSidebarChannel,
        r#"
        SELECT channels.id AS "id!",
               channels.type::text AS "channel_type!"
        FROM channelmembers
        LEFT JOIN channels ON channels.id = channelmembers.channelid
        WHERE channelmembers.userid = $1
          AND (
                ($3 AND channels.type IN ('D', 'G'))
             OR ($4 AND channels.type IN ('O', 'P') AND channels.teamid = $2)
          )
          AND channels.deleteat = 0
          AND NOT EXISTS (
                SELECT 1
                FROM sidebarchannels
                JOIN sidebarcategories ON sidebarchannels.categoryid = sidebarcategories.id
                WHERE sidebarchannels.channelid = channelmembers.channelid
                  AND sidebarcategories.userid = $1
                  AND sidebarcategories.teamid = $2
          )
        ORDER BY channels.displayname ASC
        "#,
        user_id,
        team_id,
        select_dms,
        select_channels,
    )
    .fetch_all(executor)
    .await
    .map_err(|source| StoreError::Db {
        context: "Failed to get orphaned sidebar channels".to_owned(),
        source,
    })?;

    tracing::Span::current().record("count", rows.len());
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// What `UpdateSidebarCategories` returns: Go's `(updated, original)` pair, named because the
/// app layer needs *both* — the mute reconciliation diffs one against the other.
///
/// The two are index-aligned with each other **and with the caller's request**, which is the
/// property `App::mute_channels_for_updated_categories` reads them by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarCategoryUpdate {
    pub updated: Vec<SidebarCategoryWithChannels>,
    pub original: Vec<SidebarCategoryWithChannels>,
}

/// Port of `SqlChannelStore.CreateInitialSidebarCategories` (channel_store_categories.go:19),
/// the lazy migration behind every sidebar read.
///
/// # The ids are deterministic, and that is the whole design
///
/// `fmt.Sprintf("%s_%s_%s", type, userId, teamId)` — so `favorites_<26>_<26>`, and Go's comment
/// says why: *"Use deterministic IDs for default categories to prevent potentially creating
/// multiple copies of a default category"*. The primary key on `SidebarCategories.Id` is what
/// actually enforces that, and it is why two servers racing to migrate the same user end with one
/// set of rows rather than six. It is also why `IsValidCategoryId` has a second branch at all —
/// these ids are 60 characters and are not `NewId()` output.
///
/// # `Favorites` writes its channels *before* its category row
///
/// Go's comment: *"Create the SidebarChannels first since there's more opportunity for something
/// to fail here"*. Inside one transaction the ordering is invisible unless something fails, which
/// is exactly the case it was written for. Reproduced.
///
/// # Only the missing types are inserted
///
/// The `SELECT type` up front means a user who has Favorites but lost Channels gains Channels
/// alone. Go builds one multi-row `INSERT`; this issues one statement per missing type inside the
/// same transaction, which fails and rolls back identically.
///
/// # The three defaults are not interchangeable
///
/// `SortOrder` is 0/10/20 and `Sorting` is `""`/`""`/`recent` — Direct Messages is the one that
/// sorts by recency, and its display name is `"Direct Messages"` with a space. All three display
/// names are English literals that the *client* retranslates, so they are not i18n-blocked here.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, team_id = %team_id, inserted))]
pub async fn create_initial_sidebar_categories(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
) -> Result<OrderedSidebarCategories, StoreError> {
    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "CreateInitialSidebarCategories: begin_transaction".to_owned(),
        source,
    })?;

    let existing: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT type AS "category_type!"
        FROM sidebarcategories
        WHERE userid = $1
          AND type IN ('favorites', 'channels', 'direct_messages')
          AND teamid = $2
        "#,
        user_id,
        team_id,
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "createInitialSidebarCategoriesT: failed to select existing categories".to_owned(),
        source,
    })?;

    let mut inserted = 0usize;

    if !existing.iter().any(|t| t == SIDEBAR_CATEGORY_FAVORITES) {
        let favorites_id = default_category_id(SIDEBAR_CATEGORY_FAVORITES, user_id, team_id);
        migrate_favorites_to_sidebar(&mut tx, user_id, team_id, &favorites_id).await?;
        insert_default_category(
            &mut tx,
            &favorites_id,
            user_id,
            team_id,
            DEFAULT_SIDEBAR_SORT_ORDER_FAVORITES,
            SIDEBAR_CATEGORY_SORT_DEFAULT,
            SIDEBAR_CATEGORY_FAVORITES,
            "Favorites",
        )
        .await?;
        inserted += 1;
    }

    if !existing.iter().any(|t| t == SIDEBAR_CATEGORY_CHANNELS) {
        insert_default_category(
            &mut tx,
            &default_category_id(SIDEBAR_CATEGORY_CHANNELS, user_id, team_id),
            user_id,
            team_id,
            DEFAULT_SIDEBAR_SORT_ORDER_CHANNELS,
            SIDEBAR_CATEGORY_SORT_DEFAULT,
            SIDEBAR_CATEGORY_CHANNELS,
            "Channels",
        )
        .await?;
        inserted += 1;
    }

    if !existing
        .iter()
        .any(|t| t == SIDEBAR_CATEGORY_DIRECT_MESSAGES)
    {
        insert_default_category(
            &mut tx,
            &default_category_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES, user_id, team_id),
            user_id,
            team_id,
            DEFAULT_SIDEBAR_SORT_ORDER_DMS,
            SIDEBAR_CATEGORY_SORT_RECENT,
            SIDEBAR_CATEGORY_DIRECT_MESSAGES,
            "Direct Messages",
        )
        .await?;
        inserted += 1;
    }
    tracing::Span::current().record("inserted", inserted);

    // Read back **inside the transaction**: the rows above are not committed yet, so a read on
    // the pool would answer the empty sidebar this function exists to replace.
    let ordered = get_sidebar_categories(&mut tx, user_id, team_id).await?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "CreateInitialSidebarCategories: commit_transaction".to_owned(),
        source,
    })?;

    Ok(ordered)
}

/// `fmt.Sprintf("%s_%s_%s", type, userId, teamId)` (channel_store_categories.go:72).
///
/// Kept as one function so the three call sites cannot drift, and so the shape
/// `mm_model::sidebar_category::is_valid_category_id`'s second branch matches is written once.
fn default_category_id(category_type: &str, user_id: &str, team_id: &str) -> String {
    format!("{category_type}_{user_id}_{team_id}")
}

/// One row of `createInitialSidebarCategoriesT`'s `INSERT` (channel_store_categories.go:67).
///
/// `Muted` and `Collapsed` are literal `false` in all three of Go's `Values(...)` calls, not
/// values from anywhere — a default category is never born muted or collapsed.
#[allow(clippy::too_many_arguments)]
async fn insert_default_category(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    id: &str,
    user_id: &str,
    team_id: &str,
    sort_order: i64,
    sorting: &str,
    category_type: &str,
    display_name: &str,
) -> Result<(), StoreError> {
    sqlx::query!(
        r#"
        INSERT INTO sidebarcategories
            (id, userid, teamid, sortorder, sorting, type, displayname, muted, collapsed)
        VALUES ($1, $2, $3, $4, $5, $6, $7, false, false)
        "#,
        id,
        user_id,
        team_id,
        sort_order,
        sorting,
        category_type,
        display_name,
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "createInitialSidebarCategoriesT: failed to insert categories".to_owned(),
        source,
    })?;
    Ok(())
}

/// Port of `migrateFavoritesToSidebarT` (channel_store_categories.go:139): the user's
/// `favorite_channel` **preferences** become `SidebarChannels` rows under the new Favorites
/// category.
///
/// This is the sidebar/preferences duality in its original form. Favourites predate categories,
/// so the migration reads the old representation and writes the new one; from then on
/// [`update_sidebar_categories`] keeps both in step in the other direction.
///
/// # Three joins, and each one narrows the answer
///
/// - `JOIN Channels ON Preferences.Name = Channels.Id` drops a preference naming a channel that
///   no longer exists.
/// - `JOIN ChannelMembers ON …ChannelId AND …UserId` drops one the user has since left.
/// - `Channels.TeamId = $2 OR Channels.TeamId = ''` keeps this team's channels **and every DM**,
///   whose `TeamId` is the empty string. A favourite DM therefore lands in the Favorites category
///   of whichever team is migrated first.
///
/// `Preferences.Value = 'true'` is an exact string match, so a value of `"TRUE"` is not a
/// favourite here. `ORDER BY Channels.DisplayName, Channels.Name ASC` fixes the `SortOrder`s,
/// which are `i * MinimalSidebarSortDistance` — so the initial Favorites order is
/// display-name order and not preference-insert order.
async fn migrate_favorites_to_sidebar(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    user_id: &str,
    team_id: &str,
    favorites_category_id: &str,
) -> Result<(), StoreError> {
    let favourites: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT preferences.name AS "name!"
        FROM preferences
        JOIN channels ON preferences.name = channels.id
        JOIN channelmembers ON preferences.name = channelmembers.channelid
                           AND preferences.userid = channelmembers.userid
        WHERE preferences.userid = $1
          AND preferences.category = $3
          AND preferences.value = 'true'
          AND (channels.teamid = $2 OR channels.teamid = '')
        ORDER BY channels.displayname, channels.name ASC
        "#,
        user_id,
        team_id,
        PREFERENCE_CATEGORY_FAVORITE_CHANNEL,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "migrateFavoritesToSidebarT: unable to get favorite channel IDs".to_owned(),
        source,
    })?;

    insert_sidebar_channels(&mut *tx, &favourites, user_id, favorites_category_id, false)
        .await
        .map_err(|err| match err {
            StoreError::Db { source, .. } => StoreError::Db {
                context: "migrateFavoritesToSidebarT: unable to insert SidebarChannel".to_owned(),
                source,
            },
            other => other,
        })
}

/// `INSERT INTO SidebarChannels` for a whole category, `SortOrder` counting up in
/// `MinimalSidebarSortDistance` steps from zero — the shape shared by
/// `migrateFavoritesToSidebarT`, `CreateSidebarCategory` and `UpdateSidebarCategories`.
///
/// `upsert` is the one difference between the three: `UpdateSidebarCategories` carries
/// `ON CONFLICT (ChannelId, UserId, CategoryId) DO UPDATE SET SortOrder = excluded.SortOrder`
/// and the other two do not, so a conflict is an error there and a re-sort here.
///
/// `WITH ORDINALITY` supplies Go's loop index. An empty list issues no statement at all, matching
/// Go's `if len(category.Channels) > 0` guard — and mattering, because an `INSERT … SELECT` over
/// an empty array would be a no-op while an empty `VALUES` list is a syntax error.
async fn insert_sidebar_channels(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    channel_ids: &[String],
    user_id: &str,
    category_id: &str,
    upsert: bool,
) -> Result<(), StoreError> {
    if channel_ids.is_empty() {
        return Ok(());
    }

    if upsert {
        sqlx::query!(
            r#"
            INSERT INTO sidebarchannels (channelid, userid, categoryid, sortorder)
            SELECT entry.channelid, $2, $3, (entry.ord - 1) * $4
            FROM unnest($1::text[]) WITH ORDINALITY AS entry(channelid, ord)
            ON CONFLICT (channelid, userid, categoryid)
                DO UPDATE SET sortorder = excluded.sortorder
            "#,
            channel_ids,
            user_id,
            category_id,
            MINIMAL_SIDEBAR_SORT_DISTANCE,
        )
        .execute(&mut **tx)
        .await
    } else {
        sqlx::query!(
            r#"
            INSERT INTO sidebarchannels (channelid, userid, categoryid, sortorder)
            SELECT entry.channelid, $2, $3, (entry.ord - 1) * $4
            FROM unnest($1::text[]) WITH ORDINALITY AS entry(channelid, ord)
            "#,
            channel_ids,
            user_id,
            category_id,
            MINIMAL_SIDEBAR_SORT_DISTANCE,
        )
        .execute(&mut **tx)
        .await
    }
    .map_err(|source| StoreError::Db {
        context: "failed to save SidebarChannels".to_owned(),
        source,
    })?;

    Ok(())
}

/// Port of `SqlChannelStore.CreateSidebarCategory` (channel_store_categories.go:232).
///
/// # The new category is *always* `custom`, whatever the request said
///
/// Go builds the row from a fresh `model.SidebarCategory` literal with `Type:
/// model.SidebarCategoryCustom`, so `type` in the body is ignored — a client cannot create a
/// second Favorites. `Collapsed` is absent from that literal too, so it is `false` regardless of
/// the request, unlike `Muted` and `Sorting` which are taken from it.
///
/// # It is placed *second* if Favorites is first, and first otherwise
///
/// Go's comment spells it out, and the reason it is worth a note is that the placement is decided
/// by `Categories[0].Type` alone — not by looking for a Favorites category anywhere in the order.
/// A user who has dragged Favorites down gets the new category at the very top.
///
/// # `SortOrder` is written twice and the first value never survives
///
/// The `INSERT` writes `MinimalSidebarSortDistance * len(newOrder)` — the end of the list — and
/// then `updateSidebarCategoryOrderT` immediately rewrites every category's `SortOrder` from the
/// new order, putting this one at 0 or 10. The struct returned to the caller is then *patched* to
/// that second value (`category.SortOrder = int64(newCategorySortOrder)`) because it still holds
/// the first. So the response and the row agree, but only because of that patch: dropping it
/// answers with a plausible sort order that is 10× the category count.
///
/// # `channel_ids` in the answer is the request's list, not a re-read
///
/// Go returns `newCategory.Channels` verbatim. Since the API layer has already filtered that list
/// to channels the user is in and de-duplicated it, the answer is that filtered list — and a
/// channel the caller named but is not a member of is silently absent rather than refused.
#[tracing::instrument(skip(pool, new_category), fields(user_id = %user_id, team_id = %team_id, category_id))]
pub async fn create_sidebar_category(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
    new_category: &SidebarCategoryWithChannels,
) -> Result<SidebarCategoryWithChannels, StoreError> {
    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    let existing = get_sidebar_categories(&mut tx, user_id, team_id).await?;
    let categories = existing.categories.unwrap_or_default();
    if categories.is_empty() {
        return Err(StoreError::NotFound {
            entity: "categories not found",
            criteria: format!("userId={user_id},teamId={team_id}"),
        });
    }
    let mut new_order = existing.order.unwrap_or_default();

    let new_category_id = mm_model::utils::new_id();
    tracing::Span::current().record("category_id", &new_category_id);

    // Go indexes `newOrder[0]` unguarded; `categories` is non-empty and the two are appended to
    // in lockstep by `getSidebarCategoriesT`, so `order` is non-empty too.
    let new_category_sort_order = if categories[0].category.category_type
        == SIDEBAR_CATEGORY_FAVORITES
        && !new_order.is_empty()
    {
        new_order.insert(1, new_category_id.clone());
        MINIMAL_SIDEBAR_SORT_DISTANCE
    } else {
        new_order.insert(0, new_category_id.clone());
        0
    };

    let channel_ids = new_category.channel_ids.clone().unwrap_or_default();

    let mut category = SidebarCategory {
        id: new_category_id.clone(),
        user_id: user_id.to_owned(),
        team_id: team_id.to_owned(),
        // The end of the list; `update_sidebar_category_order_within` overwrites it below.
        sort_order: MINIMAL_SIDEBAR_SORT_DISTANCE * new_order.len() as i64,
        sorting: new_category.category.sorting.clone(),
        category_type: SIDEBAR_CATEGORY_CUSTOM.to_owned(),
        display_name: new_category.category.display_name.clone(),
        muted: new_category.category.muted,
        collapsed: false,
    };

    sqlx::query!(
        r#"
        INSERT INTO sidebarcategories
            (id, userid, teamid, sortorder, sorting, type, displayname, muted, collapsed)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
        category.id,
        category.user_id,
        category.team_id,
        category.sort_order,
        category.sorting,
        category.category_type,
        category.display_name,
        category.muted,
        category.collapsed,
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to save SidebarCategory".to_owned(),
        source,
    })?;

    if !channel_ids.is_empty() {
        // "Remove any channels from their previous categories and add them to the new one" —
        // scoped to this **user** and this **team** through the join, so a channel filed on
        // another team is left alone. Without this a channel would be in two categories at once
        // and `getSidebarCategoriesT` would return it twice.
        sqlx::query!(
            r#"
            DELETE FROM sidebarchannels
            USING sidebarcategories
            WHERE sidebarchannels.categoryid = sidebarcategories.id
              AND sidebarchannels.userid = $1
              AND sidebarchannels.channelid = ANY($2::text[])
              AND sidebarcategories.teamid = $3
            "#,
            user_id,
            &channel_ids,
            team_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to delete SidebarChannels".to_owned(),
            source,
        })?;

        insert_sidebar_channels(&mut tx, &channel_ids, user_id, &new_category_id, false).await?;
    }

    update_sidebar_category_order_within(&mut tx, &new_order).await?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    // "patch category to return proper sort order" — see the note above.
    category.sort_order = new_category_sort_order;

    Ok(SidebarCategoryWithChannels {
        category,
        channel_ids: new_category.channel_ids.clone(),
    })
}

/// Port of `updateSidebarCategoryOrderT` (channel_store_categories.go:573): rewrite `SortOrder`
/// for the ids given, `0, 10, 20, …` in list order.
///
/// One `UPDATE` per id, as Go does, rather than a single statement joined against the array. The
/// difference only shows for a *duplicated* id — Go's last write wins and a joined update picks a
/// row arbitrarily — and neither caller can produce one, but the loop is the version whose
/// behaviour is defined.
///
/// **An id not belonging to this user is updated all the same.** There is no `UserId` predicate
/// here; the callers are responsible for that, and both check it — `CreateSidebarCategory` builds
/// the list from a read it just did, and `UpdateSidebarCategoryOrder` compares the list against
/// the user's own order first.
async fn update_sidebar_category_order_within(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    category_order: &[String],
) -> Result<(), StoreError> {
    let mut running_order: i64 = 0;
    for category_id in category_order {
        sqlx::query!(
            "UPDATE sidebarcategories SET sortorder = $1 WHERE id = $2",
            running_order,
            category_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "Error updating sidebar category order".to_owned(),
            source,
        })?;
        running_order += MINIMAL_SIDEBAR_SORT_DISTANCE;
    }
    Ok(())
}

/// Port of `SqlChannelStore.UpdateSidebarCategoryOrder` (channel_store_categories.go:592).
///
/// # Two guards with two different statuses, and the length check comes first
///
/// A list of the wrong **length** is a bare `errors.New` — which `errors.As` does not match, so
/// the app layer answers **500**. A list of the right length naming a category the user does not
/// have is `store.NewErrInvalidInput`, which the app layer answers **400**. So reordering these
/// two checks, or collapsing them into one, moves a client's status code.
///
/// The membership test runs over the *existing* order looking for each id in the request, which
/// makes "no categories left out" the thing being enforced; combined with the equal-length check
/// that also excludes anything extra.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, team_id = %team_id, count = category_order.len()))]
pub async fn update_sidebar_category_order(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
    category_order: &[String],
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    let existing_order = get_sidebar_category_order(&mut *tx, user_id, team_id).await?;

    if existing_order.len() != category_order.len() {
        return Err(StoreError::Argument {
            entity: "SidebarCategories",
            detail: "cannot update category order, passed list of categories different size than \
                     in DB",
        });
    }

    for original in &existing_order {
        if !category_order.contains(original) {
            return Err(StoreError::InvalidInput {
                entity: "SidebarCategories",
                field: "id",
                value: format!("{category_order:?}"),
            });
        }
    }

    update_sidebar_category_order_within(&mut tx, category_order).await?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    Ok(())
}

/// Port of `SqlChannelStore.UpdateSidebarCategories` (channel_store_categories.go:629) — the
/// route behind both `PUT …/categories` and `PUT …/categories/{category_id}`, and the only write
/// here that touches a second table's rows on purpose.
///
/// # Five fields are read-only and Go enforces that here, not in the model
///
/// `UserId`, `TeamId`, `SortOrder` and `Type` are copied back from the stored row, so a request
/// that renames its own `team_id` changes nothing. `DisplayName` is read-only unless the category
/// is `custom` — renaming Favorites is silently ignored. `Muted` is read-only for the
/// **Direct Messages** category and writable for every other type; `Sorting` and `Collapsed` are
/// always taken from the request.
///
/// A DM category's `channel_ids` is forced to `[]` in the answer, because its membership is
/// derived (see [`complete_populating_categories`]) rather than stored.
///
/// # The type consulted in the last two loops is the **request's**, not the row's
///
/// The channel-insert loop and the favourites-preference loop both branch on `category.Type`,
/// where `category` is the caller's struct — while everything written to `SidebarCategories`
/// above uses `srcCategory.Type`. So a request that mislabels a Channels category as
/// `direct_messages` has its channels deleted and not reinserted. Faithfully reproduced; it is
/// reachable from the API, since nothing validates `type` against the row.
///
/// # Favorites is mirrored into `Preferences`, and the two branches are not symmetrical
///
/// For a Favorites category the **original** channel list is deleted from `Preferences` and the
/// **new** one inserted, so a channel that stayed favourite is deleted and re-added. For any
/// other category the **new** list is deleted from `Preferences`, which is how dragging a channel
/// out of Favorites un-favourites it. Getting the two lists the wrong way round leaves stale
/// `favorite_channel` rows that the flagged-posts and webapp star icons read.
///
/// # Statement order
///
/// Categories are updated before channels are deleted, and the category updates are issued in
/// **id order** rather than request order. Both are deadlock avoidance against a concurrent
/// transaction touching the same tables — Go says so in two comments — and neither is observable
/// in a response.
#[tracing::instrument(skip(pool, categories), fields(user_id = %user_id, team_id = %team_id, count = categories.len()))]
pub async fn update_sidebar_categories(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
    categories: &[SidebarCategoryWithChannels],
) -> Result<SidebarCategoryUpdate, StoreError> {
    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    let mut updated: Vec<SidebarCategoryWithChannels> = Vec::with_capacity(categories.len());
    let mut original: Vec<SidebarCategoryWithChannels> = Vec::with_capacity(categories.len());

    for category in categories {
        let src = get_sidebar_category(&mut tx, &category.category.id)
            .await
            .map_err(|err| match err {
                StoreError::NotFound { criteria, .. } => StoreError::NotFound {
                    entity: "failed to find SidebarCategories",
                    criteria,
                },
                other => other,
            })?;

        let mut dest = category.category.clone();
        // "Prevent any changes to read-only fields of SidebarCategories".
        dest.user_id = src.category.user_id.clone();
        dest.team_id = src.category.team_id.clone();
        dest.sort_order = src.category.sort_order;
        dest.category_type = src.category.category_type.clone();
        dest.muted = src.category.muted;

        if dest.category_type != SIDEBAR_CATEGORY_CUSTOM {
            dest.display_name = src.category.display_name.clone();
        }

        let dest_channels = if dest.category_type == SIDEBAR_CATEGORY_DIRECT_MESSAGES {
            Vec::new()
        } else {
            dest.muted = category.category.muted;
            category.channel_ids.clone().unwrap_or_default()
        };

        updated.push(SidebarCategoryWithChannels {
            category: dest,
            // `make([]string, len(...))` — never nil, so this is `Some` even when empty.
            channel_ids: Some(dest_channels),
        });
        original.push(src);
    }

    // Sorted by id, for the deadlock reason in the doc comment. `str`'s ordering is byte-wise,
    // matching Go's `strings.Compare`.
    let mut sorted: Vec<&SidebarCategoryWithChannels> = updated.iter().collect();
    sorted.sort_by(|a, b| a.category.id.cmp(&b.category.id));

    for dest in sorted {
        sqlx::query!(
            r#"
            UPDATE sidebarcategories
            SET displayname = $1, sorting = $2, muted = $3, collapsed = $4
            WHERE id = $5
            "#,
            dest.category.display_name,
            dest.category.sorting,
            dest.category.muted,
            dest.category.collapsed,
            dest.category.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to update SidebarCategories".to_owned(),
            source,
        })?;
    }

    // One delete for every category named, "to prevent deadlocks" — and because moving a channel
    // between two categories in one request needs both cleared before either is refilled.
    let category_ids: Vec<String> = categories
        .iter()
        .map(|category| category.category.id.clone())
        .collect();
    sqlx::query!(
        "DELETE FROM sidebarchannels WHERE categoryid = ANY($1::text[])",
        &category_ids,
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to delete SidebarChannels".to_owned(),
        source,
    })?;

    for category in categories {
        // The **request's** type. See the doc comment.
        if category.category.category_type == SIDEBAR_CATEGORY_DIRECT_MESSAGES {
            // "The order of the DM category isn't stored explicitly, so there's nothing to do"
            continue;
        }
        let channel_ids = category.channel_ids.clone().unwrap_or_default();
        insert_sidebar_channels(&mut tx, &channel_ids, user_id, &category.category.id, true)
            .await?;
    }

    for (index, category) in categories.iter().enumerate() {
        let src = &original[index];
        let request_channels = category.channel_ids.clone().unwrap_or_default();

        if category.category.category_type == SIDEBAR_CATEGORY_FAVORITES {
            // Remove the favourites this category *used* to hold …
            delete_favorite_preferences(
                &mut tx,
                user_id,
                src.channel_ids.as_deref().unwrap_or_default(),
            )
            .await?;

            // … and add the ones it holds now. Go reaches through the `PreferenceStore`
            // abstraction to reuse its upsert inside this transaction; the statement is that
            // upsert, and `Preference.PreUpdate` is a no-op for this category (it rewrites
            // `Value` only for `theme`).
            for channel_id in &request_channels {
                sqlx::query!(
                    r#"
                    INSERT INTO preferences (userid, category, name, value)
                    VALUES ($1, $2, $3, 'true')
                    ON CONFLICT (userid, category, name) DO UPDATE SET value = 'true'
                    "#,
                    user_id,
                    PREFERENCE_CATEGORY_FAVORITE_CHANNEL,
                    channel_id,
                )
                .execute(&mut *tx)
                .await
                .map_err(|source| StoreError::Db {
                    context: "failed to save Preference".to_owned(),
                    source,
                })?;
            }
        } else {
            // "Remove any old favorites that might have been in this category" — the request's
            // list, not the stored one: these channels are now somewhere that is not Favorites.
            delete_favorite_preferences(&mut tx, user_id, &request_channels).await?;
        }
    }

    // "Ensure Channels are populated for Channels/Direct Messages category if they change" —
    // inside the transaction, so the orphan query sees the deletes and inserts above.
    complete_populating_categories(&mut *tx, user_id, team_id, &mut updated).await?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    Ok(SidebarCategoryUpdate { updated, original })
}

/// `DELETE FROM Preferences WHERE UserId = ? AND Name IN (…) AND Category = 'favorite_channel'`.
///
/// An empty list matches nothing on both servers: squirrel renders `sq.Eq` over an empty slice as
/// a false predicate, and `= ANY('{}')` is false for every row. Issuing the statement anyway
/// rather than guarding on emptiness keeps the two paths identical.
async fn delete_favorite_preferences(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    user_id: &str,
    channel_ids: &[String],
) -> Result<(), StoreError> {
    sqlx::query!(
        r#"
        DELETE FROM preferences
        WHERE userid = $1
          AND name = ANY($2::text[])
          AND category = $3
        "#,
        user_id,
        channel_ids,
        PREFERENCE_CATEGORY_FAVORITE_CHANNEL,
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to delete Preferences".to_owned(),
        source,
    })?;
    Ok(())
}

/// Port of `SqlChannelStore.DeleteSidebarCategory` (channel_store_categories.go:1013).
///
/// # Only a `custom` category can be deleted, and the refusal is a 400
///
/// `store.NewErrInvalidInput("SidebarCategory", "id", categoryId)`, which the app layer answers
/// **400**. Note the entity name is singular here and plural everywhere else in this file —
/// invisible on the wire, but it is what the Go source says.
///
/// A category id naming no row takes a different path: `GetBuilder` on zero rows is
/// `sql.ErrNoRows`, wrapped rather than converted, so the app layer answers **500**. Unreachable
/// through the API for an ordinary caller, because `SessionHasPermissionToCategory` fetches the
/// category first and refuses with a 403.
///
/// # The channels are not moved anywhere; they become orphans
///
/// `SidebarChannels` rows for the category are deleted outright. Every channel that was in it is
/// then in no category, so the next read's `getOrphanedSidebarChannels` appends it to the
/// Channels or DMs category in display-name order. That is where a deleted category's channels
/// "go", and it is why deleting a category does not lose them.
///
/// Category first, channels second — the deadlock ordering, stated in Go's comment.
#[tracing::instrument(skip(pool), fields(category_id = %category_id))]
pub async fn delete_sidebar_category(pool: &PgPool, category_id: &str) -> Result<(), StoreError> {
    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    let category_type: String = sqlx::query_scalar!(
        r#"SELECT type AS "category_type!" FROM sidebarcategories WHERE id = $1"#,
        category_id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find SidebarCategories with id={category_id}"),
        source,
    })?;

    if category_type != SIDEBAR_CATEGORY_CUSTOM {
        return Err(StoreError::InvalidInput {
            entity: "SidebarCategory",
            field: "id",
            value: category_id.to_owned(),
        });
    }

    sqlx::query!("DELETE FROM sidebarcategories WHERE id = $1", category_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to delete SidebarCategory".to_owned(),
            source,
        })?;

    sqlx::query!(
        "DELETE FROM sidebarchannels WHERE categoryid = $1",
        category_id
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to delete SidebarChannel".to_owned(),
        source,
    })?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two selectors are the only thing standing between a Favorites category and every
    /// channel the user is in — see the note on [`get_orphaned_sidebar_channels`]. Asserted here
    /// because the DB-backed suite cannot reach the branch: it returns before touching the pool,
    /// so a pool is not needed to test it.
    #[tokio::test]
    async fn neither_selector_set_issues_no_query_at_all() {
        // An unreachable database: if the guard were removed this would fail to connect rather
        // than return, which is exactly the distinction under test.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://127.0.0.1:1/none")
            .expect("a lazy pool needs no server");

        let orphans = get_orphaned_sidebar_channels(
            &pool,
            "y9i4er48tt8bukijy7i3u5y9ar",
            "n3ocs5fepw8qt1mb3psko5oq7y",
            false,
            false,
        )
        .await
        .expect("the guard returns before any query");
        assert!(orphans.is_empty());
    }

    /// A category whose type is neither `channels` nor `direct_messages` takes that same guard,
    /// so Favorites and custom categories never gain orphans.
    #[tokio::test]
    async fn a_favorites_category_gains_no_orphans() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://127.0.0.1:1/none")
            .expect("a lazy pool needs no server");

        for category_type in ["favorites", "custom", "managed", ""] {
            let mut category = SidebarCategoryWithChannels {
                category: SidebarCategory {
                    category_type: category_type.to_owned(),
                    ..SidebarCategory::default()
                },
                channel_ids: Some(vec!["already-here".to_owned()]),
            };
            complete_populating_category(&pool, &mut category)
                .await
                .expect("no query is issued for this type");
            assert_eq!(
                category.channel_ids,
                Some(vec!["already-here".to_owned()]),
                "{category_type} must not be populated"
            );
        }
    }
}
