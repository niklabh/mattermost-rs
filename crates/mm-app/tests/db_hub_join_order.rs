//! When a websocket connection's channel memberships are read ([D-1032]), against a real Postgres.
//!
//! ```sh
//! MM_STORE_DB=1 cargo test -p mm-app --test db_hub_join_order
//! ```
//!
//! # Why this is here and not only in the parity suite
//!
//! Over the stack, Go's answer on the interesting path is a coin toss (see
//! `App::should_send_event`), so the parity tranche can only tolerate one frame there. This pins
//! the rule itself — the first channel-scoped decision loads the memberships and nothing but an
//! invalidation reloads them — through `App::publish` in the order `JoinDefaultChannels`
//! publishes.
//!
//! Every test is named `hub_join_order_*` so `MUTATE_FILTER` can select them by name.

use std::sync::Arc;

use mm_app::App;
use mm_app::hub::{OutgoingFrame, WebConn};
use mm_model::session::Session;
use mm_model::utils::get_millis;
use mm_model::websocket_message::WebSocketEvent;
use mm_store::{SessionStore, SqlStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::mpsc;

const USER: &str = "mmrshjouseraaaaaaaaaaaaaaa";
const TEAM: &str = "mmrshjoteamaaaaaaaaaaaaaaa";
/// The first default channel, joined before the connection's first channel-scoped decision.
const TOWN: &str = "mmrshjotownaaaaaaaaaaaaaaa";
/// The second, joined after it.
const OFF: &str = "mmrshjooffaaaaaaaaaaaaaaaa";

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

/// Purges at the **start**, because a failing assertion panics past any trailing cleanup.
async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM channelmembers WHERE userid = 'mmrshjouseraaaaaaaaaaaaaaa'",
        "DELETE FROM sessions WHERE userid = 'mmrshjouseraaaaaaaaaaaaaaa'",
        "DELETE FROM channels WHERE teamid = 'mmrshjoteamaaaaaaaaaaaaaaa'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover fixtures");
    }
}

async fn channel(pool: &PgPool, id: &str, name: &str) {
    sqlx::query(
        r#"
        INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name,
                              header, purpose, lastpostat, totalmsgcount, extraupdateat, creatorid,
                              schemeid, groupconstrained, shared, totalmsgcountroot,
                              lastrootpostat, defaultcategoryname, autotranslation, discoverable)
        VALUES ($1, $2, $2, 0, $3, 'O', $4, $4, '', '', 0, 0, 0, '',
                NULL, NULL, NULL, 0, 0, '', false, false)
        "#,
    )
    .bind(id)
    .bind(get_millis())
    .bind(TEAM)
    .bind(name)
    .execute(pool)
    .await
    .expect("the channel");
}

/// `SaveMember`'s row, as far as the membership read needs it.
async fn join(pool: &PgPool, channel_id: &str) {
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser,
                                     schemeadmin, schemeguest, lastviewedat, msgcount,
                                     mentioncount, mentioncountroot, msgcountroot,
                                     urgentmentioncount, lastupdateat)
         VALUES ($1, $2, '', '{}'::jsonb, true, false, false, 0, 0, 0, 0, 0, 0, $3)",
    )
    .bind(channel_id)
    .bind(USER)
    .bind(get_millis())
    .execute(pool)
    .await
    .expect("the membership");
}

/// A registered connection with a stored session, which an invalidation makes it read again.
async fn connect(app: &App) -> (Arc<WebConn>, mpsc::Receiver<OutgoingFrame>) {
    let session = app
        .store()
        .session()
        .save(Session {
            user_id: USER.to_owned(),
            expires_at: get_millis() + 3_600_000,
            roles: "system_user".to_owned(),
            ..Session::default()
        })
        .await
        .expect("the session");
    let (conn, rx) = WebConn::new("mmrshjoconnaaaaaaaaaaaaaaa".to_owned(), session, false);
    let hello = app.hello_message(&conn);
    app.hub().register(Arc::clone(&conn), hello);
    (conn, rx)
}

/// `JoinDefaultChannels`' two events for one channel: the join post, then `user_added`.
fn join_events(channel_id: &str) -> [WebSocketEvent; 2] {
    let event = |kind: &str| {
        let mut event = WebSocketEvent::new(kind, "", channel_id, "", None, "");
        event.add("user_id", serde_json::Value::String(USER.to_owned()));
        event
    };
    [event("posted"), event("user_added")]
}

/// What the connection was sent since the last call, as `event@channel`.
fn heard(rx: &mut mpsc::Receiver<OutgoingFrame>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        if let OutgoingFrame::Event { event, .. } = frame {
            let channel = event
                .get_broadcast()
                .map(|b| b.channel_id.clone())
                .unwrap_or_default();
            let name = match channel.as_str() {
                TOWN => "town",
                OFF => "off",
                _ => "",
            };
            out.push(format!("{}@{name}", event.event_type()));
        }
    }
    out
}

/// The joiner's own connection, in `JoinDefaultChannels`' order: town-square saved, its two
/// events, off-topic saved, its two events, then `JoinUserToTeam`'s invalidation. The first
/// decision loads a cache holding town-square only, so off-topic's events go unheard — Go's
/// answer whenever its hub keeps up — and the next off-topic event after the invalidation is
/// heard.
#[tokio::test]
async fn hub_join_order_hears_what_the_first_load_saw() {
    if !enabled() {
        return;
    }
    let pool = pool().await;
    purge(&pool).await;
    channel(&pool, TOWN, "mmrs-hjo-town").await;
    channel(&pool, OFF, "mmrs-hjo-off").await;

    let app = App::new(SqlStore::from_pool(pool.clone()));
    let (_conn, mut rx) = connect(&app).await;
    assert_eq!(heard(&mut rx), ["hello@"]);

    join(&pool, TOWN).await;
    for event in join_events(TOWN) {
        app.publish(event).await;
    }
    assert_eq!(heard(&mut rx), ["posted@town", "user_added@town"]);

    join(&pool, OFF).await;
    for event in join_events(OFF) {
        app.publish(event).await;
    }
    assert_eq!(
        heard(&mut rx),
        Vec::<String>::new(),
        "off-topic was saved after the cache was loaded"
    );

    // What `JoinUserToTeam` does after the loop (team.go:872).
    app.clear_session_cache_for_user(USER).await;
    let [posted, _] = join_events(OFF);
    app.publish(posted).await;
    assert_eq!(heard(&mut rx), ["posted@off"]);

    purge(&pool).await;
}
