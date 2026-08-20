//! `SqlPreferenceStore::{get_all, get_category, get}` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_preference_reads
//! ```
//!
//! What only this file can cover: each predicate column on its own. The parity suite reads the
//! fixture user's rows, where every `UserId` is the same value — so a `WHERE` clause missing the
//! user predicate, or comparing the wrong column, gives the same bytes as the right one. Rows
//! are seeded here so that every column differs from every other and a neighbour row shares
//! exactly one of them. Also the NULL `Value` row, which `Save` cannot produce and Go's scan
//! refuses.

use mm_store::{PreferenceStore, StoreError, preference_store::SqlPreferenceStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER_A: &str = "mmrsprefread00000000000usa";
const USER_B: &str = "mmrsprefread00000000000usb";
const USER_NULL: &str = "mmrsprefread0000000000null";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    sqlx::query("DELETE FROM preferences WHERE userid LIKE 'mmrsprefread%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

async fn seed(pool: &PgPool) {
    // (user, category, name, value). Every column holds a value that appears in no other column,
    // and each of USER_A's rows shares exactly one column with a USER_B row.
    let rows = [
        (USER_A, "cat_one", "name_one", "val_a_one_one"),
        (USER_A, "cat_one", "name_two", "val_a_one_two"),
        (USER_A, "cat_two", "name_one", "val_a_two_one"),
        (USER_B, "cat_one", "name_one", "val_b_one_one"),
        (USER_B, "cat_two", "name_two", "val_b_two_two"),
    ];
    for (user, category, name, value) in rows {
        sqlx::query(
            "INSERT INTO preferences (userid, category, name, value) VALUES ($1, $2, $3, $4)",
        )
        .bind(user)
        .bind(category)
        .bind(name)
        .bind(value)
        .execute(pool)
        .await
        .expect("inserts a preference");
    }
    sqlx::query("INSERT INTO preferences (userid, category, name, value) VALUES ($1, 'cat_one', 'name_one', NULL)")
        .bind(USER_NULL)
        .execute(pool)
        .await
        .expect("inserts the NULL-valued row");
}

fn triples(prefs: &mm_model::preference::Preferences) -> Vec<(String, String, String, String)> {
    let mut out: Vec<_> = prefs
        .iter()
        .map(|p| {
            (
                p.user_id.clone(),
                p.category.clone(),
                p.name.clone(),
                p.value.clone(),
            )
        })
        .collect();
    out.sort();
    out
}

fn owned(rows: &[(&str, &str, &str, &str)]) -> Vec<(String, String, String, String)> {
    let mut out: Vec<_> = rows
        .iter()
        .map(|(u, c, n, v)| {
            (
                (*u).to_owned(),
                (*c).to_owned(),
                (*n).to_owned(),
                (*v).to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn each_read_filters_on_exactly_its_own_columns() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlPreferenceStore::new(pool.clone());

    // GetAll: USER_A's three rows and none of USER_B's, across both categories.
    let all = store.get_all(USER_A).await.expect("get_all");
    assert_eq!(
        triples(&all),
        owned(&[
            (USER_A, "cat_one", "name_one", "val_a_one_one"),
            (USER_A, "cat_one", "name_two", "val_a_one_two"),
            (USER_A, "cat_two", "name_one", "val_a_two_one"),
        ])
    );

    // GetCategory: both predicates. USER_B has a `cat_one` row and USER_A has a `cat_two` row;
    // neither may appear.
    let category = store
        .get_category(USER_A, "cat_one")
        .await
        .expect("get_category");
    assert_eq!(
        triples(&category),
        owned(&[
            (USER_A, "cat_one", "name_one", "val_a_one_one"),
            (USER_A, "cat_one", "name_two", "val_a_one_two"),
        ])
    );

    // An unknown category is an empty list here — the 404 is the app layer's decision.
    let none = store
        .get_category(USER_A, "cat_none")
        .await
        .expect("get_category");
    assert!(none.is_empty());

    // Get: all three predicates. (USER_B, cat_one, name_one) exists too, with a different value.
    let one = store.get(USER_A, "cat_one", "name_one").await.expect("get");
    assert_eq!(
        (
            one.user_id.as_str(),
            one.category.as_str(),
            one.name.as_str(),
            one.value.as_str()
        ),
        (USER_A, "cat_one", "name_one", "val_a_one_one")
    );
    let other = store.get(USER_B, "cat_one", "name_one").await.expect("get");
    assert_eq!(other.value, "val_b_one_one");

    // A miss on any one predicate is NotFound.
    for (u, c, n) in [
        ("mmrsprefread00000000000usz", "cat_one", "name_one"),
        (USER_A, "cat_none", "name_one"),
        (USER_A, "cat_one", "name_none"),
        (USER_A, "cat_two", "name_two"),
    ] {
        let miss = store.get(u, c, n).await.expect_err("no such row");
        assert!(
            matches!(
                miss,
                StoreError::NotFound {
                    entity: "Preference",
                    ..
                }
            ),
            "{u}/{c}/{n}: {miss:?}"
        );
    }

    purge(&pool).await;
}

/// A NULL `Value` fails the whole read, the way Go's `string` scan does — not a `None`, not a
/// skipped row. All three reads, since each scans the same column.
#[tokio::test]
async fn a_null_value_fails_the_read_rather_than_becoming_empty() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlPreferenceStore::new(pool.clone());

    let all = store
        .get_all(USER_NULL)
        .await
        .expect_err("NULL is a scan error");
    assert!(matches!(all, StoreError::Db { .. }), "{all:?}");
    let category = store
        .get_category(USER_NULL, "cat_one")
        .await
        .expect_err("NULL is a scan error");
    assert!(matches!(category, StoreError::Db { .. }), "{category:?}");
    let one = store
        .get(USER_NULL, "cat_one", "name_one")
        .await
        .expect_err("NULL is a scan error");
    assert!(matches!(one, StoreError::Db { .. }), "{one:?}");

    purge(&pool).await;
}
