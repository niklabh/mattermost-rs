//! `SqlPluginStore` against a real Postgres: expiry in both directions, the NULL `ExpireAt` a
//! pre-6.x row can carry, compare-and-set's two paths, paging and the delete-all scope.
//!
//! ```sh
//! MM_STORE_DB=1 cargo test -p mm-store --test db_plugin_store
//! ```
//!
//! # The expiry fixture straddles now
//!
//! Every expiry predicate is `ExpireAt = 0 OR ExpireAt > now` (or its negation), so each test that
//! reads one plants a row an hour in the past **and** a row an hour in the future. A reversed
//! comparison then hides the live row and shows the dead one, rather than agreeing with the right
//! answer on a fixture where only one side exists.
//!
//! # Every test here is named `plugin_store_*`
//!
//! `MUTATE_FILTER` selects test **names**, not files. Each test owns its own plugin id, so the
//! tests run concurrently without seeing each other's rows.

use mm_model::plugin_key_value::PluginKeyValue;
use mm_model::plugin_kvset_options::PluginKVSetOptions;
use mm_model::utils::get_millis;
use mm_store::{PluginStore, SqlPluginStore, StoreError};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const HOUR: i64 = 3_600_000;

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

/// A plugin id of this file's own, emptied: `mmrs.kvstore.<test>`.
async fn fresh(pool: &PgPool, test: &str) -> String {
    let id = format!("mmrs.kvstore.{test}");
    sqlx::query("DELETE FROM pluginkeyvaluestore WHERE pluginid = $1 OR pluginid = $2")
        .bind(&id)
        .bind(format!("{id}.neighbour"))
        .execute(pool)
        .await
        .expect("purges the plugin's rows");
    id
}

/// One row, written around the store, so the fixture can hold what the store would never write.
async fn plant(pool: &PgPool, plugin: &str, key: &str, value: &[u8], expire_at: Option<i64>) {
    sqlx::query(
        "INSERT INTO pluginkeyvaluestore (pluginid, pkey, pvalue, expireat) VALUES ($1, $2, $3, $4)",
    )
    .bind(plugin)
    .bind(key)
    .bind(value)
    .bind(expire_at)
    .execute(pool)
    .await
    .expect("plants a row");
}

/// The rows of `plugin`, by key, as `(key, value, expireat)`.
async fn rows(pool: &PgPool, plugin: &str) -> Vec<(String, Option<Vec<u8>>, Option<i64>)> {
    sqlx::query_as(
        "SELECT pkey, pvalue, expireat FROM pluginkeyvaluestore WHERE pluginid = $1 ORDER BY pkey",
    )
    .bind(plugin)
    .fetch_all(pool)
    .await
    .expect("reads the rows")
}

fn kv(plugin: &str, key: &str, value: &[u8], expire_at: i64) -> PluginKeyValue {
    PluginKeyValue {
        plugin_id: plugin.to_owned(),
        key: key.to_owned(),
        value: Some(value.to_vec()),
        expire_at,
    }
}

async fn value_of(store: &SqlPluginStore, plugin: &str, key: &str) -> Option<Vec<u8>> {
    match store.get(plugin, key).await {
        Ok(kv) => kv.value,
        Err(StoreError::NotFound { .. }) => None,
        Err(other) => panic!("get {key}: {other:?}"),
    }
}

#[tokio::test]
async fn plugin_store_get_sees_only_live_rows() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "get").await;
    let now = get_millis();
    plant(&pool, &p, "forever", b"f", Some(0)).await;
    plant(&pool, &p, "future", b"u", Some(now + HOUR)).await;
    plant(&pool, &p, "past", b"p", Some(now - HOUR)).await;
    plant(&pool, &p, "null", b"n", None).await;

    let got = store.get(&p, "future").await.unwrap();
    assert_eq!(
        got,
        kv(&p, "future", b"u", now + HOUR),
        "every column comes back"
    );
    assert_eq!(
        value_of(&store, &p, "forever").await.as_deref(),
        Some(&b"f"[..])
    );
    assert_eq!(value_of(&store, &p, "past").await, None, "expired");
    assert_eq!(
        value_of(&store, &p, "null").await,
        None,
        "a NULL expiry matches neither arm"
    );
    match store.get(&p, "absent").await {
        Err(StoreError::NotFound { entity, criteria }) => {
            assert_eq!(entity, "PluginKeyValue");
            assert_eq!(criteria, format!("pluginId={p}, key=absent"));
        }
        other => panic!("expected not found, got {other:?}"),
    }
    assert_eq!(
        value_of(&store, &format!("{p}.neighbour"), "forever").await,
        None,
        "scoped by plugin"
    );
}

#[tokio::test]
async fn plugin_store_save_or_update_upserts_and_a_nil_value_deletes() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "upsert").await;
    store.save_or_update(&kv(&p, "k", b"one", 0)).await.unwrap();
    store
        .save_or_update(&kv(&p, "k", b"two", 42))
        .await
        .unwrap();
    assert_eq!(
        rows(&pool, &p).await,
        vec![("k".to_owned(), Some(b"two".to_vec()), Some(42))],
        "the conflict updates both the value and the expiry"
    );
    let mut nil = kv(&p, "k", b"", 0);
    nil.value = None;
    store.save_or_update(&nil).await.unwrap();
    assert!(rows(&pool, &p).await.is_empty());
}

/// A key is at most 150 **characters**: 150 two-byte characters are 300 bytes and valid.
#[tokio::test]
async fn plugin_store_counts_a_key_in_characters() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "runes").await;
    let wide = "é".repeat(150);
    store.save_or_update(&kv(&p, &wide, b"v", 0)).await.unwrap();
    assert_eq!(
        value_of(&store, &p, &wide).await.as_deref(),
        Some(&b"v"[..])
    );
    let err = store
        .save_or_update(&kv(&p, &"é".repeat(151), b"v", 0))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid { .. }), "{err:?}");
}

#[tokio::test]
async fn plugin_store_compare_and_set_inserts_only_over_nothing_live() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "cas_insert").await;
    let now = get_millis();
    plant(&pool, &p, "live", b"l", Some(now + HOUR)).await;
    plant(&pool, &p, "dead", b"d", Some(now - HOUR)).await;
    plant(&pool, &p, "null", b"n", None).await;

    assert!(
        store
            .compare_and_set(&kv(&p, "new", b"1", 0), None)
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_and_set(&kv(&p, "live", b"2", 0), None)
            .await
            .unwrap(),
        "a live value blocks the insert"
    );
    assert!(
        store
            .compare_and_set(&kv(&p, "dead", b"3", 7), None)
            .await
            .unwrap(),
        "an expired value is cleared first"
    );
    assert!(
        !store
            .compare_and_set(&kv(&p, "null", b"4", 0), None)
            .await
            .unwrap(),
        "a NULL expiry is not cleared, so the insert conflicts"
    );
    assert_eq!(
        rows(&pool, &p).await,
        vec![
            ("dead".to_owned(), Some(b"3".to_vec()), Some(7)),
            ("live".to_owned(), Some(b"l".to_vec()), Some(now + HOUR)),
            ("new".to_owned(), Some(b"1".to_vec()), Some(0)),
            ("null".to_owned(), Some(b"n".to_vec()), None),
        ]
    );
}

#[tokio::test]
async fn plugin_store_compare_and_set_updates_only_a_live_match() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "cas_update").await;
    let now = get_millis();
    plant(&pool, &p, "forever", b"old", Some(0)).await;
    plant(&pool, &p, "future", b"old", Some(now + HOUR)).await;
    plant(&pool, &p, "past", b"old", Some(now - HOUR)).await;

    assert!(
        !store
            .compare_and_set(&kv(&p, "forever", b"x", 0), Some(b"other"))
            .await
            .unwrap(),
        "a mismatch writes nothing"
    );
    assert!(
        store
            .compare_and_set(&kv(&p, "forever", b"new", 5), Some(b"old"))
            .await
            .unwrap()
    );
    assert!(
        store
            .compare_and_set(&kv(&p, "future", b"new", 0), Some(b"old"))
            .await
            .unwrap(),
        "an unexpired match"
    );
    assert!(
        !store
            .compare_and_set(&kv(&p, "past", b"new", 0), Some(b"old"))
            .await
            .unwrap(),
        "an expired match is no match"
    );
    assert!(
        !store
            .compare_and_set(&kv(&p, "absent", b"new", 0), Some(b"old"))
            .await
            .unwrap(),
        "an update inserts nothing"
    );
    assert_eq!(
        rows(&pool, &p).await,
        vec![
            ("forever".to_owned(), Some(b"new".to_vec()), Some(5)),
            ("future".to_owned(), Some(b"new".to_vec()), Some(0)),
            ("past".to_owned(), Some(b"old".to_vec()), Some(now - HOUR)),
        ]
    );
}

#[tokio::test]
async fn plugin_store_compare_and_delete_removes_only_a_live_match() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "cad").await;
    let now = get_millis();
    plant(&pool, &p, "future", b"v", Some(now + HOUR)).await;
    plant(&pool, &p, "past", b"v", Some(now - HOUR)).await;
    plant(&pool, &p, "kept", b"v", Some(0)).await;

    assert!(
        !store
            .compare_and_delete(&kv(&p, "kept", b"", 0), Some(b"w"))
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_and_delete(&kv(&p, "kept", b"", 0), None)
            .await
            .unwrap()
    );
    assert!(
        store
            .compare_and_delete(&kv(&p, "future", b"", 0), Some(b"v"))
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_and_delete(&kv(&p, "past", b"", 0), Some(b"v"))
            .await
            .unwrap()
    );
    // Compare-and-set with no new value is this compare-and-delete.
    let mut nil = kv(&p, "kept", b"", 0);
    nil.value = None;
    assert!(store.compare_and_set(&nil, Some(b"v")).await.unwrap());
    assert_eq!(
        rows(&pool, &p).await,
        vec![("past".to_owned(), Some(b"v".to_vec()), Some(now - HOUR))]
    );
}

#[tokio::test]
async fn plugin_store_set_with_options_stamps_the_expiry_in_milliseconds() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "options").await;
    let before = get_millis();
    let opt = |atomic: bool, expire_in_seconds: i64| PluginKVSetOptions {
        atomic,
        old_value: None,
        expire_in_seconds,
    };
    assert!(
        store
            .set_with_options(&p, "plain", Some(b"a"), &opt(false, 0))
            .await
            .unwrap()
    );
    assert!(
        store
            .set_with_options(&p, "ttl", Some(b"b"), &opt(false, 60))
            .await
            .unwrap()
    );
    assert!(
        store
            .set_with_options(&p, "atomic", Some(b"c"), &opt(true, -60))
            .await
            .unwrap()
    );
    assert!(
        !store
            .set_with_options(&p, "plain", Some(b"z"), &opt(true, 0))
            .await
            .unwrap(),
        "atomic with no old value is insert-if-absent"
    );
    assert!(
        store
            .set_with_options(&p, "plain", None, &opt(false, 0))
            .await
            .unwrap(),
        "a nil value deletes, and still answers true"
    );
    let after = get_millis();
    let rows = rows(&pool, &p).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (key, _, atomic_expiry) = &rows[0];
    assert_eq!(key, "atomic");
    let atomic_expiry = atomic_expiry.unwrap();
    assert!(
        (before - 60_000..=after - 60_000).contains(&atomic_expiry),
        "a negative expiry is in the past: {atomic_expiry}"
    );
    let (key, _, ttl_expiry) = &rows[1];
    assert_eq!(key, "ttl");
    let ttl_expiry = ttl_expiry.unwrap();
    assert!(
        (before + 60_000..=after + 60_000).contains(&ttl_expiry),
        "{ttl_expiry}"
    );
}

#[tokio::test]
async fn plugin_store_lists_live_keys_by_key_a_page_at_a_time() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "list").await;
    let now = get_millis();
    // Planted out of order, so insertion order and key order disagree.
    for n in [7, 3, 11, 0, 9, 1, 5, 10, 2, 8, 4, 6] {
        plant(&pool, &p, &format!("k{n:02}"), b"v", Some(0)).await;
    }
    plant(&pool, &p, "k03a-future", b"v", Some(now + HOUR)).await;
    plant(&pool, &p, "k03b-past", b"v", Some(now - HOUR)).await;
    plant(&pool, &p, "k03c-null", b"v", None).await;
    plant(&pool, &format!("{p}.neighbour"), "k00", b"v", Some(0)).await;

    let keys = |from: &[&str]| from.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        store.list(&p, 0, 3).await.unwrap(),
        keys(&["k00", "k01", "k02"])
    );
    assert_eq!(
        store.list(&p, 3, 3).await.unwrap(),
        keys(&["k03", "k03a-future", "k04"]),
        "the live expiring key is listed, the dead and the NULL ones are not"
    );
    assert_eq!(
        store.list(&p, -5, 2).await.unwrap(),
        keys(&["k00", "k01"]),
        "a negative offset is zero"
    );
    assert_eq!(
        store.list(&p, 0, 0).await.unwrap().len(),
        10,
        "no limit is the default of ten"
    );
    assert_eq!(store.list(&p, 2, -1).await.unwrap().len(), 10);
    assert_eq!(store.list(&p, 12, 10).await.unwrap(), keys(&["k11"]));
    assert!(store.list(&p, 13, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn plugin_store_delete_all_is_scoped_to_the_plugin_and_takes_expired_rows() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlPluginStore::new(pool.clone());
    let p = fresh(&pool, "delete_all").await;
    let neighbour = format!("{p}.neighbour");
    let now = get_millis();
    plant(&pool, &p, "a", b"v", Some(0)).await;
    plant(&pool, &p, "dead", b"v", Some(now - HOUR)).await;
    plant(&pool, &p, "null", b"v", None).await;
    plant(&pool, &neighbour, "a", b"v", Some(0)).await;

    store.delete(&p, "absent").await.unwrap();
    store.delete(&p, "a").await.unwrap();
    assert_eq!(rows(&pool, &p).await.len(), 2);
    assert_eq!(
        rows(&pool, &neighbour).await.len(),
        1,
        "delete is scoped too"
    );
    store.delete_all_for_plugin(&p).await.unwrap();
    assert!(rows(&pool, &p).await.is_empty());
    assert_eq!(rows(&pool, &neighbour).await.len(), 1);
}
