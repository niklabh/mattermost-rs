//! Port of the sidebar-category **reads** of `SqlChannelStore`
//! (channels/store/sqlstore/channel_store_categories.go): `GetSidebarCategories`,
//! `GetSidebarCategory` and `GetSidebarCategoryOrder`, plus the
//! `completePopulatingCategor{y,ies}T` / `getOrphanedSidebarChannels` machinery they all end in.
//!
//! Go hangs these off `ChannelStore`; they live in their own store here so that the sidebar
//! routes and the channel routes can be migrated independently.
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
use mm_model::sidebar_category::{
    OrderedSidebarCategories, SIDEBAR_CATEGORY_CHANNELS, SIDEBAR_CATEGORY_DIRECT_MESSAGES,
    SidebarCategory, SidebarCategoryWithChannels,
};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.ChannelStore` sidebar surface that is ported: the three reads.
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
        get_sidebar_categories(&self.pool, user_id, team_id).await
    }

    #[tracing::instrument(skip_all, fields(category_id = %category_id))]
    async fn get_sidebar_category(
        &self,
        category_id: &str,
    ) -> Result<SidebarCategoryWithChannels, StoreError> {
        get_sidebar_category(&self.pool, category_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
    async fn get_sidebar_category_order(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        get_sidebar_category_order(&self.pool, user_id, team_id).await
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
#[tracing::instrument(skip(pool), fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn get_sidebar_categories(
    pool: &PgPool,
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
    .fetch_all(pool)
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

    complete_populating_categories(pool, user_id, team_id, &mut categories).await?;

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
#[tracing::instrument(skip(pool), fields(category_id = %category_id))]
pub async fn get_sidebar_category(
    pool: &PgPool,
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
    .fetch_all(pool)
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

    complete_populating_category(pool, &mut category).await?;
    Ok(category)
}

/// Port of `getSidebarCategoryOrderT` (channel_store_categories.go:554).
///
/// The one read that does **not** touch `SidebarChannels` and does not populate orphans: it is
/// the category ids alone, in `SortOrder` order. `ids := []string{}` in Go, so an empty answer is
/// `[]` and never `null`.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn get_sidebar_category_order(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
) -> Result<Vec<String>, StoreError> {
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
    .fetch_all(pool)
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
async fn complete_populating_category(
    pool: &PgPool,
    category: &mut SidebarCategoryWithChannels,
) -> Result<(), StoreError> {
    let orphans = get_orphaned_sidebar_channels(
        pool,
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
async fn complete_populating_categories(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
    categories: &mut [SidebarCategoryWithChannels],
) -> Result<(), StoreError> {
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
        pool,
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
#[tracing::instrument(skip(pool), fields(user_id = %user_id, team_id = %team_id, count))]
async fn get_orphaned_sidebar_channels(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
    select_channels: bool,
    select_dms: bool,
) -> Result<Vec<OrphanedSidebarChannel>, StoreError> {
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
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "Failed to get orphaned sidebar channels".to_owned(),
        source,
    })?;

    tracing::Span::current().record("count", rows.len());
    Ok(rows)
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
