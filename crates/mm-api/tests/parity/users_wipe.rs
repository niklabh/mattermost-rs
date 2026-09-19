//! Cross-server parity for `DELETE /api/v4/users` on the local socket —
//! `localPermanentDeleteAllUsers` (api4/user_local.go:351), which erases **every account**.
//!
//! ```sh
//! scripts/wipe-parity.sh        # recreates stack 6, runs this, takes the stack down again
//! ```
//!
//! # Only ever on a stack that is about to be thrown away
//!
//! Run against a shared stack this would erase the fixture administrator every other suite logs
//! in as. So it does nothing unless `MMRS_WIPE_PARITY` names the stack this binary was built for
//! (`MMRS_STACK`), and it refuses a database with more accounts than a freshly seeded stack plus
//! this fixture could hold. `scripts/wipe-parity.sh` sets both, on a stack it recreates first.
//!
//! # One database, erased twice
//!
//! Every table is copied into a `pdu_snap` schema after seeding. Go's socket erases everything
//! and the result is read; the copy is restored (and the files rewritten); our socket erases
//! everything and the result is read again. The two results are compared **row for row across
//! every table** but the ones the servers write in the background (`EXCLUDED`), with the time
//! columns compared as moved-or-not relative to the copy — see
//! `user_permanent_delete::normalized`. Ids need no mapping: it is the same data both times.
//!
//! The fixture is two `user_permanent_delete` subjects (every table, a bot each) on top of the
//! seeded stack, whose own `seed-bot` belongs to the administrator and sorts before them — so the
//! served path meets a bot owner whose bot is erased first, which Go does not cascade.
//!
//! Once Go has created `system-bot` it belongs to `sliceuser` and sorts **after** it, so Go's
//! erasure of `sliceuser` reaches the bot cascade and the request must be forwarded whole (see
//! `App::permanent_delete_all_needs_go`). The comparison re-owns every such bot to a
//! plugin-style id first, so there is a served path to compare; the last phase plants one on
//! purpose and asserts the forward.

use std::collections::BTreeMap;

use futures_util::FutureExt;

use crate::common;
use crate::parity::user_permanent_delete::{
    Subject, TABLES, drop_apparatus, normalized, plant, pool,
};

use common::{GO, go_minted_token, logged_in_user_id, stack_enabled};

/// Tables the Go server writes on its own schedule or at startup, and the snapshot itself.
const EXCLUDED: &[&str] = &[
    "systems",
    "jobs",
    "clusterdiscovery",
    "configurations",
    "configurationfiles",
    "db_migrations",
    "db_lock",
    "pluginkeyvaluestore",
];

/// The stack number this run may destroy, when `MMRS_WIPE_PARITY` names the stack the binary
/// targets; `None` (and the test does nothing) otherwise.
fn wipe_stack() -> Option<u16> {
    let wanted: u16 = std::env::var("MMRS_WIPE_PARITY").ok()?.parse().ok()?;
    let stack: u16 = std::env::var("MMRS_STACK").ok()?.parse().ok()?;
    if wanted != stack || stack == 0 {
        eprintln!("skipping: MMRS_WIPE_PARITY={wanted} does not name this stack ({stack})");
        return None;
    }
    let port = 8065 + 100 * stack;
    assert!(
        GO.ends_with(&format!(":{port}")),
        "this binary was built for {GO}, not stack {stack}; rebuild with its environment"
    );
    Some(stack)
}

async fn public_tables(pool: &sqlx::PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
          WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
          ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
    .expect("the table list")
    .into_iter()
    .filter(|t: &String| !EXCLUDED.contains(&t.as_str()))
    .collect()
}

async fn take_copy(pool: &sqlx::PgPool, tables: &[String]) {
    sqlx::query("DROP SCHEMA IF EXISTS pdu_snap CASCADE")
        .execute(pool)
        .await
        .expect("drop the old copy");
    sqlx::query("CREATE SCHEMA pdu_snap")
        .execute(pool)
        .await
        .expect("create the copy's schema");
    for table in tables {
        sqlx::query(&format!(
            "CREATE TABLE pdu_snap.{table} AS TABLE public.{table}"
        ))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("copying {table}: {e}"));
    }
}

/// Put every copied table back, in one transaction with triggers (and so foreign keys) off.
async fn restore_copy(pool: &sqlx::PgPool, tables: &[String]) {
    let mut tx = pool.begin().await.expect("a transaction");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .expect("triggers off");
    let all = tables
        .iter()
        .map(|t| format!("public.{t}"))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query(&format!("TRUNCATE {all}"))
        .execute(&mut *tx)
        .await
        .expect("truncate");
    for table in tables {
        sqlx::query(&format!(
            "INSERT INTO public.{table} SELECT * FROM pdu_snap.{table}"
        ))
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("restoring {table}: {e}"));
    }
    tx.commit().await.expect("restore commits");
}

type Rows = BTreeMap<String, Vec<String>>;

/// Every table, normalized against the copy, minus the apparatus rows (`drop_apparatus`).
async fn read_back(
    pool: &sqlx::PgPool,
    tables: &[String],
    before: &BTreeMap<String, BTreeMap<String, serde_json::Map<String, serde_json::Value>>>,
) -> Rows {
    let refs: Vec<&str> = tables.iter().map(String::as_str).collect();
    let after = user_permanent_delete_snapshot(pool, &refs).await;
    let mut rows = normalized(before, &after, &[]);
    drop_apparatus(&mut rows);
    rows
}

async fn user_permanent_delete_snapshot(
    pool: &sqlx::PgPool,
    tables: &[&str],
) -> BTreeMap<String, BTreeMap<String, serde_json::Map<String, serde_json::Value>>> {
    crate::parity::user_permanent_delete::snapshot(pool, tables, &[]).await
}

async fn wipe(socket: &std::path::Path) -> (u16, Vec<u8>, bool) {
    let (status, headers, body) =
        common::local_socket::over_socket(socket, "DELETE", "/api/v4/users").await;
    let served = headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, body, served)
}

fn files(subjects: &[Subject]) -> Vec<Vec<(String, bool)>> {
    subjects.iter().map(Subject::files_present).collect()
}

#[tokio::test]
async fn erasing_every_account_leaves_the_same_database_as_go() {
    if !stack_enabled() || wipe_stack().is_none() {
        return;
    }
    assert!(
        common::local_socket::sockets_enabled(),
        "a wipe run needs both local sockets"
    );
    let go_socket = common::local_socket::go_socket().expect("checked");
    let rust_socket = common::local_socket::rust_socket().expect("checked");

    let http = common::client();
    let admin = go_minted_token(&http).await;
    let admin_id = logged_in_user_id().to_owned();
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let pool = pool().await;

    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .expect("a count");
    assert!(
        accounts <= 20,
        "{accounts} accounts: this does not look like a freshly seeded stack, refusing to erase it"
    );

    let subjects = vec![
        plant(&admin_id, &team_id, "pduwa").await,
        plant(&admin_id, &team_id, "pduwb").await,
    ];
    // Any live bot that sorts at or after its owning account would make Go reach the bot cascade,
    // and the whole request Go's — `system-bot`, owned by `sliceuser`, once Go has created it. To
    // give the served path something to erase, such bots are handed to a plugin-style owner (an id
    // that is no account) for the comparison; the last phase builds that case on purpose.
    sqlx::query(
        "UPDATE bots SET ownerid = 'com.mattermost.pdu'
          WHERE userid IN (SELECT b.userid FROM bots b
                             JOIN users bu ON bu.id = b.userid
                             JOIN users o ON o.id = b.ownerid
                            WHERE b.deleteat = 0 AND bu.username >= o.username)",
    )
    .execute(&pool)
    .await
    .expect("late bots are re-owned");

    let tables = public_tables(&pool).await;
    for table in TABLES {
        assert!(tables.iter().any(|t| t == table), "{table} is compared");
    }
    take_copy(&pool, &tables).await;
    // From here the database is restored whatever happens, so the stack can be run again —
    // which is what lets `scripts/mutate-batch.sh` drive this suite at all.
    let outcome = std::panic::AssertUnwindSafe(async {
        let refs: Vec<&str> = tables.iter().map(String::as_str).collect();
        let before = user_permanent_delete_snapshot(&pool, &refs).await;

        // Go's erasure.
        let (status, body, _) = wipe(&go_socket).await;
        assert_eq!(status, 200, "Go: {}", String::from_utf8_lossy(&body));
        let go_body = body;
        let go_rows = read_back(&pool, &tables, &before).await;
        let go_files = files(&subjects);

        // Ours, from the same starting point.
        restore_copy(&pool, &tables).await;
        for subject in &subjects {
            subject.write_files();
        }
        let (status, body, served) = wipe(&rust_socket).await;
        assert!(served, "the erasure is served here");
        assert_eq!(status, 200, "Rust: {}", String::from_utf8_lossy(&body));
        assert_eq!(body, go_body);
        let rs_rows = read_back(&pool, &tables, &before).await;

        for (table, go) in &go_rows {
            let rs = rs_rows.get(table).cloned().unwrap_or_default();
            assert_eq!(
                go, &rs,
                "`{table}` differs after erasing everyone\n  go:   {go:#?}\n  rust: {rs:#?}"
            );
        }
        assert_eq!(
            go_files,
            files(&subjects),
            "the file store after erasing everyone"
        );
        assert!(go_rows["users"].is_empty(), "no account survives");
        assert!(go_rows["bots"].is_empty(), "nor any bot row");
        assert!(
            !go_rows["status"].is_empty(),
            "the deactivations left status rows, which PermanentDeleteUser does not remove"
        );

        // A bot that sorts after its owner: erasing the owner would reach the cascade, so the whole
        // request is Go's.
        restore_copy(&pool, &tables).await;
        sqlx::query("UPDATE bots SET ownerid = $1 WHERE userid = $2")
            .bind(format!("{:x<26}", "pduwauser"))
            .bind(format!("{:x<26}", "pduwbbot"))
            .execute(&pool)
            .await
            .expect("mmrspduwauser owns pduwbbot, which sorts after it");
        let (status, body, served) = wipe(&rust_socket).await;
        assert!(
            !served,
            "a cascade anywhere in the sequence forwards the whole request"
        );
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));

        // Not asserted: a bot that owns **itself**, which `permanent_delete_all_needs_go` also
        // forwards (`>=`, not `>`). Go never returns from that erasure — `disableUserBots` →
        // `UpdateBotActive` → `UpdateActive` → `userDeactivated` recurses on the same row (see
        // `mm_api::local_users::local_convert_user_to_bot`) — so the forward reproduces a hang,
        // measured at 22 minutes on 2026-09-19 before the recursion unwound.
    })
    .catch_unwind()
    .await;

    restore_copy(&pool, &tables).await;
    for subject in &subjects {
        subject.write_files();
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    let _ = sqlx::query("DROP SCHEMA IF EXISTS pdu_snap CASCADE")
        .execute(&pool)
        .await;
}
