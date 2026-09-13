//! `App::license` against a real Postgres and the stack's real signed licence.
//!
//! ```sh
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_license_loading
//! ```
//!
//! # What only this file can see
//!
//! Every branch of `LoadLicense` past `MM_LICENSE` is a database read: `Systems.ActiveLicenseId`,
//! then the `Licenses` row it names, then the signature on that row. The unit tests cover the
//! validator with keys minted in-process; the parity suite covers the licensed pair over HTTP with
//! `MM_LICENSE`. Neither reaches the stored-row path — the parity stack must never plant a real
//! licence row, because the stack's Go server would not see it and every unlicensed suite would
//! then compare a licensed mm-api against an unlicensed Go. So the row path is exercised here,
//! against the real key pair `scripts/go-licensed.sh` generated, and cleaned up before the file
//! ends.
//!
//! The second licence is signed **in the test** with the stack's private key: switching the
//! active id from one licence to another is the only way to see that the signature cache is keyed
//! by id and not "whatever verified last".
//!
//! Every test is named `license_loading_*` so `MUTATE_FILTER` can select them by name. They share
//! one row, so they take one lock.

use base64::Engine;
use mm_app::App;
use mm_app::config::Config;
use mm_store::SqlStore;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use sha2::Digest;
use sqlx::postgres::{PgPool, PgPoolOptions};

fn enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

static ROW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn stack_license_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../reference/.build/license")
}

fn read(name: &str) -> String {
    let path = stack_license_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "no {} — run `scripts/go-licensed.sh start` first ({e})",
            path.display()
        )
    })
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

/// An `App` with no `MM_LICENSE`, verifying against the stack's key — so everything it learns
/// about the licence, it learns from the database.
fn app(pool: &PgPool) -> App {
    App::with_config(
        SqlStore::from_pool(pool.clone()),
        Config {
            license: String::new(),
            license_public_key: Some(read("public.pem")),
            ..Config::default()
        },
    )
}

/// Mattermost's format, signed with the stack's private key — what `openssl dgst -sha512 -sign`
/// produces in `scripts/go-licensed.sh`.
fn sign_with_stack_key(plaintext: &str) -> String {
    let pem = read("private.pem");
    let private = rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(&pem))
        .expect("the stack's private key parses as PKCS#8 or PKCS#1");
    let digest = sha2::Sha512::digest(plaintext.as_bytes());
    let signature = private
        .sign(rsa::Pkcs1v15Sign::new::<sha2::Sha512>(), &digest)
        .expect("signs");
    let mut signed = plaintext.as_bytes().to_vec();
    signed.extend_from_slice(&signature);
    base64::engine::general_purpose::STANDARD.encode(signed)
}

const ORACLE_ID: &str = "mmrslicensedoracle00000001";
const SECOND_ID: &str = "mmrslicensedsecond00000001";
const GARBAGE_ID: &str = "mmrslicensedgarbage0000001";
const FOREIGN_ID: &str = "mmrslicensedforeign0000001";
const NO_ROW_ID: &str = "mmrslicensednorow000000001";

async fn clear(pool: &PgPool) {
    sqlx::query("DELETE FROM systems WHERE name = 'ActiveLicenseId'")
        .execute(pool)
        .await
        .expect("clears the active id");
    sqlx::query("DELETE FROM licenses WHERE id LIKE 'mmrslicensed%'")
        .execute(pool)
        .await
        .expect("clears the planted rows");
}

async fn plant_row(pool: &PgPool, id: &str, bytes: &str) {
    sqlx::query("INSERT INTO licenses (id, createat, bytes) VALUES ($1, 1767225600000, $2) ON CONFLICT (id) DO UPDATE SET bytes = EXCLUDED.bytes")
        .bind(id)
        .bind(bytes)
        .execute(pool)
        .await
        .expect("plants a licence row");
}

async fn set_active(pool: &PgPool, id: &str) {
    sqlx::query("DELETE FROM systems WHERE name = 'ActiveLicenseId'")
        .execute(pool)
        .await
        .expect("clears");
    sqlx::query("INSERT INTO systems (name, value) VALUES ('ActiveLicenseId', $1)")
        .bind(id)
        .execute(pool)
        .await
        .expect("sets the active id");
}

/// The whole ladder of `LoadLicense`'s database branches, in one test because they share one
/// row and one order: no id, a verifying row, a *different* verifying row, an id naming no row, a
/// blank id, a row that is not a licence, a row signed by somebody else.
#[tokio::test]
async fn license_loading_follows_the_active_id_through_every_branch() {
    if !enabled() {
        return;
    }
    let _row = ROW.lock().await;
    let pool = pool().await;
    clear(&pool).await;
    let app = app(&pool);

    assert!(
        app.license().await.unwrap().is_none(),
        "no ActiveLicenseId: unlicensed"
    );

    // The stack's own licence, stored the way `SaveLicense` stores it.
    plant_row(&pool, ORACLE_ID, read("license.signed").trim()).await;
    set_active(&pool, ORACLE_ID).await;
    let license = app
        .license()
        .await
        .unwrap()
        .expect("the stored licence verifies");
    assert_eq!(license.id, ORACLE_ID);
    assert_eq!(license.sku_short_name, "enterprise");
    assert_eq!(
        license.features.as_ref().and_then(|f| f.ldap),
        Some(true),
        "SetDefaults ran on the loaded licence"
    );

    // A second licence, Professional, signed here with the stack's key. Switching the active id
    // must switch the answer: the signature cache is keyed by id.
    let second = format!(
        r#"{{"id":"{SECOND_ID}","issued_at":1767225600000,"starts_at":1767225600000,"expires_at":4102444800000,"customer":{{"id":"c","name":"Second","email":"second@mmrs.invalid","company":"co"}},"features":{{"users":5}},"sku_name":"Professional","sku_short_name":"professional"}}"#
    );
    plant_row(&pool, SECOND_ID, &sign_with_stack_key(&second)).await;
    set_active(&pool, SECOND_ID).await;
    let license = app
        .license()
        .await
        .unwrap()
        .expect("the second licence verifies");
    assert_eq!(license.id, SECOND_ID);
    assert_eq!(license.sku_short_name, "professional");
    assert!(
        !mm_model::license::minimum_enterprise_license(Some(&license)),
        "and it is below the enterprise rung"
    );

    // Back to the first: the cache is replaced, not appended to, and the answer follows the row.
    set_active(&pool, ORACLE_ID).await;
    assert_eq!(
        app.license()
            .await
            .unwrap()
            .map(|l| l.sku_short_name.clone()),
        Some("enterprise".to_owned())
    );

    // An id that names no row: Go logs "License key from https://mattermost.com required" and
    // stays unlicensed.
    set_active(&pool, NO_ROW_ID).await;
    assert!(app.license().await.unwrap().is_none(), "no row: unlicensed");

    // The de-licensed shape `RemoveLicense` leaves: a blank value, not a deleted row.
    set_active(&pool, "").await;
    assert!(
        app.license().await.unwrap().is_none(),
        "blank id: unlicensed"
    );

    // A row that is not a licence at all: "License key is invalid."
    plant_row(&pool, GARBAGE_ID, "bm90IGEgbGljZW5jZQ==").await;
    set_active(&pool, GARBAGE_ID).await;
    assert!(
        app.license().await.unwrap().is_none(),
        "garbage row: unlicensed"
    );

    // A licence signed by a key that is not the stack's: verifies against nothing we trust.
    let foreign_private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let body = format!(
        r#"{{"id":"{FOREIGN_ID}","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{{"id":"c","name":"n","email":"e","company":"co"}},"features":{{"users":5}},"sku_name":"Enterprise","sku_short_name":"enterprise"}}"#
    );
    let digest = sha2::Sha512::digest(body.as_bytes());
    let signature = foreign_private
        .sign(rsa::Pkcs1v15Sign::new::<sha2::Sha512>(), &digest)
        .unwrap();
    let mut signed = body.into_bytes();
    signed.extend_from_slice(&signature);
    plant_row(
        &pool,
        FOREIGN_ID,
        &base64::engine::general_purpose::STANDARD.encode(signed),
    )
    .await;
    set_active(&pool, FOREIGN_ID).await;
    assert!(
        app.license().await.unwrap().is_none(),
        "a foreign signature: unlicensed"
    );

    clear(&pool).await;
    assert!(app.license().await.unwrap().is_none());
}

/// `MM_LICENSE` wins over the database — and an **invalid** `MM_LICENSE` hides it. Go returns
/// from `LoadLicense` the moment the variable is set, whether or not it parsed, so a verifying
/// row behind a broken variable is not a licence.
#[tokio::test]
async fn license_loading_env_licence_precedes_and_can_hide_the_row() {
    if !enabled() {
        return;
    }
    let _row = ROW.lock().await;
    let pool = pool().await;
    clear(&pool).await;
    plant_row(&pool, ORACLE_ID, read("license.signed").trim()).await;
    set_active(&pool, ORACLE_ID).await;

    let key = read("public.pem");
    let second = format!(
        r#"{{"id":"{SECOND_ID}","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{{"id":"c","name":"n","email":"e","company":"co"}},"features":{{"users":5}},"sku_name":"Professional","sku_short_name":"professional"}}"#
    );
    let from_env = App::with_config(
        SqlStore::from_pool(pool.clone()),
        Config {
            license: sign_with_stack_key(&second),
            license_public_key: Some(key.clone()),
            ..Config::default()
        },
    );
    assert_eq!(
        from_env.license().await.unwrap().map(|l| l.id.clone()),
        Some(SECOND_ID.to_owned()),
        "MM_LICENSE is read first, whatever the row says"
    );

    let broken_env = App::with_config(
        SqlStore::from_pool(pool.clone()),
        Config {
            license: "not a licence".to_owned(),
            license_public_key: Some(key),
            ..Config::default()
        },
    );
    assert!(
        broken_env.license().await.unwrap().is_none(),
        "an invalid MM_LICENSE returns early: the verifying row behind it is never read"
    );

    clear(&pool).await;
}
