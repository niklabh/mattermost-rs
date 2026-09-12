//! The five writes behind `POST /api/v4/emoji`, `DELETE /api/v4/emoji/{emoji_id}`,
//! `POST /api/v4/terms_of_service` and `POST /api/v4/users/{user_id}/terms_of_service`, against a
//! real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_emoji_and_terms_writes
//! ```
//!
//! What only this file can reach: the **soft** delete's second attempt (a 404 the REST route
//! cannot produce twice without racing), the `(Name, DeleteAt)` unique constraint, the
//! `UPDATE`-then-`INSERT` upsert in `UserTermsOfService::save`, and the reaction sweep a deleted
//! emoji triggers — which the API answers `{"status":"OK"}` to whether it worked or not.

use mm_model::emoji::Emoji;
use mm_model::terms_of_service::TermsOfService;
use mm_model::user_terms_of_service::UserTermsOfService;
use mm_store::{
    EmojiStore, ReactionStore, SqlEmojiStore, SqlReactionStore, SqlTermsOfServiceStore,
    SqlUserTermsOfServiceStore, StoreError, TermsOfServiceStore, UserTermsOfServiceStore,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// One lock for the whole file: every test purges the same prefixes.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrsewuser0000000000000001";
const OTHER_USER: &str = "mmrsewuser0000000000000002";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        // CLAUDE.md: a test that waits is a bug. sqlx's default is 30 seconds.
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM emoji WHERE name LIKE 'mmrsew%'",
        "DELETE FROM reactions WHERE emojiname LIKE 'mmrsew%'",
        "DELETE FROM posts WHERE id LIKE 'mmrsewpost%'",
        "DELETE FROM termsofservice WHERE userid LIKE 'mmrsewuser%'",
        "DELETE FROM usertermsofservice WHERE userid LIKE 'mmrsewuser%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// A distinct name per test, so a leftover row from a crashed run cannot make a later one pass.
fn unique_name(tag: &str) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("mmrsew{tag}{stamp}")
}

// ---------------------------------------------------------------------------------------------
// Emoji
// ---------------------------------------------------------------------------------------------

/// `Save` runs `PreSave` **and** `IsValid` itself, so the row that lands is not the row that was
/// handed in: the id is minted, the name is lower-cased and both timestamps are the store's.
#[tokio::test]
async fn saving_an_emoji_mints_the_id_and_lowercases_the_name() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlEmojiStore::new(pool.clone());

    let name = unique_name("MiXeD");
    let saved = store
        .save(Emoji {
            // A caller-supplied id survives the store — it is `App::create_emoji` that blanks it.
            id: String::new(),
            create_at: 1,
            update_at: 2,
            delete_at: 0,
            creator_id: USER.to_owned(),
            name: name.clone(),
        })
        .await
        .expect("the save succeeds");

    assert_eq!(saved.id.len(), 26, "PreSave minted an id");
    assert_eq!(saved.name, name.to_lowercase(), "PreSave lower-cased");
    assert_ne!(saved.create_at, 1, "PreSave overwrites create_at");
    assert_eq!(
        saved.create_at, saved.update_at,
        "PreSave copies create_at onto update_at"
    );
    assert_eq!(saved.delete_at, 0);

    let read = store.get(&saved.id).await.expect("the row is there");
    assert_eq!(read, saved, "what came back is what was written");
    assert_eq!(
        store.get_by_name(&saved.name).await.expect("by name"),
        saved
    );
}

/// `IsValid` inside the store, and its error carries the model's own id and 400 all the way out.
#[tokio::test]
async fn an_invalid_emoji_never_reaches_the_table() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlEmojiStore::new(pool.clone());

    // A system emoji name: rejected with a *different* id from a malformed one.
    let err = store
        .save(Emoji {
            creator_id: USER.to_owned(),
            name: "grinning".to_owned(),
            ..Emoji::default()
        })
        .await
        .expect_err("a system name is refused");
    let StoreError::Invalid { app_error, .. } = err else {
        panic!("expected a validation failure");
    };
    assert_eq!(app_error.id, "model.emoji.system_emoji_name.app_error");
    assert_eq!(app_error.status_code, 400);

    let err = store
        .save(Emoji {
            creator_id: USER.to_owned(),
            name: "has space".to_owned(),
            ..Emoji::default()
        })
        .await
        .expect_err("a malformed name is refused");
    let StoreError::Invalid { app_error, .. } = err else {
        panic!("expected a validation failure");
    };
    assert_eq!(app_error.id, "model.emoji.name.app_error");

    let leftovers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM emoji WHERE name IN ('grinning', 'has space')")
            .fetch_one(&pool)
            .await
            .expect("counts");
    assert_eq!(leftovers, 0, "neither refusal wrote a row");
}

/// `(Name, DeleteAt)` is unique, so a second **live** emoji cannot take a name — and a delete
/// frees it again, which is the whole reason the constraint is composite.
#[tokio::test]
async fn a_live_name_is_unique_and_a_deleted_one_is_not() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlEmojiStore::new(pool.clone());

    let name = unique_name("dup");
    let first = store
        .save(Emoji {
            creator_id: USER.to_owned(),
            name: name.clone(),
            ..Emoji::default()
        })
        .await
        .expect("the first save succeeds");

    let err = store
        .save(Emoji {
            creator_id: USER.to_owned(),
            name: name.clone(),
            ..Emoji::default()
        })
        .await
        .expect_err("the second live row is refused by the unique constraint");
    assert!(
        matches!(err, StoreError::Db { .. }),
        "a constraint violation is a driver error, which the app layer folds into one 500"
    );

    store
        .delete(&first.id, 1_700_000_000_000)
        .await
        .expect("the delete succeeds");
    store
        .save(Emoji {
            creator_id: USER.to_owned(),
            name,
            ..Emoji::default()
        })
        .await
        .expect("the name is free once the first row is deleted");
}

/// The delete is **soft**, both timestamps take the same value, and a second attempt is
/// `ErrNotFound` — which the app layer answers 404 to with `app.emoji.delete.no_results`.
#[tokio::test]
async fn deleting_is_soft_and_deleting_twice_is_a_miss() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlEmojiStore::new(pool.clone());

    let saved = store
        .save(Emoji {
            creator_id: USER.to_owned(),
            name: unique_name("del"),
            ..Emoji::default()
        })
        .await
        .expect("the save succeeds");

    const WHEN: i64 = 1_700_000_000_123;
    store
        .delete(&saved.id, WHEN)
        .await
        .expect("the first delete");

    let row: (i64, i64) = sqlx::query_as("SELECT deleteat, updateat FROM emoji WHERE id = $1")
        .bind(&saved.id)
        .fetch_one(&pool)
        .await
        .expect("the row is still there");
    assert_eq!(
        row,
        (WHEN, WHEN),
        "both columns take the delete's timestamp"
    );

    // Every read filters `DeleteAt = 0`, so the row is gone from all three.
    assert!(store.get(&saved.id).await.is_err());
    assert!(store.get_by_name(&saved.name).await.is_err());
    assert!(
        store
            .get_multiple_by_name(std::slice::from_ref(&saved.name))
            .await
            .expect("the query runs")
            .is_empty()
    );

    let err = store
        .delete(&saved.id, WHEN + 1)
        .await
        .expect_err("the second delete matches nothing");
    assert!(err.is_not_found(), "zero rows affected is ErrNotFound");

    let err = store
        .delete("mmrsewnosuchemoji000000001", WHEN)
        .await
        .expect_err("an id that never existed");
    assert!(err.is_not_found());
}

/// `DeleteAllWithEmojiName` soft-deletes the reactions that used the emoji, leaves every other
/// reaction alone, and recomputes `Posts.HasReactions`.
#[tokio::test]
async fn deleting_an_emoji_sweeps_its_reactions_and_repairs_the_posts() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlReactionStore::new(pool.clone());

    let doomed = unique_name("gone");
    let kept = unique_name("stay");
    // Two posts: the first loses its only reaction, the second keeps one of two.
    for post in ["mmrsewpost00000000000000a1", "mmrsewpost00000000000000a2"] {
        sqlx::query("INSERT INTO posts (id, hasreactions, updateat) VALUES ($1, true, 1)")
            .bind(post)
            .execute(&pool)
            .await
            .expect("inserts the post");
    }
    for (post, user, emoji, deleted) in [
        ("mmrsewpost00000000000000a1", USER, &doomed, 0_i64),
        ("mmrsewpost00000000000000a2", USER, &doomed, 0),
        ("mmrsewpost00000000000000a2", OTHER_USER, &kept, 0),
        // Already deleted: the sweep must not move its timestamps again.
        ("mmrsewpost00000000000000a1", OTHER_USER, &doomed, 42),
    ] {
        sqlx::query(
            "INSERT INTO reactions (userid, postid, emojiname, createat, updateat, deleteat, channelid)
             VALUES ($1, $2, $3, 1, 1, $4, 'mmrsewchan00000000000000c1')",
        )
        .bind(user)
        .bind(post)
        .bind(emoji)
        .bind(deleted)
        .execute(&pool)
        .await
        .expect("inserts the reaction");
    }

    store
        .delete_all_with_emoji_name(&doomed)
        .await
        .expect("the sweep succeeds");

    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reactions WHERE emojiname = $1 AND COALESCE(deleteat, 0) = 0",
    )
    .bind(&doomed)
    .fetch_one(&pool)
    .await
    .expect("counts");
    assert_eq!(live, 0, "every live reaction with that name is gone");

    let untouched: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reactions WHERE emojiname = $1 AND COALESCE(deleteat, 0) = 0",
    )
    .bind(&kept)
    .fetch_one(&pool)
    .await
    .expect("counts");
    assert_eq!(untouched, 1, "a reaction with another name is left alone");

    // The already-deleted row keeps its original timestamp: the predicate excludes it.
    let already: i64 = sqlx::query_scalar(
        "SELECT deleteat FROM reactions WHERE emojiname = $1 AND userid = $2 AND postid = $3",
    )
    .bind(&doomed)
    .bind(OTHER_USER)
    .bind("mmrsewpost00000000000000a1")
    .fetch_one(&pool)
    .await
    .expect("reads it back");
    assert_eq!(already, 42, "an already-deleted reaction is not re-stamped");

    let first: bool = sqlx::query_scalar(
        "SELECT hasreactions FROM posts WHERE id = 'mmrsewpost00000000000000a1'",
    )
    .fetch_one(&pool)
    .await
    .expect("reads the post");
    assert!(!first, "the post lost its only live reaction");

    let second: bool = sqlx::query_scalar(
        "SELECT hasreactions FROM posts WHERE id = 'mmrsewpost00000000000000a2'",
    )
    .fetch_one(&pool)
    .await
    .expect("reads the post");
    assert!(
        second,
        "the post still has the reaction with the other name"
    );

    // An emoji nobody reacted with is not an error.
    store
        .delete_all_with_emoji_name(&unique_name("never"))
        .await
        .expect("a sweep that matches nothing succeeds");
}

// ---------------------------------------------------------------------------------------------
// TermsOfService
// ---------------------------------------------------------------------------------------------

/// `Save` refuses a caller-supplied id **before** `PreSave` would have minted one — which is the
/// only ordering under which that check is reachable at all.
#[tokio::test]
async fn publishing_terms_mints_an_id_and_refuses_a_supplied_one() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlTermsOfServiceStore::new(pool.clone());

    let err = store
        .save(TermsOfService {
            id: "mmrsewtos00000000000000001".to_owned(),
            create_at: 0,
            user_id: USER.to_owned(),
            text: "supplied id".to_owned(),
        })
        .await
        .expect_err("a supplied id is refused");
    assert!(
        matches!(err, StoreError::InvalidInput { field: "Id", .. }),
        "the app layer turns this into a 400, not a 500"
    );

    let saved = store
        .save(TermsOfService {
            id: String::new(),
            create_at: 0,
            user_id: USER.to_owned(),
            text: "the first revision".to_owned(),
        })
        .await
        .expect("the save succeeds");
    assert_eq!(saved.id.len(), 26);
    assert_ne!(saved.create_at, 0, "PreSave stamped it");

    assert_eq!(
        store.get(&saved.id).await.expect("reads back"),
        saved,
        "Get(id) returns what Save wrote"
    );
    assert_eq!(
        store.get_latest().await.expect("reads the latest"),
        saved,
        "one revision is the latest revision"
    );

    let missing = store
        .get("mmrsewtos00000000000000999")
        .await
        .expect_err("an unknown id");
    assert!(missing.is_not_found());
}

/// A second revision becomes the latest, and `IsValid` still guards the write.
#[tokio::test]
async fn the_newest_revision_wins_and_an_unowned_one_is_refused() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlTermsOfServiceStore::new(pool.clone());

    let first = store
        .save(TermsOfService {
            user_id: USER.to_owned(),
            text: "first".to_owned(),
            ..TermsOfService::default()
        })
        .await
        .expect("the first save");

    // `ORDER BY CreateAt DESC` with no tiebreak, so the second revision needs a later stamp than
    // the first for "latest" to be defined at all. `PreSave` reads the clock; a millisecond is
    // enough and the two saves are not in the same one in practice — force it rather than hope.
    sqlx::query("UPDATE termsofservice SET createat = createat - 1000 WHERE id = $1")
        .bind(&first.id)
        .execute(&pool)
        .await
        .expect("ages the first revision");

    let second = store
        .save(TermsOfService {
            user_id: USER.to_owned(),
            text: "second".to_owned(),
            ..TermsOfService::default()
        })
        .await
        .expect("the second save");

    assert_eq!(store.get_latest().await.expect("latest").id, second.id);
    assert_eq!(
        store.get(&first.id).await.expect("by id").text,
        "first",
        "the older revision is still readable by id"
    );

    // `IsValid` rejects a user id that is not id-shaped, and the model's 400 survives.
    let err = store
        .save(TermsOfService {
            user_id: "nope".to_owned(),
            text: "x".to_owned(),
            ..TermsOfService::default()
        })
        .await
        .expect_err("an unowned revision");
    let StoreError::Invalid { app_error, .. } = err else {
        panic!("expected a validation failure");
    };
    assert_eq!(
        app_error.id,
        "model.terms_of_service.is_valid.user_id.app_error"
    );
}

// ---------------------------------------------------------------------------------------------
// UserTermsOfService
// ---------------------------------------------------------------------------------------------

/// The upsert is `UPDATE` then `INSERT`-if-nothing-matched, so accepting twice leaves **one** row
/// and moves its `CreateAt`.
#[tokio::test]
async fn accepting_twice_replaces_the_row_rather_than_adding_one() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlUserTermsOfServiceStore::new(pool.clone());

    let first = store
        .save(UserTermsOfService {
            user_id: USER.to_owned(),
            terms_of_service_id: "mmrsewtos00000000000000001".to_owned(),
            create_at: 0,
        })
        .await
        .expect("the first acceptance");
    assert_ne!(first.create_at, 0, "PreSave stamped it");

    sqlx::query("UPDATE usertermsofservice SET createat = createat - 1000 WHERE userid = $1")
        .bind(USER)
        .execute(&pool)
        .await
        .expect("ages the row so the rewrite is visible");

    let second = store
        .save(UserTermsOfService {
            user_id: USER.to_owned(),
            terms_of_service_id: "mmrsewtos00000000000000002".to_owned(),
            create_at: 0,
        })
        .await
        .expect("the second acceptance");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usertermsofservice WHERE userid = $1")
        .bind(USER)
        .fetch_one(&pool)
        .await
        .expect("counts");
    assert_eq!(rows, 1, "the UPDATE matched, so no second row was inserted");

    let read = store.get_by_user(USER).await.expect("reads back");
    assert_eq!(read.terms_of_service_id, "mmrsewtos00000000000000002");
    assert_eq!(read.create_at, second.create_at);
    assert!(read.create_at > first.create_at - 1000, "the stamp moved");

    // An acceptance that fails `IsValid` never reaches either statement.
    let err = store
        .save(UserTermsOfService {
            user_id: USER.to_owned(),
            terms_of_service_id: String::new(),
            create_at: 0,
        })
        .await
        .expect_err("an empty terms id");
    let StoreError::Invalid { app_error, .. } = err else {
        panic!("expected a validation failure");
    };
    assert_eq!(
        app_error.id,
        "model.user_terms_of_service.is_valid.terms_of_service_id.app_error"
    );
    assert_eq!(
        app_error.detailed_error,
        format!("user_terms_of_service_user_id={USER}"),
        "the detail is the user id, on the branch about the terms id"
    );
}

/// `Delete` matches **both** columns and never checks how many rows it removed, so rejecting a
/// revision the user did not accept is a silent success that leaves their acceptance standing.
#[tokio::test]
async fn rejecting_matches_the_pair_and_a_miss_is_not_an_error() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlUserTermsOfServiceStore::new(pool.clone());

    store
        .save(UserTermsOfService {
            user_id: USER.to_owned(),
            terms_of_service_id: "mmrsewtos00000000000000001".to_owned(),
            create_at: 0,
        })
        .await
        .expect("the acceptance");

    // The wrong revision: the predicate is on both columns, so nothing is removed.
    store
        .delete(USER, "mmrsewtos00000000000000002")
        .await
        .expect("a delete that matches nothing still succeeds");
    assert!(
        store.get_by_user(USER).await.is_ok(),
        "the acceptance of the other revision survives"
    );

    // A user who accepted nothing at all.
    store
        .delete(OTHER_USER, "mmrsewtos00000000000000001")
        .await
        .expect("a delete for a user with no row succeeds");

    store
        .delete(USER, "mmrsewtos00000000000000001")
        .await
        .expect("the matching delete");
    assert!(
        store
            .get_by_user(USER)
            .await
            .expect_err("gone")
            .is_not_found(),
        "the row is really gone"
    );
}
