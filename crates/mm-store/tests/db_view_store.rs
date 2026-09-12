//! `SqlViewStore.get_for_channel`'s **ordering**, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_view_store
//! ```
//!
//! # Why this file exists
//!
//! The view store shipped with **no DB-backed test at all**, and two things followed from that.
//! `parity_views`' two list comparisons disagree with Go on row order on every run ([D-330]), and
//! `store-list-drops-the-createat-tiebreak` is the one non-control mutation in
//! `scripts/mutations/view-routes.plan` that survives. Both are the same gap seen from different
//! sides: nothing asserted what order the store returns.
//!
//! # The fixture is built so that the three orderings disagree
//!
//! `ORDER BY sortorder ASC, createat ASC, id ASC` is only testable if the three keys would each
//! produce a *different* answer. So every row here shares `sortorder = 0`, their `createat` values
//! ascend in one order, and their ids ascend in the **opposite** order:
//!
//! | id | createat | by createat | by id |
//! |---|---|---|---|
//! | `…zzz` | 1000 | 1st | 3rd |
//! | `…mmm` | 2000 | 2nd | 2nd |
//! | `…aaa` | 3000 | 3rd | 1st |
//!
//! A store that honours the `createat` tiebreak answers zzz, mmm, aaa. One that has fallen through
//! to `id` answers aaa, mmm, zzz. With equal `createat` values — which is what a fixture built by
//! three quick API calls can accidentally produce — both answers are the same and the test proves
//! nothing, which is the trap this table exists to avoid.
//!
//! # Every test here is named `view_store_*`
//!
//! `MUTATE_FILTER` selects test **names**, not files. A filter naming this file matches nothing,
//! cargo runs zero tests, and every mutation is reported SURVIVED.

use mm_model::view::ViewQueryOpts;
use mm_store::{SqlViewStore, ViewStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A channel id that names no channel. `get_for_channel` filters on `channelid` and joins nothing,
/// so the rows are reachable without a `Channels` row — and planting one would put a channel with
/// no team into lists that other suites byte-compare.
const CHANNEL: &str = "mmrsviewstorechan000000001";

const VIEW_LATE_ID: &str = "mmrsviewstore0000000000zzz";
const VIEW_MID_ID: &str = "mmrsviewstore0000000000mmm";
const VIEW_EARLY_ID: &str = "mmrsviewstore0000000000aaa";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

/// Scoped to this file's own id prefix, not to `mmrsview%`: the parity binary plants views of its
/// own through the API and runs concurrently with this one.
async fn purge(pool: &PgPool) {
    sqlx::query("DELETE FROM views WHERE id LIKE 'mmrsviewstore%' OR channelid = $1")
        .bind(CHANNEL)
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

/// One view row, with `sortorder` and `createat` given explicitly.
async fn plant_view(pool: &PgPool, id: &str, title: &str, sort_order: i32, create_at: i64) {
    sqlx::query(
        "INSERT INTO views
            (id, channelid, type, creatorid, title, description, sortorder, props,
             createat, updateat, deleteat)
         VALUES ($1, $2, 'kanban', 'mmrsviewstorecreator000001', $3, 'planted', $4,
                 '{}'::jsonb, $5, $5, 0)",
    )
    .bind(id)
    .bind(CHANNEL)
    .bind(title)
    .bind(sort_order)
    .bind(create_at)
    .execute(pool)
    .await
    .expect("plants a view");
}

fn all_pages() -> ViewQueryOpts {
    ViewQueryOpts {
        page: 0,
        // Zero means "the store's default", which is larger than this fixture.
        per_page: 0,
    }
}

/// **The tiebreak is `CreateAt`, not `Id`.** This is [D-330]'s assertion, at the layer that owns
/// the `ORDER BY`.
#[tokio::test]
async fn view_store_breaks_a_sortorder_tie_by_createat_not_by_id() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    // Planted in id order, which is the reverse of createat order, so an insertion-order answer is
    // also distinguishable from the right one.
    plant_view(&pool, VIEW_EARLY_ID, "aaa", 0, 3000).await;
    plant_view(&pool, VIEW_MID_ID, "mmm", 0, 2000).await;
    plant_view(&pool, VIEW_LATE_ID, "zzz", 0, 1000).await;

    let store = SqlViewStore::new(pool.clone());
    let views = store
        .get_for_channel(CHANNEL, &all_pages())
        .await
        .expect("the planted rows come back");

    let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![VIEW_LATE_ID, VIEW_MID_ID, VIEW_EARLY_ID],
        "tied on sortorder, the order is createat ascending (1000, 2000, 3000); \
         getting the reverse means the tiebreak fell through to id"
    );

    purge(&pool).await;
}

/// `sortorder` outranks `createat`: the newest row sorts first when its `sortorder` is lower.
///
/// Without this, a store ordering by `createat` alone passes the test above.
#[tokio::test]
async fn view_store_sortorder_outranks_the_createat_tiebreak() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    // The newest row, given the lowest sortorder.
    plant_view(&pool, VIEW_EARLY_ID, "aaa", 1, 3000).await;
    plant_view(&pool, VIEW_MID_ID, "mmm", 5, 2000).await;
    plant_view(&pool, VIEW_LATE_ID, "zzz", 9, 1000).await;

    let store = SqlViewStore::new(pool.clone());
    let views = store
        .get_for_channel(CHANNEL, &all_pages())
        .await
        .expect("the planted rows come back");

    let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![VIEW_EARLY_ID, VIEW_MID_ID, VIEW_LATE_ID],
        "sortorder 1, 5, 9 decides, and createat 3000, 2000, 1000 does not"
    );

    purge(&pool).await;
}

/// A soft-deleted view is not listed, and the count agrees with the list.
#[tokio::test]
async fn view_store_skips_a_soft_deleted_row_in_both_the_list_and_the_count() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    plant_view(&pool, VIEW_LATE_ID, "zzz", 0, 1000).await;
    plant_view(&pool, VIEW_MID_ID, "mmm", 0, 2000).await;
    sqlx::query("UPDATE views SET deleteat = 4242 WHERE id = $1")
        .bind(VIEW_MID_ID)
        .execute(&pool)
        .await
        .expect("soft-deletes one row");

    let store = SqlViewStore::new(pool.clone());
    let views = store
        .get_for_channel(CHANNEL, &all_pages())
        .await
        .expect("the live row comes back");
    let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(ids, vec![VIEW_LATE_ID], "the deleted row is not listed");

    let count = store
        .count_for_channel(CHANNEL, &all_pages())
        .await
        .expect("the count is readable");
    assert_eq!(count, 1, "and it is not counted either");

    purge(&pool).await;
}
