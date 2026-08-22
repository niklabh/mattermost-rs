//! The three search queries behind `GET /api/v4/users/autocomplete`, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_user_search
//! ```
//!
//! The decisions here are all ones only a database can settle: which columns a term is matched
//! against, whether `AllowFullNames` moves two of them, which side of the anti-join the
//! `IS NULL` belongs on, whether the `%` a caller sends is a wildcard or a literal, and what an
//! empty team id means. The fixture is built so each has an **observable** consequence — every
//! searchable column holds a token that appears nowhere else, so a query reading the wrong one
//! returns a different set rather than the same one by luck.
//!
//! Every fixture row is prefixed `mmrsusrch`, and every search term below contains that prefix,
//! so three sibling worktrees creating users against the same database cannot change an answer.

use mm_store::user_store::{SqlUserStore, UserSearchOptions};
use mm_store::{StoreError, UserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The suites run concurrently within a binary; the fixture is shared and rebuilt once.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrsusrch00000000000team1";
const OTHER_TEAM: &str = "mmrsusrch00000000000team2";
const CHANNEL: &str = "mmrsusrch00000000000chan1";

/// In the team and in the channel. Its **username** carries the shared token.
const INCH: &str = "mmrsusrch000000000000usr1";
/// In the team, **not** in the channel.
const OUTCH: &str = "mmrsusrch000000000000usr2";
/// In the team and the channel but **deactivated** — `Users.DeleteAt != 0`.
const GONE: &str = "mmrsusrch000000000000usr3";
/// No team membership at all: only the system-wide arm can see it.
const LONER: &str = "mmrsusrch000000000000usr4";
/// In `OTHER_TEAM` only — the row that proves the team join is a filter and not decoration.
const OTHER: &str = "mmrsusrch000000000000usr5";
/// In the team, with a `TeamMembers.DeleteAt != 0` row: left the team.
const LEFT: &str = "mmrsusrch000000000000usr6";

/// Tokens that live in exactly one column each, so a query that reads the wrong column returns
/// a different set. All six users carry all five columns; only the token differs.
const TOKEN_USERNAME: &str = "mmrsusrchname";
const TOKEN_FIRST: &str = "mmrsusrchfirstonly";
const TOKEN_LAST: &str = "mmrsusrchlastonly";
const TOKEN_NICK: &str = "mmrsusrchnickonly";
/// The one that must never match: `AllowEmails` is hard-wired `false` on this route.
const TOKEN_EMAIL: &str = "mmrsusrchemailonly";

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

fn options(allow_full_names: bool, limit: i64) -> UserSearchOptions {
    UserSearchOptions {
        allow_full_names,
        limit,
    }
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM channelmembers WHERE userid LIKE 'mmrsusrch%'",
        "DELETE FROM teammembers WHERE userid LIKE 'mmrsusrch%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsusrch%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsusrch%'",
        "DELETE FROM users WHERE id LIKE 'mmrsusrch%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Every user gets all five searchable columns populated, with the *distinguishing* token in
/// whichever one this call names. Usernames are chosen so that alphabetical order differs from
/// id order: `zulu…` sorts last and has the lowest id.
#[allow(clippy::too_many_arguments)]
async fn insert_user(
    pool: &PgPool,
    id: &str,
    username: &str,
    first: &str,
    last: &str,
    nickname: &str,
    email: &str,
    delete_at: i64,
) {
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, password, authdata,
                            authservice, email, emailverified, nickname, firstname, lastname,
                            position, roles, allowmarketing, props, notifyprops,
                            lastpasswordupdate, lastpictureupdate, failedattempts, locale,
                            timezone, mfaactive, mfasecret, mfausedtimestamps, remoteid,
                            lastlogin)
         VALUES ($1, 1000, 2000, $2, $3, 'hash', NULL, '', $4, true, $5, $6, $7,
                 'pos', 'system_user', false, '{}'::jsonb, '{}'::jsonb, 1000, 0, 0, 'en',
                 '{}'::jsonb, false, '', 'null'::jsonb, '', 0)",
    )
    .bind(id)
    .bind(delete_at)
    .bind(username)
    .bind(email)
    .bind(nickname)
    .bind(first)
    .bind(last)
    .execute(pool)
    .await
    .expect("inserts the user");
}

async fn insert_team(pool: &PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type,
                            allowopeninvite)
         VALUES ($1, 0, 0, 0, 'mmrs search', $2, 'O', false)",
    )
    .bind(id)
    .bind(format!("mmrs-usrch-{id}"))
    .execute(pool)
    .await
    .expect("inserts the team");
}

async fn insert_channel(pool: &PgPool, id: &str, team: &str) {
    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                               name, header, purpose, lastpostat, totalmsgcount, extraupdateat,
                               creatorid, totalmsgcountroot, lastrootpostat)
         VALUES ($1, 0, 0, 0, $2, 'O', 'mmrs search', $3, '', '', 0, 0, 0, '', 0, 0)",
    )
    .bind(id)
    .bind(team)
    .bind(format!("mmrs-usrch-{id}"))
    .execute(pool)
    .await
    .expect("inserts the channel");
}

async fn add_to_team(pool: &PgPool, team: &str, user: &str, member_delete_at: i64) {
    sqlx::query(
        "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin,
                                  schemeguest, createat)
         VALUES ($1, $2, '', $3, true, false, false, 0)",
    )
    .bind(team)
    .bind(user)
    .bind(member_delete_at)
    .execute(pool)
    .await
    .expect("inserts the team membership");
}

async fn add_to_channel(pool: &PgPool, channel: &str, user: &str) {
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                     mentioncount, notifyprops, lastupdateat, schemeuser,
                                     schemeadmin, schemeguest, mentioncountroot, msgcountroot,
                                     urgentmentioncount)
         VALUES ($1, $2, '', 0, 0, 0, '{}'::jsonb, 0, true, false, false, 0, 0, 0)",
    )
    .bind(channel)
    .bind(user)
    .execute(pool)
    .await
    .expect("inserts the channel membership");
}

/// Usernames deliberately out of id order, so a query that lost `ORDER BY Username ASC` comes
/// back in primary-key order and the first assertion catches it.
async fn seed(pool: &PgPool) {
    purge(pool).await;
    insert_team(pool, TEAM).await;
    insert_team(pool, OTHER_TEAM).await;
    insert_channel(pool, CHANNEL, TEAM).await;

    // id ascending / username descending.
    insert_user(
        pool,
        INCH,
        &format!("zulu-{TOKEN_USERNAME}"),
        "Alpha",
        "Alphason",
        "alphanick",
        &format!("{INCH}@mmrs.invalid"),
        0,
    )
    .await;
    insert_user(
        pool,
        OUTCH,
        &format!("yankee-{TOKEN_USERNAME}"),
        "Bravo",
        "Bravoson",
        "bravonick",
        &format!("{OUTCH}@mmrs.invalid"),
        0,
    )
    .await;
    insert_user(
        pool,
        GONE,
        &format!("xray-{TOKEN_USERNAME}"),
        "Charlie",
        "Charlieson",
        "charlienick",
        &format!("{GONE}@mmrs.invalid"),
        1_700_000_000_000,
    )
    .await;
    // The three column-probe users. Their usernames do NOT contain the shared username token,
    // so only a query reading the named column finds them.
    insert_user(
        pool,
        LONER,
        "mmrsusrch-loner",
        TOKEN_FIRST,
        "Delton",
        "deltanick",
        &format!("{LONER}@mmrs.invalid"),
        0,
    )
    .await;
    insert_user(
        pool,
        OTHER,
        "mmrsusrch-other",
        "Echo",
        TOKEN_LAST,
        TOKEN_NICK,
        &format!("{TOKEN_EMAIL}@mmrs.invalid"),
        0,
    )
    .await;
    insert_user(
        pool,
        LEFT,
        &format!("whiskey-{TOKEN_USERNAME}"),
        "Foxtrot",
        "Foxtrotson",
        "foxtrotnick",
        &format!("{LEFT}@mmrs.invalid"),
        0,
    )
    .await;

    add_to_team(pool, TEAM, INCH, 0).await;
    add_to_team(pool, TEAM, OUTCH, 0).await;
    add_to_team(pool, TEAM, GONE, 0).await;
    add_to_team(pool, TEAM, OTHER, 0).await;
    add_to_team(pool, OTHER_TEAM, OTHER, 0).await;
    // Left the team: the row exists with a non-zero DeleteAt.
    add_to_team(pool, TEAM, LEFT, 1_700_000_000_000).await;

    add_to_channel(pool, CHANNEL, INCH).await;
    add_to_channel(pool, CHANNEL, GONE).await;
}

fn usernames(users: &[mm_model::user::User]) -> Vec<&str> {
    users.iter().map(|u| u.username.as_str()).collect()
}

/// `Search` with a team id: the `TeamMembers` join with `DeleteAt = 0`, plus the user's own
/// `DeleteAt = 0`. Four rows carry the username token and only two survive both predicates.
#[tokio::test]
async fn search_in_a_team_excludes_the_deactivated_and_the_departed() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .search(TEAM, TOKEN_USERNAME, &options(true, 100))
        .await
        .expect("the search runs");

    // `xray-…` is deactivated (Users.DeleteAt), `whiskey-…` left the team
    // (TeamMembers.DeleteAt). Alphabetical, not id order — `zulu` has the lowest id.
    assert_eq!(
        usernames(&users),
        [
            format!("yankee-{TOKEN_USERNAME}"),
            format!("zulu-{TOKEN_USERNAME}")
        ]
    );

    purge(&pool).await;
}

/// An **empty** team id is the system-wide arm: no join at all, so the user who left the team
/// comes back — while the deactivated one still does not, because that predicate is on `Users`.
#[tokio::test]
async fn an_empty_team_id_searches_every_active_user() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .search("", TOKEN_USERNAME, &options(true, 100))
        .await
        .expect("the search runs");

    assert_eq!(
        usernames(&users),
        [
            format!("whiskey-{TOKEN_USERNAME}"),
            format!("yankee-{TOKEN_USERNAME}"),
            format!("zulu-{TOKEN_USERNAME}")
        ],
        "no team filter, but `Users.DeleteAt = 0` still applies"
    );

    purge(&pool).await;
}

/// The four searchable columns, one probe each, and the fifth that must never match.
///
/// This is the "Never autocomplete on emails" assertion (api4/user.go:1399): `AllowEmails` is
/// hard-wired false, so `Email` is not in `UserSearchTypeNames` and a term that exists only in
/// an address finds nobody — even though that same user's `email` field is on the wire for any
/// search that *does* match them.
#[tokio::test]
async fn the_searchable_columns_are_username_nickname_and_the_two_names_but_never_email() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let found = async |term: &str| {
        usernames(
            &store
                .search("", term, &options(true, 100))
                .await
                .expect("the search runs"),
        )
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>()
    };

    assert_eq!(found(TOKEN_FIRST).await, ["mmrsusrch-loner"], "FirstName");
    assert_eq!(found(TOKEN_LAST).await, ["mmrsusrch-other"], "LastName");
    assert_eq!(found(TOKEN_NICK).await, ["mmrsusrch-other"], "Nickname");
    assert!(
        found(TOKEN_EMAIL).await.is_empty(),
        "Email is not a searchable column on this route"
    );

    // And the user whose address holds the token is findable by every other column, so the
    // empty result above is about the column and not about the row.
    assert_eq!(found("mmrsusrch-other").await, ["mmrsusrch-other"]);

    purge(&pool).await;
}

/// `AllowFullNames = false` drops `FirstName` and `LastName` from the search — and only those
/// two. The same fixture, the same terms, a different flag.
#[tokio::test]
async fn allow_full_names_moves_exactly_two_columns() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let found = async |term: &str, allow: bool| {
        store
            .search("", term, &options(allow, 100))
            .await
            .expect("the search runs")
            .len()
    };

    assert_eq!(found(TOKEN_FIRST, true).await, 1);
    assert_eq!(found(TOKEN_FIRST, false).await, 0, "FirstName goes away");
    assert_eq!(found(TOKEN_LAST, true).await, 1);
    assert_eq!(found(TOKEN_LAST, false).await, 0, "LastName goes away");

    // Nickname and Username are in both lists.
    assert_eq!(found(TOKEN_NICK, false).await, 1, "Nickname stays");
    assert_eq!(found("mmrsusrch-loner", false).await, 1, "Username stays");

    purge(&pool).await;
}

/// Every whitespace-separated field is its own `AND` clause, so multiple terms **narrow** the
/// result. Writing the loop as an `OR` would widen it instead, and a single-term suite could
/// never tell.
#[tokio::test]
async fn multiple_terms_are_anded_across_the_columns() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let found = async |term: &str| {
        store
            .search("", term, &options(true, 100))
            .await
            .expect("the search runs")
            .len()
    };

    // `mmrsusrchname` matches four; adding `zulu` leaves one. Each clause may be satisfied by a
    // *different* column, which is why "Alpha" (a FirstName) narrows a Username match.
    assert_eq!(found(TOKEN_USERNAME).await, 3, "three active users match");
    assert_eq!(found(&format!("{TOKEN_USERNAME} zulu")).await, 1);
    assert_eq!(found(&format!("{TOKEN_USERNAME} Alpha")).await, 1);
    // A term matching nobody kills the whole result, which an OR could not do.
    assert_eq!(found(&format!("{TOKEN_USERNAME} nosuchtoken")).await, 0);

    purge(&pool).await;
}

/// A leading `@` is trimmed — all of them — so the mention box's own text works verbatim.
#[tokio::test]
async fn a_leading_at_sign_is_trimmed_off_each_term() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let found = async |term: &str| {
        store
            .search("", term, &options(true, 100))
            .await
            .expect("the search runs")
            .len()
    };

    assert_eq!(found(&format!("@{TOKEN_USERNAME}")).await, 3);
    assert_eq!(found(&format!("@@{TOKEN_USERNAME}")).await, 3);
    // Trailing at-signs are not trimmed, so this matches nobody.
    assert_eq!(found(&format!("{TOKEN_USERNAME}@")).await, 0);

    purge(&pool).await;
}

/// `sanitizeSearchTerm` escapes `%` and `_` and the query declares `ESCAPE '*'`, so a caller
/// cannot smuggle a wildcard in. Dropping the escaping would make the first assertion return
/// every user in the database.
#[tokio::test]
async fn a_caller_supplied_wildcard_is_a_literal() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let found = async |term: &str| {
        store
            .search("", term, &options(true, 100))
            .await
            .expect("the search runs")
            .len()
    };

    // `%` as a literal appears in no username, so this finds nothing at all — where an
    // unescaped `%` would match every row in the shared table.
    assert_eq!(found("mmrsusrch%").await, 0, "the per cent is literal");
    // `_` likewise: `mmrsusrch_loner` would match `mmrsusrch-loner` if `_` were a wildcard.
    assert_eq!(
        found("mmrsusrch_loner").await,
        0,
        "the underscore is literal"
    );
    assert_eq!(found("mmrsusrch-loner").await, 1, "the real hyphen matches");

    purge(&pool).await;
}

/// An id is a search term of its own — `Id = ?` sits beside the `LIKE`s in every clause.
#[tokio::test]
async fn an_exact_id_matches_even_though_it_is_no_ones_name() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .search("", LONER, &options(true, 100))
        .await
        .expect("the search runs");
    assert_eq!(usernames(&users), ["mmrsusrch-loner"]);

    purge(&pool).await;
}

/// `LIMIT` is applied after the ordering, so a limit of one returns the alphabetically first
/// match and not an arbitrary one. **Zero** is a real limit and returns nothing, and a negative
/// one is a failed query — the two halves of the api layer's one-sided clamp.
#[tokio::test]
async fn the_limit_is_applied_after_the_ordering_and_zero_means_zero() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .search("", TOKEN_USERNAME, &options(true, 1))
        .await
        .expect("the search runs");
    assert_eq!(
        usernames(&users),
        [format!("whiskey-{TOKEN_USERNAME}")],
        "the first by username, not the first by id"
    );

    let none = store
        .search("", TOKEN_USERNAME, &options(true, 0))
        .await
        .expect("the search runs");
    assert!(none.is_empty(), "LIMIT 0 is an empty list, not no limit");

    let err = store
        .search("", TOKEN_USERNAME, &options(true, -1))
        .await
        .expect_err("Postgres refuses a negative LIMIT");
    assert!(matches!(err, StoreError::Db { .. }));

    purge(&pool).await;
}

/// A blank term drops the search predicate entirely and returns the whole joined set. The
/// deactivated member is still excluded, so the `DeleteAt` predicate is not part of what the
/// blank-term branch skips.
#[tokio::test]
async fn a_blank_term_returns_the_whole_joined_set() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let in_channel = store
        .search_in_channel(CHANNEL, "   ", &options(true, 100))
        .await
        .expect("the search runs");
    assert_eq!(
        usernames(&in_channel),
        [format!("zulu-{TOKEN_USERNAME}")],
        "both members, minus the deactivated one"
    );

    purge(&pool).await;
}

/// `SearchInChannel` joins `ChannelMembers` and takes **no team id**, so a channel member who
/// has left the team is still listed. `SearchNotInChannel` is its anti-join and *does* take one.
#[tokio::test]
async fn the_channel_pair_splits_the_team_between_them() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let in_channel = store
        .search_in_channel(CHANNEL, TOKEN_USERNAME, &options(true, 100))
        .await
        .expect("the search runs");
    assert_eq!(
        usernames(&in_channel),
        [format!("zulu-{TOKEN_USERNAME}")],
        "`xray-…` is in the channel but deactivated"
    );

    let out_of_channel = store
        .search_not_in_channel(TEAM, CHANNEL, TOKEN_USERNAME, &options(true, 100))
        .await
        .expect("the search runs");
    assert_eq!(
        usernames(&out_of_channel),
        [format!("yankee-{TOKEN_USERNAME}")],
        "in the team, not in the channel — and `whiskey-…` left the team"
    );

    // The two lists are disjoint and neither contains a row the other should have had.
    assert!(
        in_channel
            .iter()
            .all(|u| !out_of_channel.iter().any(|o| o.id == u.id)),
        "a user cannot be both in and out of the same channel"
    );

    purge(&pool).await;
}

/// The anti-join's team filter is a real filter: a user in another team entirely is not
/// "out of channel" for this one.
#[tokio::test]
async fn the_out_of_channel_half_is_scoped_to_the_team() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    // `mmrsusrch-other` belongs to both teams, so it is out-of-channel for TEAM…
    let mine = store
        .search_not_in_channel(TEAM, CHANNEL, "mmrsusrch-other", &options(true, 100))
        .await
        .expect("the search runs");
    assert_eq!(usernames(&mine), ["mmrsusrch-other"]);

    // …but `mmrsusrch-loner`, who is in no team, is out-of-channel for none of them, while the
    // system-wide search finds it.
    let loner_scoped = store
        .search_not_in_channel(TEAM, CHANNEL, "mmrsusrch-loner", &options(true, 100))
        .await
        .expect("the search runs");
    assert!(
        loner_scoped.is_empty(),
        "no team membership, so not available to invite"
    );
    let loner_global = store
        .search("", "mmrsusrch-loner", &options(true, 100))
        .await
        .expect("the search runs");
    assert_eq!(usernames(&loner_global), ["mmrsusrch-loner"]);

    purge(&pool).await;
}
