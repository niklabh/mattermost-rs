//! `App::is_profile_image_locked_for_user`'s **unlicensed** answer, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_profile_image_lock
//! ```
//!
//! # Why this is here and not in the unit tests or the parity suite
//!
//! The function's fourth conjunct is the licence, and `App::license_state` answers `Unlicensed`
//! only by reading `Systems.ActiveLicenseId` and finding nothing there. Every unit test in
//! `mm_app::user::tests` is built on a deliberately unreachable pool, so it either short-circuits
//! at one of the first three conjuncts or dies on the store — `LicenseState::Unlicensed` is never
//! reached by any of them.
//!
//! Over HTTP the branch is invisible for a different reason: `Ok(false)` and the licensed
//! `Unreproducible` forward reach the **same** place. Both callers spell the check
//! `… == Some(true)`, so "not locked" and "cannot tell" both fall through to the hand-over that
//! follows, and a mutation turning the unlicensed answer into a forward changes no status, no
//! body and not even `x-mmrs-served-by`. `lock-unlicensed-forwards-instead-of-false` survived the
//! whole parity suite for exactly that reason, which is a finding about where the branch can be
//! observed at all — here, where the function returns its own value.
//!
//! # The precondition, asserted rather than assumed
//!
//! An installation with an `ActiveLicenseId` planted in `Systems` is *licensed* as far as this
//! process is concerned, and several mm-api parity suites plant one. `cargo test` runs test
//! binaries one at a time, so they cannot overlap with this — but an aborted run can leave the row
//! behind, and then this file would be asserting the licensed branch while claiming the other.
//! So the row is read first and the test says which world it ran in.
//!
//! Every test is named `profile_image_lock_*` so `MUTATE_FILTER` can select them by name.

use mm_app::App;
use mm_app::config::Config;
use mm_model::session::Session;
use mm_model::user::User;
use mm_store::SqlStore;
use sqlx::postgres::PgPoolOptions;

fn enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn app_with(config: Config) -> App {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres");
    App::with_config(SqlStore::from_pool(pool), config)
}

/// Is there an `ActiveLicenseId` row? If there is, this installation reads as licensed and the
/// unlicensed branch is not the one under test.
async fn installation_is_unlicensed() -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return false;
    };
    let planted: Option<String> =
        sqlx::query_scalar("SELECT value FROM systems WHERE name = 'ActiveLicenseId'")
            .fetch_optional(&pool)
            .await
            .expect("the systems table reads")
            .flatten();
    !planted.is_some_and(|id| id.len() == 26)
}

/// The configuration where the first three conjuncts all hold, so the licence is the only one
/// left to decide: an ordinary session with no `edit_other_users`, an email/password account, and
/// `LockProfileFieldsForEmailUsers` at `"all"`.
fn every_conjunct_but_the_licence() -> Config {
    Config {
        lock_profile_fields_for_email_users: "all".to_owned(),
        // Empty, so `license_state` goes to the store rather than short-circuiting on `MM_LICENSE`.
        license: String::new(),
        ..Config::default()
    }
}

/// An unlicensed installation **answers** `false`; it does not forward.
///
/// `MinimumEnterpriseLicense(nil)` is `false` in Go, so the conjunction is false and the picture
/// is not locked. Reproducing that as a hand-over instead would be wrong in principle — the
/// answer is known — and is precisely what the parity suite cannot see.
#[tokio::test]
async fn profile_image_lock_answers_false_without_a_licence() {
    if !enabled() {
        return;
    }
    assert!(
        installation_is_unlicensed().await,
        "an ActiveLicenseId is planted; this test needs an unlicensed installation. \
         An aborted mm-api parity run leaves that row behind — clear it and re-run."
    );

    let app = app_with(every_conjunct_but_the_licence()).await;
    let locked = app
        .is_profile_image_locked_for_user(&Session::default(), &User::default())
        .await
        .expect("an unlicensed server answers rather than forwarding");
    assert!(
        !locked,
        "MinimumEnterpriseLicense(nil) is false, so the conjunction is false"
    );
}

/// The same call with `MM_LICENSE` set **is** the forward, so the test above is not passing
/// because the function forwards everything.
///
/// This half needs no database — `license_state` short-circuits on the configured licence — but it
/// lives here so the pair is read together: the difference between them is the only observable
/// consequence of the licence conjunct anywhere in the tree.
#[tokio::test]
async fn profile_image_lock_forwards_a_licensed_server() {
    if !enabled() {
        return;
    }
    let app = app_with(Config {
        license: "a-signed-licence-blob".to_owned(),
        ..every_conjunct_but_the_licence()
    })
    .await;
    let err = app
        .is_profile_image_locked_for_user(&Session::default(), &User::default())
        .await
        .expect_err("the SKU tier is not visible from here");
    assert!(
        matches!(err, mm_app::post::PrepareError::Unreproducible(_)),
        "a licensed server is handed over, not guessed at: {err:?}"
    );
}

/// And the three conjuncts in front of it still short-circuit on an unlicensed server, so the
/// answer above is the licence branch's and not one of theirs arriving early.
#[tokio::test]
async fn profile_image_lock_short_circuits_before_the_store() {
    if !enabled() {
        return;
    }
    let app = app_with(Config {
        // `"none"`, the stock value: the third conjunct fails and the licence is never consulted.
        lock_profile_fields_for_email_users: "none".to_owned(),
        ..every_conjunct_but_the_licence()
    })
    .await;
    assert!(
        !app.is_profile_image_locked_for_user(&Session::default(), &User::default())
            .await
            .expect("no licence question is reached"),
    );

    // The auth-service conjunct, on the same live store, for the same reason.
    let app = app_with(every_conjunct_but_the_licence()).await;
    let sso = User {
        auth_service: "saml".to_owned(),
        ..User::default()
    };
    assert!(
        !app.is_profile_image_locked_for_user(&Session::default(), &sso)
            .await
            .expect("no licence question is reached"),
    );
}
