//! `AccessControlPolicyStore::delete` against a real Postgres.
//!
//! ```sh
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_access_control_policy
//! ```
//!
//! Nothing over HTTP can plant a policy row on a server below Enterprise Advanced, so every
//! archive the parity suites drive through `cleanupTeamAccessControlPolicy` is the no-op arm. The
//! copy-then-delete arm — the one that would lose a policy if it were wrong — is held down here on
//! a planted row.

use mm_store::{AccessControlPolicyStore, SqlAccessControlPolicyStore};
use sqlx::postgres::{PgPool, PgPoolOptions};

fn enabled() -> bool {
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

const POLICY_ID: &str = "mmrsacpolicydeletetest0001";

async fn clear(pool: &PgPool) {
    for table in ["accesscontrolpolicies", "accesscontrolpolicyhistory"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE id = $1"))
            .bind(POLICY_ID)
            .execute(pool)
            .await
            .expect("clears the planted policy");
    }
}

async fn plant(pool: &PgPool, revision: i32) {
    sqlx::query(
        "INSERT INTO accesscontrolpolicies (id, name, type, active, createat, revision, version, data, props)
         VALUES ($1, 'mmrs test policy', 'team', true, 1767225600000, $2, 'v0.2', '{\"rules\":[]}'::jsonb, '{\"k\":\"v\"}'::jsonb)",
    )
    .bind(POLICY_ID)
    .bind(revision)
    .execute(pool)
    .await
    .expect("plants a policy row");
}

#[tokio::test]
async fn delete_moves_the_row_to_history_and_is_a_no_op_without_one() {
    if !enabled() {
        return;
    }
    let pool = pool().await;
    clear(&pool).await;
    let store = SqlAccessControlPolicyStore::new(pool.clone());

    // No row: nothing happens, and nothing is written to history.
    store
        .delete(POLICY_ID)
        .await
        .expect("a missing policy is not an error");
    let history: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM accesscontrolpolicyhistory WHERE id = $1")
            .bind(POLICY_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(history, 0);

    // A row: copied to history with its revision, then gone.
    plant(&pool, 3).await;
    store
        .delete(POLICY_ID)
        .await
        .expect("the planted policy deletes");
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accesscontrolpolicies WHERE id = $1")
        .bind(POLICY_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 0, "the policy row is deleted");
    let archived: (String, i32, String, serde_json::Value) = sqlx::query_as(
        "SELECT name, revision, version, props FROM accesscontrolpolicyhistory WHERE id = $1",
    )
    .bind(POLICY_ID)
    .fetch_one(&pool)
    .await
    .expect("one history row");
    assert_eq!(archived.0, "mmrs test policy");
    assert_eq!(archived.1, 3, "the revision travels with the row");
    assert_eq!(archived.2, "v0.2");
    assert_eq!(
        archived.3,
        serde_json::json!({"k": "v"}),
        "props are copied as jsonb"
    );

    // The same revision again: the history primary key refuses it, the transaction rolls back,
    // and the live row **survives** — Go's caller logs a warning and moves on.
    plant(&pool, 3).await;
    let err = store
        .delete(POLICY_ID)
        .await
        .expect_err("a second copy of (id, revision) violates the history key");
    assert!(err.to_string().contains("history"), "{err}");
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accesscontrolpolicies WHERE id = $1")
        .bind(POLICY_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 1, "rolled back: the live row is still there");

    clear(&pool).await;
}
