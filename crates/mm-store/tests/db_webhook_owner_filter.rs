//! `SqlWebhookStore`'s **owner filter**, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_webhook_owner_filter
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! Both list routes narrow their query to one owner **unless** the caller holds
//! `manage_others_*_webhooks`. On a stock server the only roles granting `manage_own_*` are
//! `system_admin` and `team_admin`, and **both also grant `manage_others_*`** — so every caller
//! that can reach the route at all arrives with the filter already cleared, and no HTTP request
//! can exercise the narrowed branch. A mutation swapping the outgoing table's `CreatorId` for
//! another column survived the whole `api` suite for exactly that reason.
//!
//! The branch is not unreachable in principle — a custom scheme could grant one permission and not
//! the other — so it is tested where it *is* reachable: at the store, with the id passed directly.
//!
//! # And the two tables name the column differently
//!
//! `IncomingWebhooks.UserId` versus `OutgoingWebhooks.CreatorId` (webhook_store.go:188, :320).
//! A fixture whose creator is also the only user cannot tell them apart; these rows give each
//! table two owners.

use mm_store::{SqlWebhookStore, WebhookStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const OWNER: &str = "mmrswhowner0000000000owner";
const OTHER: &str = "mmrswhowner0000000000other";
const TEAM: &str = "mmrswhowner00000000000team";
const CHANNEL: &str = "mmrswhowner0000000000chann";

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

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM incomingwebhooks WHERE id LIKE 'mmrswhowner%'",
        "DELETE FROM outgoingwebhooks WHERE id LIKE 'mmrswhowner%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Two hooks in each table, one per owner, on the same team and channel.
async fn seed(pool: &PgPool) {
    for (id, owner) in [
        ("mmrswhowner00000000000in01", OWNER),
        ("mmrswhowner00000000000in02", OTHER),
    ] {
        sqlx::query(
            "INSERT INTO incomingwebhooks
                (id, createat, updateat, deleteat, userid, channelid, teamid,
                 displayname, description, username, iconurl, channellocked, lastused)
             VALUES ($1, 1788636490668, 1788636490669, 0, $2, $3, $4,
                     'mmrs owner filter', '', '', '', false, 0)",
        )
        .bind(id)
        .bind(owner)
        .bind(CHANNEL)
        .bind(TEAM)
        .execute(pool)
        .await
        .expect("inserts the incoming hook");
    }

    for (id, owner) in [
        ("mmrswhowner0000000000out01", OWNER),
        ("mmrswhowner0000000000out02", OTHER),
    ] {
        sqlx::query(
            "INSERT INTO outgoingwebhooks
                (id, token, createat, updateat, deleteat, creatorid, channelid, teamid,
                 triggerwords, triggerwhen, callbackurls, displayname, description,
                 contenttype, username, iconurl)
             VALUES ($1, 'mmrswhowner000000000token1', 1788636490668, 1788636490669, 0,
                     $2, $3, $4, '[]', 1, '[]', 'mmrs owner filter', '',
                     'application/json', '', '')",
        )
        .bind(id)
        .bind(owner)
        .bind(CHANNEL)
        .bind(TEAM)
        .execute(pool)
        .await
        .expect("inserts the outgoing hook");
    }
}

fn ids(hooks: &[impl HookId]) -> Vec<String> {
    hooks.iter().map(HookId::hook_id).collect()
}

trait HookId {
    fn hook_id(&self) -> String;
}
impl HookId for mm_model::incoming_webhook::IncomingWebhook {
    fn hook_id(&self) -> String {
        self.id.clone()
    }
}
impl HookId for mm_model::outgoing_webhook::OutgoingWebhook {
    fn hook_id(&self) -> String {
        self.id.clone()
    }
}

#[tokio::test]
async fn a_named_owner_narrows_each_list_to_that_owners_hooks() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlWebhookStore::new(pool.clone());

    // Incoming: `UserId`.
    let both = store
        .get_incoming_by_team_by_user(TEAM, "", 0, 60)
        .await
        .expect("the team query runs");
    assert_eq!(ids(&both).len(), 2, "an empty owner means every owner");

    let mine = store
        .get_incoming_by_team_by_user(TEAM, OWNER, 0, 60)
        .await
        .expect("the team query runs");
    assert_eq!(
        ids(&mine),
        vec!["mmrswhowner00000000000in01".to_owned()],
        "a named owner narrows the incoming list to `UserId`"
    );

    let global = store
        .get_incoming_list_by_user(OTHER, 0, 60)
        .await
        .expect("the list query runs");
    assert!(
        global.iter().all(|hook| hook.user_id == OTHER),
        "and the unscoped list narrows on the same column: {:?}",
        ids(&global)
    );

    // Outgoing: `CreatorId` — a different column with the same meaning.
    let both = store
        .get_outgoing_by_channel_by_user(CHANNEL, "", 0, 60)
        .await
        .expect("the channel query runs");
    assert_eq!(ids(&both).len(), 2);

    let mine = store
        .get_outgoing_by_channel_by_user(CHANNEL, OWNER, 0, 60)
        .await
        .expect("the channel query runs");
    assert_eq!(
        ids(&mine),
        vec!["mmrswhowner0000000000out01".to_owned()],
        "a named owner narrows the outgoing list to `CreatorId`"
    );

    let by_team = store
        .get_outgoing_by_team_by_user(TEAM, OTHER, 0, 60)
        .await
        .expect("the team query runs");
    assert_eq!(ids(&by_team), vec!["mmrswhowner0000000000out02".to_owned()]);

    let global = store
        .get_outgoing_list_by_user(OWNER, 0, 60)
        .await
        .expect("the list query runs");
    assert!(
        global.iter().all(|hook| hook.creator_id == OWNER),
        "and the unscoped outgoing list too: {:?}",
        ids(&global)
    );

    purge(&pool).await;
}
