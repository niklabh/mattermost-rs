//! `SqlPostStore`'s two `PostReminders` methods against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_post_reminder_store
//! ```
//!
//! # Why these need a suite of their own
//!
//! `POST /api/v4/users/{user_id}/posts/{post_id}/reminder` is **forwarded** ([D-420]): its
//! ephemeral confirmation always carries a permalink, and the permalink-embed path is not ported.
//! So the two store methods behind it have no route, and without this file nothing but the
//! compiler and the offline schema check would ever look at them — which is precisely the shape
//! of unreachable code this project has too much of already.
//!
//! Everything here is asserted against our own implementation with the Go source
//! (post_store.go:3278, :3341) as the reference. There is no cross-server oracle, because there is
//! no route in front of these on our side to compare.
//!
//! # The three decisions worth a fixture
//!
//! 1. **The insert is an upsert.** `ON CONFLICT (postid, userid) DO UPDATE SET TargetTime` — a
//!    second reminder on the same post **moves** the first. A plain `INSERT` would 500 instead,
//!    and every "set a reminder" test that only ever sets one would pass.
//! 2. **A missing post is a not-found, and it writes nothing.** The `SELECT EXISTS` and the insert
//!    share a transaction, so the failure path must leave `PostReminders` untouched. A port that
//!    checked after inserting, or outside the transaction, is caught by the row count.
//! 3. **`COALESCE(t.name, '')` is load-bearing.** A DM has `Channels.TeamId = ''`, which matches
//!    no team, so the `LEFT JOIN` yields SQL `NULL` and the caller's permalink branch reads the
//!    emptiness. Without the `COALESCE` this is not a different string, it is a scan failure.
//!
//! # The rows are `mmrsreminder%` and are deleted by the same test that writes them
//!
//! `users` is the one table here that other suites read in aggregate, so every column Go scans
//! into a non-pointer type is filled — the same normalisation `purge_api_fixtures` performs — and
//! the row exists only for the body of one test, under [`FIXTURES`].

use mm_store::post_store::{PostStore, SqlPostStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// One set of rows, purged and re-seeded per test; two running interleaved would delete each
/// other's fixtures mid-assertion.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrsreminder0000000team001";
const TEAM_CHANNEL: &str = "mmrsreminder0000000chan001";
/// A DM-shaped channel: `TeamId = ''`, which is what makes the `COALESCE` fire.
const DM_CHANNEL: &str = "mmrsreminder0000000chan002";
const AUTHOR: &str = "mmrsreminder0000000user001";
/// The channel's creator, who is **not** the post's author — so a query that walked
/// `Channels.CreatorId` instead of `Posts.UserId` answers the wrong username rather than none.
const CREATOR: &str = "mmrsreminder0000000user002";
const TEAM_POST: &str = "mmrsreminder0000000post001";
const DM_POST: &str = "mmrsreminder0000000post002";
const ABSENT_POST: &str = "mmrsreminder0000000post999";
const REMINDING_USER: &str = "mmrsreminder0000000user003";
const OTHER_REMINDING_USER: &str = "mmrsreminder0000000user004";

const AUTHOR_NAME: &str = "mmrsreminderauthor";
const CREATOR_NAME: &str = "mmrsremindercreator";
const TEAM_NAME: &str = "mmrsreminderteam";

fn enabled() -> Option<String> {
    if std::env::var("MM_STORE_DB").ok().as_deref() != Some("1") {
        return None;
    }
    std::env::var("DATABASE_URL").ok()
}

async fn pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        // Capped, so a suite run without a database fails in seconds rather than sitting on
        // sqlx's 30-second default.
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await
        .expect("the development database is reachable")
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM postreminders WHERE postid LIKE 'mmrsreminder%' OR userid LIKE 'mmrsreminder%'",
        "DELETE FROM posts WHERE id LIKE 'mmrsreminder%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsreminder%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsreminder%'",
        "DELETE FROM users WHERE id LIKE 'mmrsreminder%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("the purge runs");
    }
}

/// A team, two channels (one in it, one DM-shaped), two users and two posts.
async fn seed(pool: &PgPool) {
    purge(pool).await;

    for (id, username) in [(AUTHOR, AUTHOR_NAME), (CREATOR, CREATOR_NAME)] {
        // Every column Go scans into a non-pointer type is filled. A NULL in any of them makes
        // *Go's* profile reads 500 while the row exists, which would surface in a suite that has
        // nothing to do with reminders.
        sqlx::query(
            "INSERT INTO users (id, createat, updateat, deleteat, username, password, authdata,
                                authservice, email, emailverified, nickname, firstname, lastname,
                                position, roles, allowmarketing, props, notifyprops,
                                lastpasswordupdate, lastpictureupdate, failedattempts, locale,
                                mfaactive, mfasecret, mfausedtimestamps, remoteid)
             VALUES ($1, 1, 1, 0, $2, '', NULL, '', $3, true, '', '', '', '', 'system_user',
                     false, '{}', '{}', 1, 0, 0, 'en', false, '', '{}', NULL)",
        )
        .bind(id)
        .bind(username)
        .bind(format!("{username}@mmrs.invalid"))
        .execute(pool)
        .await
        .expect("the user is planted");
    }

    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                            email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                            schemeid, groupconstrained, cloudlimitsarchived)
         VALUES ($1, 1, 1, 0, 'MMRS Reminder', $2, '', '', 'O', '', '', 'mmrsreminderinvite00000001',
                 false, NULL, NULL, false)",
    )
    .bind(TEAM)
    .bind(TEAM_NAME)
    .execute(pool)
    .await
    .expect("the team is planted");

    for (id, team_id, channel_type) in [(TEAM_CHANNEL, TEAM, "O"), (DM_CHANNEL, "", "D")] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, header, purpose, lastpostat, totalmsgcount, extraupdateat,
                                   creatorid, schemeid, groupconstrained, shared,
                                   totalmsgcountroot, lastrootpostat, defaultcategoryname,
                                   autotranslation, discoverable)
             VALUES ($1, 1, 1, 0, $2, $3::channel_type, 'MMRS Reminder', $1, '', '', 1, 0, 0, $4, NULL, NULL,
                     NULL, 0, 1, '', false, false)",
        )
        .bind(id)
        .bind(team_id)
        .bind(channel_type)
        .bind(CREATOR)
        .execute(pool)
        .await
        .expect("the channel is planted");
    }

    for (id, channel) in [(TEAM_POST, TEAM_CHANNEL), (DM_POST, DM_CHANNEL)] {
        sqlx::query(
            "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid,
                                channelid, rootid, originalid, message, type, props, hashtags,
                                filenames, fileids, hasreactions, remoteid)
             VALUES ($1, 1, 1, 0, 0, false, $2, $3, '', '', 'mmrs reminder body', '', '{}', '',
                     NULL, NULL, false, NULL)",
        )
        .bind(id)
        .bind(AUTHOR)
        .bind(channel)
        .execute(pool)
        .await
        .expect("the post is planted");
    }
}

async fn reminder_rows(pool: &PgPool, post_id: &str) -> Vec<(String, i64)> {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT userid, targettime FROM postreminders WHERE postid = $1 ORDER BY userid",
    )
    .bind(post_id)
    .fetch_all(pool)
    .await
    .expect("the reminders read back")
}

/// A reminder is written verbatim, and a **second** one for the same pair moves the first rather
/// than adding a row or failing.
///
/// The two target times are far apart and both are plausible Unix **second** values — the column
/// holds seconds, not milliseconds (app/post.go:2866 formats it with `time.Unix(t, 0)`), and a
/// port that multiplied by 1000 somewhere would still round-trip if the test used 0.
#[tokio::test]
async fn post_reminder_store_upserts_rather_than_duplicating() {
    let Some(url) = enabled() else { return };
    let _guard = FIXTURES.lock().await;
    let pool = pool(&url).await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    const FIRST: i64 = 1_900_000_000;
    const SECOND: i64 = 2_000_000_000;

    store
        .set_post_reminder(TEAM_POST, REMINDING_USER, FIRST)
        .await
        .expect("the first reminder is written");
    assert_eq!(
        reminder_rows(&pool, TEAM_POST).await,
        vec![(REMINDING_USER.to_owned(), FIRST)]
    );

    store
        .set_post_reminder(TEAM_POST, REMINDING_USER, SECOND)
        .await
        .expect("the second reminder replaces the first");
    assert_eq!(
        reminder_rows(&pool, TEAM_POST).await,
        vec![(REMINDING_USER.to_owned(), SECOND)],
        "the conflict target is (postid, userid), so one user has at most one reminder per post"
    );

    // A **different** user on the same post is a second row, not a replacement — which is what
    // makes the composite key a composite key.
    store
        .set_post_reminder(TEAM_POST, OTHER_REMINDING_USER, FIRST)
        .await
        .expect("another user's reminder is written");
    assert_eq!(
        reminder_rows(&pool, TEAM_POST).await,
        // `ORDER BY userid` ascending: `…user003` before `…user004`.
        vec![
            (REMINDING_USER.to_owned(), SECOND),
            (OTHER_REMINDING_USER.to_owned(), FIRST),
        ]
    );

    purge(&pool).await;
}

/// A post that does not exist is `ErrNotFound` **and writes nothing**.
///
/// `PostReminders` has no foreign key to `Posts`, so the `SELECT EXISTS` is the only thing
/// standing between a client and a reminder filed against an id that names nothing. Asserting the
/// error alone would pass a port that inserted first and checked afterwards.
#[tokio::test]
async fn post_reminder_store_refuses_a_post_that_does_not_exist_without_writing() {
    let Some(url) = enabled() else { return };
    let _guard = FIXTURES.lock().await;
    let pool = pool(&url).await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    let err = store
        .set_post_reminder(ABSENT_POST, REMINDING_USER, 1_900_000_000)
        .await
        .expect_err("a reminder on a missing post is refused");
    assert!(
        err.is_not_found(),
        "Go raises store.NewErrNotFound(\"Post\", …) here: {err}"
    );
    assert!(
        reminder_rows(&pool, ABSENT_POST).await.is_empty(),
        "the transaction must roll back, leaving no row behind"
    );

    purge(&pool).await;
}

/// The metadata read: the author's username, the channel, and the `COALESCE` that a DM needs.
///
/// The channel's `CreatorId` is a **different** user from the post's author, so a query that
/// joined `Channels.CreatorId` — which is the join a reader reaches for when the sentence says
/// "reminded … by @somebody" — answers `mmrsremindercreator` rather than failing.
#[tokio::test]
async fn post_reminder_metadata_names_the_author_and_coalesces_a_missing_team() {
    let Some(url) = enabled() else { return };
    let _guard = FIXTURES.lock().await;
    let pool = pool(&url).await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    let in_team = store
        .get_post_reminder_metadata(TEAM_POST)
        .await
        .expect("the metadata reads");
    assert_eq!(in_team.channel_id, TEAM_CHANNEL);
    assert_eq!(
        in_team.team_name, TEAM_NAME,
        "a channel in a team carries the team's `Name`, not its `DisplayName` — the permalink is \
         built from it"
    );
    assert_eq!(
        in_team.username, AUTHOR_NAME,
        "the username is the post author's, not the channel creator's"
    );
    assert_ne!(in_team.username, CREATOR_NAME);
    assert_eq!(in_team.user_locale, "en");

    let in_dm = store
        .get_post_reminder_metadata(DM_POST)
        .await
        .expect("the metadata reads for a DM too");
    assert_eq!(in_dm.channel_id, DM_CHANNEL);
    assert_eq!(
        in_dm.team_name, "",
        "a DM has TeamId = '', so the LEFT JOIN yields NULL and COALESCE makes it empty — this is \
         the value the caller branches on to build a `/pl/{{id}}` permalink instead of a \
         `/{{team}}/pl/{{id}}` one"
    );
    assert_eq!(in_dm.username, AUTHOR_NAME);

    purge(&pool).await;
}

/// A post that does not exist has no metadata, and the failure is an ordinary error.
///
/// Go's `GetReplica().Get` turns zero rows into `sql.ErrNoRows`, wraps it, and returns it — there
/// is **no** not-found branch on this read, and the app layer answers the same 500 it answers for
/// a broken connection. Pinned so that "improving" it into a 404 fails here.
#[tokio::test]
async fn post_reminder_metadata_has_no_not_found_branch() {
    let Some(url) = enabled() else { return };
    let _guard = FIXTURES.lock().await;
    let pool = pool(&url).await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    let err = store
        .get_post_reminder_metadata(ABSENT_POST)
        .await
        .expect_err("a missing post has no metadata");
    assert!(
        !err.is_not_found(),
        "Go wraps sql.ErrNoRows as an ordinary error here, not as store.ErrNotFound: {err}"
    );

    purge(&pool).await;
}
