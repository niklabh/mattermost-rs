//! The five sidebar-category **writes** of `SqlChannelStore` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_sidebar_category_writes
//! ```
//!
//! # Why these are here and not only in the parity suite
//!
//! Most of what these functions decide is invisible in a response body:
//!
//! - **`CreateInitialSidebarCategories` is not reachable over HTTP at all** on the writing server.
//!   Go runs it lazily inside a `GET`, so a cross-server test cannot make our server run it
//!   without Go having run it first and left the rows behind.
//! - **Only the missing default types are inserted.** A user who has Favorites and Channels but
//!   no DMs category is a state no API call produces, and the wrong implementation — insert all
//!   three — fails on the primary key rather than answering differently.
//! - **The favourites migration reads `Preferences`**, filters through two joins and a
//!   `TeamId = ''` disjunct, and orders by display name. None of the drops are visible in a
//!   category listing: a dropped channel simply is not there, which is also what a correct answer
//!   looks like for a channel that was never favourited.
//! - **`UpdateSidebarCategories` writes `Preferences` rows** that no sidebar route reads back.
//!   The webapp's star icons and `getFlaggedPosts` read them, so a port that skipped the mirror
//!   would pass every sidebar test and silently unstar nothing.
//!
//! Every row here is `mmrssbw`-prefixed and purged before and after.

use mm_model::sidebar_category::{
    SIDEBAR_CATEGORY_CHANNELS, SIDEBAR_CATEGORY_CUSTOM, SIDEBAR_CATEGORY_DIRECT_MESSAGES,
    SIDEBAR_CATEGORY_FAVORITES, SIDEBAR_CATEGORY_SORT_ALPHABETICAL, SIDEBAR_CATEGORY_SORT_DEFAULT,
    SIDEBAR_CATEGORY_SORT_RECENT, SidebarCategory, SidebarCategoryWithChannels,
};
use mm_store::{SidebarCategoryStore, SqlSidebarCategoryStore, StoreError};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The fixture is mutated by every test, so they take turns.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrssbwuser00000000000user";
const TEAM: &str = "mmrssbwteam00000000000team";
/// Four public channels, named so that display-name order is *not* insertion order.
const ALPHA: &str = "mmrssbwchan0000000000alpha";
const BRAVO: &str = "mmrssbwchan0000000000bravo";
const CHARLIE: &str = "mmrssbwchan000000000charli";
const DELTA: &str = "mmrssbwchan0000000000delta";
/// A DM: `TeamId` is the empty string, which the favourites migration admits explicitly.
const DM: &str = "mmrssbwchan00000000000dmch";
/// A channel the user has a favourite preference for but is **not** a member of.
const LEFT: &str = "mmrssbwchan00000000000left";
/// A favourite preference naming a channel row that does not exist.
const GHOST: &str = "mmrssbwchan0000000000ghost";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM sidebarchannels WHERE userid LIKE 'mmrssbw%' OR channelid LIKE 'mmrssbw%' \
         OR categoryid LIKE '%mmrssbw%'",
        "DELETE FROM sidebarcategories WHERE userid LIKE 'mmrssbw%' OR teamid LIKE 'mmrssbw%'",
        "DELETE FROM preferences WHERE userid LIKE 'mmrssbw%'",
        "DELETE FROM channelmembers WHERE userid LIKE 'mmrssbw%' OR channelid LIKE 'mmrssbw%'",
        "DELETE FROM channels WHERE id LIKE 'mmrssbw%' OR teamid LIKE 'mmrssbw%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("the purge runs");
    }
}

/// Channels and memberships only — no categories, so every test decides its own starting state.
async fn seed(pool: &PgPool) {
    purge(pool).await;

    // Display names deliberately out of id order: `ORDER BY DisplayName` must be observable.
    for (id, team, display, name) in [
        (ALPHA, TEAM, "SBW Delta", "mmrssbw-alpha"),
        (BRAVO, TEAM, "SBW Charlie", "mmrssbw-bravo"),
        (CHARLIE, TEAM, "SBW Bravo", "mmrssbw-charlie"),
        (DELTA, TEAM, "SBW Alpha", "mmrssbw-delta"),
        (LEFT, TEAM, "SBW Left", "mmrssbw-left"),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, 0, $2, 'O'::channel_type, $3, $4, 0, 0)",
        )
        .bind(id)
        .bind(team)
        .bind(display)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the channel");
    }

    // The DM. Its `TeamId` is `''`, not this team's id.
    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                               name, totalmsgcount, totalmsgcountroot)
         VALUES ($1, 0, 0, 0, '', 'D'::channel_type, '', $2, 0, 0)",
    )
    .bind(DM)
    .bind(format!("{USER}__{USER}"))
    .execute(pool)
    .await
    .expect("inserts the DM");

    // Member of everything except `LEFT`.
    for id in [ALPHA, BRAVO, CHARLIE, DELTA, DM] {
        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser)
             VALUES ($1, $2, '', '{}'::jsonb, true)",
        )
        .bind(id)
        .bind(USER)
        .execute(pool)
        .await
        .expect("inserts the membership");
    }
}

fn store(pool: &PgPool) -> SqlSidebarCategoryStore {
    SqlSidebarCategoryStore::new(pool.clone())
}

fn default_id(kind: &str) -> String {
    format!("{kind}_{USER}_{TEAM}")
}

/// The `(id, sortorder, sorting, displayname, muted, collapsed, type)` of every category row the
/// user has on the team, in `SortOrder` order — read straight from the table, not through the
/// store, so the store's own grouping cannot hide a wrong column.
async fn rows(pool: &PgPool) -> Vec<(String, i64, String, String, bool, bool, String)> {
    sqlx::query_as(
        "SELECT id, sortorder, sorting, displayname, muted, collapsed, type
         FROM sidebarcategories WHERE userid = $1 AND teamid = $2 ORDER BY sortorder ASC",
    )
    .bind(USER)
    .bind(TEAM)
    .fetch_all(pool)
    .await
    .expect("the rows read")
}

/// `(categoryid, channelid, sortorder)` for every explicit sidebar-channel row of this user.
async fn channel_rows(pool: &PgPool) -> Vec<(String, String, i64)> {
    sqlx::query_as(
        "SELECT categoryid, channelid, sortorder FROM sidebarchannels
         WHERE userid = $1 ORDER BY categoryid, sortorder",
    )
    .bind(USER)
    .fetch_all(pool)
    .await
    .expect("the rows read")
}

async fn favourite_preferences(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT name FROM preferences WHERE userid = $1 AND category = 'favorite_channel'
         ORDER BY name",
    )
    .bind(USER)
    .fetch_all(pool)
    .await
    .expect("the rows read")
}

async fn add_favourite_preference(pool: &PgPool, channel_id: &str, value: &str) {
    sqlx::query(
        "INSERT INTO preferences (userid, category, name, value) VALUES ($1, 'favorite_channel', $2, $3)
         ON CONFLICT (userid, category, name) DO UPDATE SET value = $3",
    )
    .bind(USER)
    .bind(channel_id)
    .bind(value)
    .execute(pool)
    .await
    .expect("inserts the preference");
}

fn request(id: &str, category_type: &str, channels: &[&str]) -> SidebarCategoryWithChannels {
    SidebarCategoryWithChannels {
        category: SidebarCategory {
            id: id.to_owned(),
            user_id: USER.to_owned(),
            team_id: TEAM.to_owned(),
            category_type: category_type.to_owned(),
            ..SidebarCategory::default()
        },
        channel_ids: Some(channels.iter().map(|c| (*c).to_owned()).collect()),
    }
}

// ---------------------------------------------------------------------------
// CreateInitialSidebarCategories
// ---------------------------------------------------------------------------

/// The three defaults, their deterministic ids, and the three fields that differ between them.
#[tokio::test]
async fn the_three_default_categories_carry_gos_ids_orders_and_sortings() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    let ordered = store(&pool)
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the migration runs");

    assert_eq!(
        ordered.order,
        Some(vec![
            default_id(SIDEBAR_CATEGORY_FAVORITES),
            default_id(SIDEBAR_CATEGORY_CHANNELS),
            default_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES),
        ]),
        "Favorites, Channels, Direct Messages — the order is the SortOrder"
    );

    let rows = rows(&pool).await;
    assert_eq!(
        rows,
        vec![
            (
                default_id(SIDEBAR_CATEGORY_FAVORITES),
                0,
                SIDEBAR_CATEGORY_SORT_DEFAULT.to_owned(),
                "Favorites".to_owned(),
                false,
                false,
                SIDEBAR_CATEGORY_FAVORITES.to_owned()
            ),
            (
                default_id(SIDEBAR_CATEGORY_CHANNELS),
                10,
                SIDEBAR_CATEGORY_SORT_DEFAULT.to_owned(),
                "Channels".to_owned(),
                false,
                false,
                SIDEBAR_CATEGORY_CHANNELS.to_owned()
            ),
            (
                // "Direct Messages" with a space, and `recent` where the other two are `""`.
                default_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES),
                20,
                SIDEBAR_CATEGORY_SORT_RECENT.to_owned(),
                "Direct Messages".to_owned(),
                false,
                false,
                SIDEBAR_CATEGORY_DIRECT_MESSAGES.to_owned()
            ),
        ]
    );

    // The ids are what `IsValidCategoryId`'s second branch matches, so the write and the URL
    // validator agree — a category whose id this rejected would be unreachable by URL.
    for kind in [
        SIDEBAR_CATEGORY_FAVORITES,
        SIDEBAR_CATEGORY_CHANNELS,
        SIDEBAR_CATEGORY_DIRECT_MESSAGES,
    ] {
        assert!(
            mm_model::sidebar_category::is_valid_category_id(&default_id(kind)),
            "{kind}"
        );
    }

    // Nothing explicit was written: the whole sidebar is orphans.
    assert!(channel_rows(&pool).await.is_empty());
    let categories = ordered.categories.expect("categories");
    assert_eq!(
        categories[1]
            .channel_ids
            .as_deref()
            .unwrap_or_default()
            .len(),
        4,
        "the four public channels arrive as orphans of the Channels category"
    );
    assert_eq!(
        categories[2].channel_ids,
        Some(vec![DM.to_owned()]),
        "and the DM as an orphan of Direct Messages"
    );

    purge(&pool).await;
}

/// Only the missing types are inserted, and an existing category is left exactly as it was.
///
/// # Three runs, because one of them proves the guard and another proves the insert
///
/// The `SELECT type` up front is what stops a run duplicating a category, and the primary key on
/// `SidebarCategories.Id` is what makes getting it wrong an **error** rather than a second row. So
/// the case that catches a dropped guard is a run with **nothing missing**: it must succeed and
/// write nothing.
///
/// The first version of this test deleted the Channels category before its only second run, which
/// made `if !hasCategoryOfType[channels]` unreachable — mutating it to `if true` changed nothing,
/// because the category really was missing. That survivor is why the no-deletion run exists.
#[tokio::test]
async fn a_second_run_inserts_only_what_is_missing_and_rewrites_nothing() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);

    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the first run");

    // Rename Favorites and move it to the end — a state no API call produces, which is exactly
    // why it is worth constructing: it makes "rewrites nothing" observable.
    sqlx::query(
        "UPDATE sidebarcategories SET displayname = 'Renamed', sortorder = 99 WHERE id = $1",
    )
    .bind(default_id(SIDEBAR_CATEGORY_FAVORITES))
    .execute(&pool)
    .await
    .expect("renames");

    // Nothing is missing. All three guards must hold, or the insert hits the primary key and this
    // is an `Err`.
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("a run with all three present inserts nothing and does not conflict");

    let untouched = rows(&pool).await;
    assert_eq!(untouched.len(), 3, "no duplicates: {untouched:?}");
    assert_eq!(
        untouched
            .iter()
            .find(|row| row.0 == default_id(SIDEBAR_CATEGORY_FAVORITES))
            .map(|row| (row.3.as_str(), row.1)),
        Some(("Renamed", 99)),
        "an existing category is not rewritten, name or order"
    );

    // Now drop one, and only that one comes back.
    sqlx::query("DELETE FROM sidebarcategories WHERE id = $1")
        .bind(default_id(SIDEBAR_CATEGORY_CHANNELS))
        .execute(&pool)
        .await
        .expect("deletes");

    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the third run");

    let rows = rows(&pool).await;
    assert_eq!(rows.len(), 3, "the missing one came back and nothing else");
    let by_id = |id: String| {
        rows.iter()
            .find(|row| row.0 == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} is missing"))
    };
    let favorites = by_id(default_id(SIDEBAR_CATEGORY_FAVORITES));
    assert_eq!(
        favorites.3, "Renamed",
        "an existing category is not rewritten"
    );
    assert_eq!(favorites.1, 99, "nor is its SortOrder");
    let channels = by_id(default_id(SIDEBAR_CATEGORY_CHANNELS));
    assert_eq!(channels.1, 10, "the reinserted one takes its default order");
    assert_eq!(channels.3, "Channels");

    purge(&pool).await;
}

/// The favourites migration: what it keeps, what each join drops, and the order it writes.
#[tokio::test]
async fn favourite_preferences_become_sidebar_channels_in_display_name_order() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    // Kept: two on this team plus a DM, whose `TeamId` is `''`.
    add_favourite_preference(&pool, ALPHA, "true").await; // display name "SBW Delta"
    add_favourite_preference(&pool, DELTA, "true").await; // display name "SBW Alpha"
    add_favourite_preference(&pool, DM, "true").await; // display name ""
    // Dropped, one per predicate: not a member, no channel row, and a value that is not `true`.
    add_favourite_preference(&pool, LEFT, "true").await;
    add_favourite_preference(&pool, GHOST, "true").await;
    add_favourite_preference(&pool, BRAVO, "false").await;

    let ordered = store(&pool)
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the migration runs");

    let favorites = ordered
        .categories
        .expect("categories")
        .into_iter()
        .find(|category| category.category.category_type == SIDEBAR_CATEGORY_FAVORITES)
        .expect("a Favorites category");
    assert_eq!(
        favorites.channel_ids,
        // `ORDER BY Channels.DisplayName`: the DM's is `""`, then "SBW Alpha", then "SBW Delta".
        Some(vec![DM.to_owned(), DELTA.to_owned(), ALPHA.to_owned()]),
        "display-name order, and only the three that survive the joins"
    );

    let written = channel_rows(&pool).await;
    assert_eq!(
        written,
        vec![
            (default_id(SIDEBAR_CATEGORY_FAVORITES), DM.to_owned(), 0),
            (default_id(SIDEBAR_CATEGORY_FAVORITES), DELTA.to_owned(), 10),
            (default_id(SIDEBAR_CATEGORY_FAVORITES), ALPHA.to_owned(), 20),
        ],
        "SortOrder counts up in MinimalSidebarSortDistance steps from zero"
    );

    purge(&pool).await;
}

// ---------------------------------------------------------------------------
// CreateSidebarCategory
// ---------------------------------------------------------------------------

/// Placed second, renumbered, forced to `custom`, and it steals its channels from wherever they
/// were.
#[tokio::test]
async fn a_new_category_lands_after_favorites_and_takes_its_channels_from_their_old_one() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the defaults exist");

    // File BRAVO explicitly under Channels first, so the delete-from-previous-category step has
    // something to delete. An orphan would leave nothing to observe.
    store
        .update_sidebar_categories(
            USER,
            TEAM,
            &[request(
                &default_id(SIDEBAR_CATEGORY_CHANNELS),
                SIDEBAR_CATEGORY_CHANNELS,
                &[BRAVO],
            )],
        )
        .await
        .expect("Channels gains an explicit member");

    let mut new_category = request("ignored-id", SIDEBAR_CATEGORY_FAVORITES, &[BRAVO, ALPHA]);
    new_category.category.display_name = "SBW Custom".to_owned();
    new_category.category.sorting = SIDEBAR_CATEGORY_SORT_ALPHABETICAL.to_owned();
    new_category.category.muted = true;
    new_category.category.collapsed = true;
    new_category.category.sort_order = 4242;

    let created = store
        .create_sidebar_category(USER, TEAM, &new_category)
        .await
        .expect("the category is created");

    assert_ne!(created.category.id, "ignored-id", "the store mints the id");
    assert_eq!(
        created.category.id.len(),
        26,
        "a NewId, not a default-shaped id"
    );
    assert_eq!(
        created.category.category_type, SIDEBAR_CATEGORY_CUSTOM,
        "the request said favorites; a client cannot create a second one"
    );
    assert!(
        !created.category.collapsed,
        "collapsed is absent from Go's literal"
    );
    assert!(created.category.muted, "muted is taken from the request");
    assert_eq!(created.category.sorting, SIDEBAR_CATEGORY_SORT_ALPHABETICAL);
    assert_eq!(created.category.display_name, "SBW Custom");
    assert_eq!(
        created.category.sort_order, 10,
        "the patched value: second, behind Favorites — not 10 × the category count"
    );
    assert_eq!(
        created.channel_ids,
        Some(vec![BRAVO.to_owned(), ALPHA.to_owned()]),
        "the answer echoes the request's list, in the request's order"
    );

    let rows = rows(&pool).await;
    let order: Vec<(&str, i64)> = rows.iter().map(|row| (row.0.as_str(), row.1)).collect();
    assert_eq!(
        order,
        vec![
            (default_id(SIDEBAR_CATEGORY_FAVORITES).as_str(), 0),
            (created.category.id.as_str(), 10),
            (default_id(SIDEBAR_CATEGORY_CHANNELS).as_str(), 20),
            (default_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES).as_str(), 30),
        ],
        "every category is renumbered, and the row agrees with the answer"
    );

    assert_eq!(
        channel_rows(&pool).await,
        vec![
            (created.category.id.clone(), BRAVO.to_owned(), 0),
            (created.category.id.clone(), ALPHA.to_owned(), 10),
        ],
        "BRAVO left the Channels category rather than being in two at once"
    );

    purge(&pool).await;
}

/// With something other than Favorites at the head, the new category goes to position zero.
#[tokio::test]
async fn a_new_category_goes_first_when_favorites_is_not_first() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the defaults exist");

    store
        .update_sidebar_category_order(
            USER,
            TEAM,
            &[
                default_id(SIDEBAR_CATEGORY_CHANNELS),
                default_id(SIDEBAR_CATEGORY_FAVORITES),
                default_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES),
            ],
        )
        .await
        .expect("the order is set");

    let created = store
        .create_sidebar_category(USER, TEAM, &request("", SIDEBAR_CATEGORY_CUSTOM, &[]))
        .await
        .expect("the category is created");
    assert_eq!(created.category.sort_order, 0, "first, not second");

    let rows = rows(&pool).await;
    assert_eq!(rows[0].0, created.category.id);
    assert_eq!(rows[0].1, 0);
    assert_eq!(rows[1].0, default_id(SIDEBAR_CATEGORY_CHANNELS));

    purge(&pool).await;
}

/// A user with no categories on the team cannot create one — the not-found the app layer turns
/// into a 404.
#[tokio::test]
async fn creating_a_category_for_a_user_with_no_categories_is_a_not_found() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    let err = store(&pool)
        .create_sidebar_category(USER, TEAM, &request("", SIDEBAR_CATEGORY_CUSTOM, &[]))
        .await
        .expect_err("there are no categories to place it among");
    assert!(err.is_not_found(), "{err}");

    purge(&pool).await;
}

// ---------------------------------------------------------------------------
// UpdateSidebarCategoryOrder
// ---------------------------------------------------------------------------

/// The length guard is a 500-shaped error and the membership guard a 400-shaped one, and the
/// length guard runs first.
#[tokio::test]
async fn the_order_guards_are_length_first_then_membership_and_they_differ_in_kind() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the defaults exist");

    let err = store
        .update_sidebar_category_order(USER, TEAM, &[default_id(SIDEBAR_CATEGORY_FAVORITES)])
        .await
        .expect_err("one id against three categories");
    assert!(
        !err.is_invalid_input() && !err.is_not_found(),
        "a bare errors.New, which the app layer answers 500 to: {err}"
    );

    // Right length, and one id that is not the user's. The other two are, so this can only be
    // caught by the membership loop.
    let err = store
        .update_sidebar_category_order(
            USER,
            TEAM,
            &[
                default_id(SIDEBAR_CATEGORY_FAVORITES),
                default_id(SIDEBAR_CATEGORY_CHANNELS),
                "mmrssbwcategory000000notmi".to_owned(),
            ],
        )
        .await
        .expect_err("a foreign category id");
    assert!(err.is_invalid_input(), "the 400-shaped one: {err}");

    // And nothing was written by either refusal.
    let before: Vec<i64> = rows(&pool).await.iter().map(|row| row.1).collect();
    assert_eq!(before, vec![0, 10, 20]);

    store
        .update_sidebar_category_order(
            USER,
            TEAM,
            &[
                default_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES),
                default_id(SIDEBAR_CATEGORY_FAVORITES),
                default_id(SIDEBAR_CATEGORY_CHANNELS),
            ],
        )
        .await
        .expect("a complete permutation is accepted");
    let after: Vec<(String, i64)> = rows(&pool)
        .await
        .into_iter()
        .map(|row| (row.0, row.1))
        .collect();
    assert_eq!(
        after,
        vec![
            (default_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES), 0),
            (default_id(SIDEBAR_CATEGORY_FAVORITES), 10),
            (default_id(SIDEBAR_CATEGORY_CHANNELS), 20),
        ]
    );

    purge(&pool).await;
}

// ---------------------------------------------------------------------------
// UpdateSidebarCategories
// ---------------------------------------------------------------------------

/// The read-only fields, and the `Preferences` mirror in both directions.
#[tokio::test]
async fn updating_favorites_mirrors_preferences_and_refuses_to_rename_it() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the defaults exist");

    let favorites = default_id(SIDEBAR_CATEGORY_FAVORITES);
    let mut update = request(&favorites, SIDEBAR_CATEGORY_FAVORITES, &[ALPHA, BRAVO]);
    update.category.display_name = "Renamed By A Client".to_owned();
    update.category.user_id = "mmrssbwuser000000000other".to_owned();
    update.category.team_id = "mmrssbwteam000000000other".to_owned();
    update.category.sort_order = 777;
    update.category.category_type = SIDEBAR_CATEGORY_FAVORITES.to_owned();
    update.category.muted = true;
    update.category.collapsed = true;
    update.category.sorting = SIDEBAR_CATEGORY_SORT_ALPHABETICAL.to_owned();

    let result = store
        .update_sidebar_categories(USER, TEAM, std::slice::from_ref(&update))
        .await
        .expect("the update runs");
    let updated = &result.updated[0];

    assert_eq!(
        updated.category.display_name, "Favorites",
        "DisplayName is read-only for a non-custom category"
    );
    assert_eq!(updated.category.user_id, USER, "UserId is read-only");
    assert_eq!(updated.category.team_id, TEAM, "TeamId is read-only");
    assert_eq!(updated.category.sort_order, 0, "SortOrder is read-only");
    assert!(updated.category.muted, "muted is writable for Favorites");
    assert!(updated.category.collapsed, "and collapsed always is");
    assert_eq!(updated.category.sorting, SIDEBAR_CATEGORY_SORT_ALPHABETICAL);
    assert_eq!(
        updated.channel_ids,
        Some(vec![ALPHA.to_owned(), BRAVO.to_owned()]),
        "Favorites gains no orphans, so this is exactly the request"
    );
    assert_eq!(
        result.original[0].category.display_name, "Favorites",
        "and the `original` half is the row as it was"
    );

    assert_eq!(
        favourite_preferences(&pool).await,
        vec![ALPHA.to_owned(), BRAVO.to_owned()],
        "the two channels are now `favorite_channel` preferences"
    );

    // Drop one. The delete uses the **original** list, so BRAVO's row must go and ALPHA's must
    // survive being deleted and re-added.
    let keep = request(&favorites, SIDEBAR_CATEGORY_FAVORITES, &[ALPHA]);
    store
        .update_sidebar_categories(USER, TEAM, std::slice::from_ref(&keep))
        .await
        .expect("the second update runs");
    assert_eq!(favourite_preferences(&pool).await, vec![ALPHA.to_owned()]);

    // Move ALPHA out of Favorites into the Channels category in one request. The non-Favorites
    // branch deletes the **request's** channels from `Preferences`, which is what un-stars it.
    store
        .update_sidebar_categories(
            USER,
            TEAM,
            &[
                request(&favorites, SIDEBAR_CATEGORY_FAVORITES, &[]),
                request(
                    &default_id(SIDEBAR_CATEGORY_CHANNELS),
                    SIDEBAR_CATEGORY_CHANNELS,
                    &[ALPHA],
                ),
            ],
        )
        .await
        .expect("the move runs");
    assert!(
        favourite_preferences(&pool).await.is_empty(),
        "dragging a channel out of Favorites un-favourites it"
    );
    assert_eq!(
        channel_rows(&pool).await,
        vec![(default_id(SIDEBAR_CATEGORY_CHANNELS), ALPHA.to_owned(), 0)],
        "and it is filed under Channels alone"
    );

    purge(&pool).await;
}

/// The Direct Messages category: `muted` is read-only and `channel_ids` is answered from the
/// orphan query rather than from the request.
#[tokio::test]
async fn the_dm_category_ignores_muted_and_stores_no_channels() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the defaults exist");

    let dms = default_id(SIDEBAR_CATEGORY_DIRECT_MESSAGES);
    let mut update = request(&dms, SIDEBAR_CATEGORY_DIRECT_MESSAGES, &[DM]);
    update.category.muted = true;
    update.category.collapsed = true;

    let result = store
        .update_sidebar_categories(USER, TEAM, std::slice::from_ref(&update))
        .await
        .expect("the update runs");
    let updated = &result.updated[0];

    assert!(
        !updated.category.muted,
        "muted is read-only for Direct Messages — Go skips the assignment for this type alone"
    );
    assert!(updated.category.collapsed, "collapsed is still writable");
    assert_eq!(
        updated.channel_ids,
        Some(vec![DM.to_owned()]),
        "the DM arrives through the orphan query, not from the request"
    );
    assert!(
        channel_rows(&pool).await.is_empty(),
        "and nothing was stored: the DM category's membership is derived"
    );

    purge(&pool).await;
}

/// The quirk: the channel-insert loop reads the **request's** `type`, so mislabelling a category
/// as `direct_messages` empties it.
#[tokio::test]
async fn a_category_mislabelled_direct_messages_has_its_channels_deleted_and_not_restored() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the defaults exist");

    let custom = store
        .create_sidebar_category(
            USER,
            TEAM,
            &request("", SIDEBAR_CATEGORY_CUSTOM, &[ALPHA, BRAVO]),
        )
        .await
        .expect("a custom category with two channels");
    assert_eq!(channel_rows(&pool).await.len(), 2);

    // The row's type is `custom`; the request claims `direct_messages`.
    let result = store
        .update_sidebar_categories(
            USER,
            TEAM,
            &[request(
                &custom.category.id,
                SIDEBAR_CATEGORY_DIRECT_MESSAGES,
                &[ALPHA, BRAVO],
            )],
        )
        .await
        .expect("the update runs");

    assert_eq!(
        result.updated[0].category.category_type, SIDEBAR_CATEGORY_CUSTOM,
        "the stored type wins for everything written to SidebarCategories"
    );
    assert_eq!(
        result.updated[0].channel_ids,
        Some(vec![ALPHA.to_owned(), BRAVO.to_owned()]),
        "and the answer still lists them, because `dest` was built from the stored type"
    );
    assert!(
        channel_rows(&pool).await.is_empty(),
        "but the insert loop skipped on the request's type, so the rows are gone"
    );

    purge(&pool).await;
}

// ---------------------------------------------------------------------------
// DeleteSidebarCategory
// ---------------------------------------------------------------------------

/// A default category cannot be deleted; a custom one can, and its channels become orphans.
#[tokio::test]
async fn only_a_custom_category_deletes_and_its_channels_become_orphans() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = store(&pool);
    store
        .create_initial_sidebar_categories(USER, TEAM)
        .await
        .expect("the defaults exist");

    for kind in [
        SIDEBAR_CATEGORY_FAVORITES,
        SIDEBAR_CATEGORY_CHANNELS,
        SIDEBAR_CATEGORY_DIRECT_MESSAGES,
    ] {
        let err = store
            .delete_sidebar_category(&default_id(kind))
            .await
            .expect_err("a default category is not deletable");
        assert!(err.is_invalid_input(), "{kind}: {err}");
    }
    assert_eq!(rows(&pool).await.len(), 3, "and none of them was deleted");

    let custom = store
        .create_sidebar_category(
            USER,
            TEAM,
            &request("", SIDEBAR_CATEGORY_CUSTOM, &[CHARLIE]),
        )
        .await
        .expect("a custom category");
    store
        .delete_sidebar_category(&custom.category.id)
        .await
        .expect("a custom category deletes");

    assert_eq!(rows(&pool).await.len(), 3, "back to the three defaults");
    assert!(
        channel_rows(&pool).await.is_empty(),
        "the category's SidebarChannels rows go with it"
    );

    let ordered = store
        .get_sidebar_categories(USER, TEAM)
        .await
        .expect("the sidebar reads");
    let channels = ordered
        .categories
        .expect("categories")
        .into_iter()
        .find(|category| category.category.category_type == SIDEBAR_CATEGORY_CHANNELS)
        .expect("a Channels category");
    assert!(
        channels
            .channel_ids
            .as_deref()
            .unwrap_or_default()
            .contains(&CHARLIE.to_owned()),
        "the deleted category's channel is an orphan now, filed under Channels: {:?}",
        channels.channel_ids
    );

    purge(&pool).await;
}

/// A category id naming no row is **not** the invalid-input refusal: it is a wrapped
/// `sql.ErrNoRows`, which the app layer answers 500 to.
#[tokio::test]
async fn deleting_a_category_that_does_not_exist_is_not_the_invalid_input_branch() {
    if !db_enabled() {
        return;
    }
    let _lock = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    let err: StoreError = store(&pool)
        .delete_sidebar_category("mmrssbwcategory00000nowher")
        .await
        .expect_err("there is no such category");
    assert!(
        !err.is_invalid_input() && !err.is_not_found(),
        "GetBuilder's ErrNoRows is wrapped, not converted: {err}"
    );

    purge(&pool).await;
}
